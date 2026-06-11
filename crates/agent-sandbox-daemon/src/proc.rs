//! Process and PTY execution for the guest daemon.
//!
//! Every exec runs in its own session (`setsid`) so the daemon can kill the
//! whole process tree by signalling the process group — not just the immediate
//! child. Output is streamed as raw byte chunks over the protocol; the host
//! client reassembles lines to preserve the existing `GuestProcess` contract.

use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::time::Duration;

use agent_sandbox_protocol::{Message, T_EXIT, T_STARTED, T_STDERR, T_STDOUT};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

/// Control messages routed from the connection read loop to a running process.
#[derive(Debug)]
pub enum ProcControl {
    Stdin(Vec<u8>),
    StdinClose,
    Resize { rows: u16, cols: u16 },
    Cancel,
}

/// Parsed `exec` request.
pub struct ExecSpec {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<String>,
    pub pty: bool,
    pub rows: u16,
    pub cols: u16,
}

const READ_CHUNK: usize = 16 * 1024;

/// Run a non-PTY process, streaming stdout/stderr and the final exit status to
/// `outbox`. Consumes `control` for stdin/cancel. Returns when the child exits.
pub async fn run_plain(
    id: u64,
    spec: ExecSpec,
    outbox: mpsc::Sender<Message>,
    mut control: mpsc::Receiver<ProcControl>,
) {
    let mut command = match build_command(&spec) {
        Ok(c) => c,
        Err(err) => {
            let _ = outbox.send(error_exit(id, &err)).await;
            return;
        }
    };
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(err) => {
            let _ = outbox.send(error_exit(id, &format!("spawn failed: {err}"))).await;
            return;
        }
    };
    let pid = child.id().map(|p| p as i32).unwrap_or(-1);
    let _ = outbox.send(Message::new(T_STARTED, id).with_i64("pid", pid as i64)).await;

    let mut stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_task = stdout.map(|r| tokio::spawn(stream_reader(id, r, outbox.clone(), T_STDOUT)));
    let stderr_task = stderr.map(|r| tokio::spawn(stream_reader(id, r, outbox.clone(), T_STDERR)));

    let status = loop {
        tokio::select! {
            msg = control.recv() => match msg {
                Some(ProcControl::Stdin(bytes)) => {
                    if let Some(w) = stdin.as_mut() {
                        use tokio::io::AsyncWriteExt;
                        let _ = w.write_all(&bytes).await;
                        let _ = w.flush().await;
                    }
                }
                Some(ProcControl::StdinClose) => { stdin = None; }
                Some(ProcControl::Resize { .. }) => {}
                Some(ProcControl::Cancel) => signal_group(pid),
                None => {}
            },
            res = child.wait() => break res,
        }
    };

    if let Some(t) = stdout_task { let _ = t.await; }
    if let Some(t) = stderr_task { let _ = t.await; }
    let _ = outbox.send(exit_message(id, status)).await;
}

/// Run a process attached to a freshly allocated PTY. stdout carries the merged
/// terminal output; `Resize` adjusts the window size live.
pub async fn run_pty(
    id: u64,
    spec: ExecSpec,
    outbox: mpsc::Sender<Message>,
    mut control: mpsc::Receiver<ProcControl>,
) {
    let (master, slave) = match open_pty(spec.rows, spec.cols) {
        Ok(pair) => pair,
        Err(err) => {
            let _ = outbox.send(error_exit(id, &format!("openpty failed: {err}"))).await;
            return;
        }
    };

    let mut command = match build_command(&spec) {
        Ok(c) => c,
        Err(err) => {
            unsafe { libc::close(master); libc::close(slave); }
            let _ = outbox.send(error_exit(id, &err)).await;
            return;
        }
    };
    // Wire the slave as the child's controlling terminal. Each Stdio consumes an
    // fd, so dup the slave three times; the originals are closed below.
    let (s_in, s_out, s_err) = unsafe {
        (dup_stdio(slave), dup_stdio(slave), dup_stdio(slave))
    };
    match (s_in, s_out, s_err) {
        (Some(i), Some(o), Some(e)) => {
            command.stdin(i);
            command.stdout(o);
            command.stderr(e);
        }
        _ => {
            unsafe { libc::close(master); libc::close(slave); }
            let _ = outbox.send(error_exit(id, "failed to dup pty slave")).await;
            return;
        }
    }
    unsafe {
        command.pre_exec(|| {
            // New session, then make fd 0 (the slave) the controlling tty.
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    let spawn = command.spawn();
    // Parent no longer needs the slave once the child holds its dups.
    unsafe { libc::close(slave); }
    let mut child = match spawn {
        Ok(c) => c,
        Err(err) => {
            unsafe { libc::close(master); }
            let _ = outbox.send(error_exit(id, &format!("spawn failed: {err}"))).await;
            return;
        }
    };
    let pid = child.id().map(|p| p as i32).unwrap_or(-1);
    let _ = outbox.send(Message::new(T_STARTED, id).with_i64("pid", pid as i64)).await;

    // Read the master from a blocking thread; PTY masters don't play nicely with
    // every async reactor and this keeps the read path simple and correct.
    let reader_outbox = outbox.clone();
    let master_for_read = unsafe { libc::dup(master) };
    let reader = std::thread::spawn(move || pty_read_loop(id, master_for_read, reader_outbox));

    let status = loop {
        tokio::select! {
            msg = control.recv() => match msg {
                Some(ProcControl::Stdin(bytes)) => { let _ = write_fd(master, &bytes); }
                Some(ProcControl::StdinClose) => {}
                Some(ProcControl::Resize { rows, cols }) => set_winsize(master, rows, cols),
                Some(ProcControl::Cancel) => signal_group(pid),
                None => {}
            },
            res = child.wait() => break res,
        }
    };

    // Closing the master makes the reader thread observe EOF/EIO and finish.
    unsafe { libc::close(master); }
    let _ = reader.join();
    let _ = outbox.send(exit_message(id, status)).await;
}

fn build_command(spec: &ExecSpec) -> Result<std::process::Command, String> {
    let program = spec
        .argv
        .first()
        .ok_or_else(|| "exec requires a non-empty argv".to_string())?;
    let mut command = std::process::Command::new(program);
    command.args(&spec.argv[1..]);
    command.env_clear();
    // Preserve a minimal sane PATH unless the caller overrides it.
    command.env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    for (k, v) in &spec.env {
        command.env(k, v);
    }
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    if !spec.pty {
        // setsid for non-PTY too, so cancel reaches the whole tree.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    Ok(command)
}

async fn stream_reader<R>(id: u64, mut reader: R, outbox: mpsc::Sender<Message>, tag: &'static str)
where
    R: AsyncReadExt + Unpin,
{
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if outbox.send(Message::new(tag, id).with_bytes("data", &buf[..n])).await.is_err() {
                    break;
                }
            }
        }
    }
}

fn pty_read_loop(id: u64, fd: RawFd, outbox: mpsc::Sender<Message>) {
    let mut buf = [0u8; READ_CHUNK];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            break;
        }
        let msg = Message::new(T_STDOUT, id).with_bytes("data", &buf[..n as usize]);
        if outbox.blocking_send(msg).is_err() {
            break;
        }
    }
    unsafe { libc::close(fd); }
}

fn exit_message(id: u64, status: std::io::Result<std::process::ExitStatus>) -> Message {
    match status {
        Ok(s) => {
            let code = s.code();
            Message::new(T_EXIT, id)
                .with_i64("code", code.unwrap_or(-1) as i64)
                .with_bool("success", s.success())
        }
        Err(err) => error_exit(id, &format!("wait failed: {err}")),
    }
}

fn error_exit(id: u64, message: &str) -> Message {
    Message::new(T_EXIT, id)
        .with_i64("code", -1)
        .with_bool("success", false)
        .with_str("error", message)
}

/// Send SIGTERM to the process group, then SIGKILL after a short grace period.
fn signal_group(pid: i32) {
    if pid <= 0 {
        return;
    }
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    });
}

fn write_fd(fd: RawFd, bytes: &[u8]) -> std::io::Result<()> {
    let mut off = 0;
    while off < bytes.len() {
        let n = unsafe {
            libc::write(
                fd,
                bytes[off..].as_ptr() as *const libc::c_void,
                bytes.len() - off,
            )
        };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        off += n as usize;
    }
    Ok(())
}

fn set_winsize(fd: RawFd, rows: u16, cols: u16) {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ as _, &ws);
    }
}

fn open_pty(rows: u16, cols: u16) -> std::io::Result<(RawFd, RawFd)> {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let mut ws = libc::winsize {
        ws_row: if rows == 0 { 24 } else { rows },
        ws_col: if cols == 0 { 80 } else { cols },
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut ws,
        )
    };
    if rc == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((master, slave))
}

unsafe fn dup_stdio(fd: RawFd) -> Option<Stdio> {
    let dup = libc::dup(fd);
    if dup == -1 {
        return None;
    }
    let owned = OwnedFd::from_raw_fd(dup);
    Some(Stdio::from(owned))
}
