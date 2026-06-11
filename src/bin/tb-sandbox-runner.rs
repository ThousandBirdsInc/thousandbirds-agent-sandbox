use libc::c_char;
use std::env;
use std::ffi::CString;
use std::io;
use std::mem;
use std::os::fd::RawFd;
use std::ptr;
use std::thread::{self, JoinHandle};
use thousandbirds_libkrun as krun;

const KRUN_LOG_LEVEL_ERROR: u32 = 1;
const KRUN_DISK_FORMAT_RAW: u32 = 0;
const KRUN_SYNC_RELAXED: u32 = 1;
const NET_FLAG_VFKIT: u32 = 1 << 0;
const COMPAT_NET_FEATURES: u32 = (1 << 0) | (1 << 1) | (1 << 7) | (1 << 10) | (1 << 11) | (1 << 14);

#[derive(Debug)]
struct VirtiofsMount {
    tag: String,
    path: String,
    read_only: bool,
}

#[derive(Debug)]
struct Config {
    kernel: Option<String>,
    root_disk: String,
    root_device: String,
    root_fstype: String,
    root_options: Option<String>,
    guest_init: Option<String>,
    initramfs: Option<String>,
    kernel_cmdline: Option<String>,
    console: String,
    passt_socket: Option<String>,
    gvproxy_socket: Option<String>,
    control_socket: Option<String>,
    restore: Option<String>,
    /// Guest IPC vsock ports bridged to host unix sockets. Each entry is
    /// (guest vsock port, host unix socket path). The host connects to the
    /// unix socket; libkrun bridges it to a guest process listening on the
    /// vsock port (`listen = true`). Used for the agent-sandbox-daemon control
    /// channel.
    vsock_ports: Vec<(u32, String)>,
    virtiofs_mounts: Vec<VirtiofsMount>,
    virtiofs_dax_size: u64,
    kernel_format: u32,
    disk_sync_mode: u32,
    memory_mib: u32,
    balloon_initial_mib: u32,
    balloon_min_mib: u32,
    cpus: u8,
    net_features: u32,
    gvproxy_vfkit_magic: bool,
    poweroff_after_ready: bool,
    serial_pty: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            kernel: None,
            root_disk: String::new(),
            root_device: "/dev/vda1".to_string(),
            root_fstype: "ext4".to_string(),
            root_options: None,
            guest_init: None,
            initramfs: None,
            kernel_cmdline: None,
            console: default_console().to_string(),
            passt_socket: None,
            gvproxy_socket: None,
            control_socket: None,
            restore: None,
            vsock_ports: Vec::new(),
            virtiofs_mounts: Vec::new(),
            virtiofs_dax_size: 0,
            kernel_format: default_kernel_format(),
            disk_sync_mode: KRUN_SYNC_RELAXED,
            memory_mib: 2048,
            balloon_initial_mib: 0,
            balloon_min_mib: 0,
            cpus: 2,
            net_features: COMPAT_NET_FEATURES,
            gvproxy_vfkit_magic: false,
            poweroff_after_ready: false,
            serial_pty: false,
        }
    }
}

fn default_kernel_format() -> u32 {
    if cfg!(target_arch = "x86_64") {
        1
    } else if cfg!(target_arch = "aarch64") {
        2
    } else {
        0
    }
}

fn default_console() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "ttyS0"
    } else if cfg!(target_arch = "aarch64") {
        "ttyAMA0"
    } else {
        "hvc0"
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let config = match parse_args(&args) {
        Ok(ParseResult::Help) => {
            print_help(&args[0]);
            std::process::exit(0);
        }
        Ok(ParseResult::Run(config)) => config,
        Err(err) => {
            eprintln!("{err}");
            print_help(&args[0]);
            std::process::exit(1);
        }
    };

    if let Err(err) = run(config) {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

enum ParseResult {
    Help,
    Run(Config),
}

fn parse_args(args: &[String]) -> Result<ParseResult, String> {
    let mut config = Config::default();
    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-h" | "--help" => return Ok(ParseResult::Help),
            "-k" | "--kernel" => config.kernel = Some(take_value(args, &mut i, arg)?),
            "-d" | "--root-disk" => config.root_disk = take_value(args, &mut i, arg)?,
            "-r" | "--root-device" => config.root_device = take_value(args, &mut i, arg)?,
            "-t" | "--root-fstype" => config.root_fstype = take_value(args, &mut i, arg)?,
            "-o" | "--root-options" => config.root_options = Some(take_value(args, &mut i, arg)?),
            "--guest-init" => config.guest_init = Some(take_value(args, &mut i, arg)?),
            "-i" | "--initramfs" => config.initramfs = Some(take_value(args, &mut i, arg)?),
            "-c" | "--kernel-cmdline" => {
                config.kernel_cmdline = Some(take_value(args, &mut i, arg)?)
            }
            "-C" | "--console" => config.console = take_value(args, &mut i, arg)?,
            "-P" | "--passt-socket" => config.passt_socket = Some(take_value(args, &mut i, arg)?),
            "-G" | "--gvproxy-socket" => {
                config.gvproxy_socket = Some(take_value(args, &mut i, arg)?)
            }
            "-V" | "--virtiofs" => {
                config
                    .virtiofs_mounts
                    .push(parse_virtiofs(&take_value(args, &mut i, arg)?, false)?);
            }
            "--virtiofs-ro" => {
                config
                    .virtiofs_mounts
                    .push(parse_virtiofs(&take_value(args, &mut i, arg)?, true)?);
            }
            "--virtiofs-dax-size" => {
                config.virtiofs_dax_size = parse_u64(&take_value(args, &mut i, arg)?, arg)?;
            }
            "--restore" => config.restore = Some(take_value(args, &mut i, arg)?),
            "--cpus" => config.cpus = parse_u8(&take_value(args, &mut i, arg)?, arg)?,
            "--memory-mib" => {
                config.memory_mib = parse_nonzero_u32(&take_value(args, &mut i, arg)?, arg)?
            }
            "--balloon-initial-mib" => {
                config.balloon_initial_mib =
                    parse_nonzero_u32(&take_value(args, &mut i, arg)?, arg)?;
            }
            "--balloon-min-mib" => {
                config.balloon_min_mib = parse_u32(&take_value(args, &mut i, arg)?, arg)?;
            }
            "--control-socket" => config.control_socket = Some(take_value(args, &mut i, arg)?),
            "--vsock-port" => {
                config
                    .vsock_ports
                    .push(parse_vsock_port(&take_value(args, &mut i, arg)?)?);
            }
            "--disk-sync" => {
                config.disk_sync_mode = parse_disk_sync(&take_value(args, &mut i, arg)?)?;
            }
            "--gvproxy-vfkit-magic" => config.gvproxy_vfkit_magic = true,
            "--net-features" => {
                config.net_features = parse_u32(&take_value(args, &mut i, arg)?, arg)?;
            }
            "--poweroff-after-ready" => config.poweroff_after_ready = true,
            "--serial-pty" => config.serial_pty = true,
            "-f" | "--kernel-format" => {
                config.kernel_format = parse_u32(&take_value(args, &mut i, arg)?, arg)?;
            }
            _ => return Err(format!("unexpected argument: {arg}")),
        }
        i += 1;
    }

    validate_config(&config)?;
    Ok(ParseResult::Run(config))
}

fn take_value(args: &[String], i: &mut usize, name: &str) -> Result<String, String> {
    *i += 1;
    let value = args
        .get(*i)
        .ok_or_else(|| format!("missing value for {name}"))?;
    if value.is_empty() {
        return Err(format!("{name} must be non-empty"));
    }
    Ok(value.clone())
}

fn validate_config(config: &Config) -> Result<(), String> {
    if config.kernel.is_none() && config.restore.is_none() {
        return Err("missing --kernel".to_string());
    }
    if config.root_disk.is_empty() {
        return Err("missing --root-disk".to_string());
    }
    if config.passt_socket.is_some() && config.gvproxy_socket.is_some() {
        return Err("--passt-socket and --gvproxy-socket are mutually exclusive".to_string());
    }
    validate_token("--root-device", &config.root_device)?;
    validate_token("--root-fstype", &config.root_fstype)?;
    if let Some(value) = &config.root_options {
        validate_token("--root-options", value)?;
    }
    if let Some(value) = &config.guest_init {
        validate_token("--guest-init", value)?;
    }
    validate_token("--console", &config.console)?;
    if config.kernel_format > 5 {
        return Err(format!("invalid --kernel-format: {}", config.kernel_format));
    }
    if config.virtiofs_mounts.len() > 8 {
        return Err("too many virtio-fs mounts; maximum is 8".to_string());
    }
    Ok(())
}

fn validate_token(name: &str, value: &str) -> Result<(), String> {
    if !value.is_empty() && !value.bytes().any(|byte| byte.is_ascii_whitespace()) {
        Ok(())
    } else {
        Err(format!(
            "{name} must be a non-empty single kernel command-line token"
        ))
    }
}

fn parse_virtiofs(value: &str, read_only: bool) -> Result<VirtiofsMount, String> {
    let (tag, path) = value
        .split_once('=')
        .ok_or_else(|| "--virtiofs must use TAG=PATH with non-empty values".to_string())?;
    if tag.is_empty() || path.is_empty() {
        return Err("--virtiofs must use TAG=PATH with non-empty values".to_string());
    }
    Ok(VirtiofsMount {
        tag: tag.to_string(),
        path: path.to_string(),
        read_only,
    })
}

fn parse_vsock_port(value: &str) -> Result<(u32, String), String> {
    let (port, path) = value
        .split_once('=')
        .ok_or_else(|| "--vsock-port must use PORT=SOCKET_PATH".to_string())?;
    if path.is_empty() {
        return Err("--vsock-port must use PORT=SOCKET_PATH with a non-empty path".to_string());
    }
    let port = port
        .parse::<u32>()
        .map_err(|_| format!("invalid --vsock-port number: {port}"))?;
    Ok((port, path.to_string()))
}

fn parse_disk_sync(value: &str) -> Result<u32, String> {
    match value {
        "none" => Ok(0),
        "relaxed" => Ok(1),
        "full" => Ok(2),
        other => {
            let parsed = parse_u32(other, "--disk-sync")?;
            if parsed <= 2 {
                Ok(parsed)
            } else {
                Err(format!("invalid --disk-sync: {value}"))
            }
        }
    }
}

fn parse_u8(value: &str, name: &str) -> Result<u8, String> {
    let parsed = parse_nonzero_u32(value, name)?;
    u8::try_from(parsed).map_err(|_| format!("invalid {name}: {value}"))
}

fn parse_nonzero_u32(value: &str, name: &str) -> Result<u32, String> {
    let parsed = parse_u32(value, name)?;
    if parsed == 0 {
        Err(format!("invalid {name}: {value}"))
    } else {
        Ok(parsed)
    }
}

fn parse_u32(value: &str, name: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .map_err(|_| format!("invalid {name}: {value}"))
}

fn parse_u64(value: &str, name: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("invalid {name}: {value}"))
}

fn run(config: Config) -> Result<(), String> {
    check_call(
        "krun_set_log_level",
        krun::krun_set_log_level(KRUN_LOG_LEVEL_ERROR),
    )?;
    let ctx_id = krun::krun_create_ctx();
    if ctx_id < 0 {
        return Err(format_krun_error("krun_create_ctx", ctx_id));
    }
    let ctx_id = ctx_id as u32;

    check_call(
        "krun_set_vm_config",
        krun::krun_set_vm_config(ctx_id, config.cpus, config.memory_mib),
    )?;

    if let Some(control_socket) = &config.control_socket {
        let control_socket = cstring("--control-socket", control_socket)?;
        let initial_mib = if config.balloon_initial_mib == 0 {
            config.memory_mib
        } else {
            config.balloon_initial_mib
        };
        check_call("krun_set_balloon", unsafe {
            krun::krun_set_balloon(
                ctx_id,
                initial_mib,
                config.balloon_min_mib,
                control_socket.as_ptr(),
            )
        })?;
    }

    check_call("krun_set_os_mode", krun::krun_set_os_mode(ctx_id))?;

    // Explicit vsock device for guest IPC (the agent-sandbox-daemon control
    // channel). We add a plain vsock device (tsi_features = 0) and map each
    // requested port to its host unix socket with `listen = true` so the host
    // initiates the connection.
    //
    // The implicit vsock must be disabled FIRST: `krun_add_vsock` rejects a
    // context whose vsock isn't already `Disabled` (returns EEXIST). libkrun
    // only auto-disables the implicit vsock inside `krun_start_enter` — too late
    // for us — so we disable it explicitly here. (Without this, boot fails with
    // an opaque `krun_add_vsock: File exists (os error 17)` and the VM never
    // starts.)
    if !config.vsock_ports.is_empty() {
        check_call(
            "krun_disable_implicit_vsock",
            krun::krun_disable_implicit_vsock(ctx_id),
        )?;
        check_call("krun_add_vsock", krun::krun_add_vsock(ctx_id, 0))?;
        for (port, path) in &config.vsock_ports {
            let path = cstring("--vsock-port", path)?;
            check_call("krun_add_vsock_port2", unsafe {
                krun::krun_add_vsock_port2(ctx_id, *port, path.as_ptr(), true)
            })?;
        }
    }

    if let Some(restore) = &config.restore {
        let restore = cstring("--restore", restore)?;
        check_call("krun_set_snapshot_restore_from", unsafe {
            krun::krun_set_snapshot_restore_from(ctx_id, restore.as_ptr())
        })?;
    }

    if let Some(kernel) = &config.kernel {
        let kernel = cstring("--kernel", kernel)?;
        let initramfs = optional_cstring("--initramfs", config.initramfs.as_deref())?;
        let kernel_cmdline = effective_kernel_cmdline(&config)?;
        let kernel_cmdline = optional_cstring("--kernel-cmdline", kernel_cmdline.as_deref())?;
        check_call("krun_set_kernel", unsafe {
            krun::krun_set_kernel(
                ctx_id,
                kernel.as_ptr(),
                config.kernel_format,
                opt_ptr(&initramfs),
                opt_ptr(&kernel_cmdline),
            )
        })?;
    }

    if let Some(guest_init) = &config.guest_init {
        let guest_init = cstring("--guest-init", guest_init)?;
        check_call("krun_set_os_init", unsafe {
            krun::krun_set_os_init(ctx_id, guest_init.as_ptr())
        })?;
    }

    let root_block = cstring("block id", "root")?;
    let root_disk = cstring("--root-disk", &config.root_disk)?;
    check_call("krun_add_disk3", unsafe {
        krun::krun_add_disk3(
            ctx_id,
            root_block.as_ptr(),
            root_disk.as_ptr(),
            KRUN_DISK_FORMAT_RAW,
            false,
            false,
            config.disk_sync_mode,
        )
    })?;

    let root_device = cstring("--root-device", &config.root_device)?;
    let root_fstype = cstring("--root-fstype", &config.root_fstype)?;
    let root_options = optional_cstring("--root-options", config.root_options.as_deref())?;
    check_call("krun_set_os_root", unsafe {
        krun::krun_set_os_root(
            ctx_id,
            root_device.as_ptr(),
            root_fstype.as_ptr(),
            opt_ptr(&root_options),
        )
    })?;

    let console = cstring("--console", &config.console)?;
    check_call("krun_set_kernel_console", unsafe {
        krun::krun_set_kernel_console(ctx_id, console.as_ptr())
    })?;

    for mount in &config.virtiofs_mounts {
        let tag = cstring("--virtiofs tag", &mount.tag)?;
        let path = cstring("--virtiofs path", &mount.path)?;
        check_call("krun_add_virtiofs3", unsafe {
            krun::krun_add_virtiofs3(
                ctx_id,
                tag.as_ptr(),
                path.as_ptr(),
                config.virtiofs_dax_size,
                mount.read_only,
            )
        })?;
    }

    if let Some(socket) = &config.passt_socket {
        let socket = cstring("--passt-socket", socket)?;
        let mac = [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee];
        check_call("krun_add_net_unixstream", unsafe {
            krun::krun_add_net_unixstream(
                ctx_id,
                socket.as_ptr(),
                -1,
                mac.as_ptr(),
                config.net_features,
                0,
            )
        })?;
    }
    if let Some(socket) = &config.gvproxy_socket {
        let socket = cstring("--gvproxy-socket", socket)?;
        let mac = [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee];
        let flags = if config.gvproxy_vfkit_magic {
            NET_FLAG_VFKIT
        } else {
            0
        };
        check_call("krun_add_net_unixgram", unsafe {
            krun::krun_add_net_unixgram(
                ctx_id,
                socket.as_ptr(),
                -1,
                mac.as_ptr(),
                config.net_features,
                flags,
            )
        })?;
    }

    let mut bridge = if config.serial_pty {
        Some(SerialPtyBridge::new()?)
    } else {
        None
    };
    let (serial_input_fd, serial_output_fd) = bridge
        .as_ref()
        .map(|bridge| (bridge.slave_fd, bridge.slave_fd))
        .unwrap_or((libc::STDIN_FILENO, libc::STDOUT_FILENO));

    check_call("krun_add_serial_console_default", unsafe {
        krun::krun_add_serial_console_default(ctx_id, serial_input_fd, serial_output_fd)
    })?;

    let result = check_call("krun_start_enter", krun::krun_start_enter(ctx_id));
    if let Some(bridge) = bridge.as_mut() {
        bridge.stop();
    }
    result
}

fn effective_kernel_cmdline(config: &Config) -> Result<Option<String>, String> {
    const POWEROFF_MARKER: &str = "KRUN_OSMODE_POWEROFF=1";
    if !config.poweroff_after_ready {
        return Ok(config.kernel_cmdline.clone());
    }
    Ok(Some(match &config.kernel_cmdline {
        Some(cmdline) if !cmdline.is_empty() => format!("{cmdline} {POWEROFF_MARKER}"),
        _ => POWEROFF_MARKER.to_string(),
    }))
}

fn cstring(name: &str, value: &str) -> Result<CString, String> {
    CString::new(value).map_err(|_| format!("{name} contains an interior NUL byte"))
}

fn optional_cstring(name: &str, value: Option<&str>) -> Result<Option<CString>, String> {
    value.map(|value| cstring(name, value)).transpose()
}

fn opt_ptr(value: &Option<CString>) -> *const c_char {
    value
        .as_ref()
        .map(|value| value.as_ptr())
        .unwrap_or(ptr::null())
}

fn check_call(operation: &str, result: i32) -> Result<(), String> {
    if result == 0 {
        Ok(())
    } else {
        Err(format_krun_error(operation, result))
    }
}

fn format_krun_error(operation: &str, result: i32) -> String {
    let err = io::Error::from_raw_os_error(-result);
    format!("{operation}: {err}")
}

struct SerialPtyBridge {
    master_fd: RawFd,
    slave_fd: RawFd,
    wake_pipe: [RawFd; 2],
    host_old_termios: Option<libc::termios>,
    thread: Option<JoinHandle<()>>,
}

impl SerialPtyBridge {
    fn new() -> Result<Self, String> {
        let master_fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
        if master_fd < 0 {
            return Err(last_os_error("posix_openpt"));
        }
        if unsafe { libc::grantpt(master_fd) } < 0 || unsafe { libc::unlockpt(master_fd) } < 0 {
            unsafe {
                libc::close(master_fd);
            }
            return Err(last_os_error("grantpt/unlockpt"));
        }
        let slave_name = unsafe { libc::ptsname(master_fd) };
        if slave_name.is_null() {
            unsafe {
                libc::close(master_fd);
            }
            return Err(last_os_error("ptsname"));
        }
        let slave_fd = unsafe { libc::open(slave_name, libc::O_RDWR | libc::O_NOCTTY) };
        if slave_fd < 0 {
            unsafe {
                libc::close(master_fd);
            }
            return Err(last_os_error("open pty slave"));
        }

        make_raw(slave_fd, false)?;
        copy_winsize_to_pty(slave_fd);

        let host_old_termios = if unsafe { libc::isatty(libc::STDIN_FILENO) } != 0 {
            make_raw(libc::STDIN_FILENO, true).ok()
        } else {
            None
        };

        let mut wake_pipe = [-1, -1];
        if unsafe { libc::pipe(wake_pipe.as_mut_ptr()) } < 0 {
            restore_termios(host_old_termios.as_ref());
            unsafe {
                libc::close(slave_fd);
                libc::close(master_fd);
            }
            return Err(last_os_error("pipe"));
        }

        let thread_wake_pipe = wake_pipe;
        let thread = thread::spawn(move || {
            serial_pty_bridge_thread(master_fd, slave_fd, thread_wake_pipe[0]);
        });

        Ok(Self {
            master_fd,
            slave_fd,
            wake_pipe,
            host_old_termios,
            thread: Some(thread),
        })
    }

    fn stop(&mut self) {
        if self.wake_pipe[1] >= 0 {
            let byte = [b'x'];
            unsafe {
                libc::write(self.wake_pipe[1], byte.as_ptr().cast(), byte.len());
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        restore_termios(self.host_old_termios.as_ref());
    }
}

impl Drop for SerialPtyBridge {
    fn drop(&mut self) {
        self.stop();
        for fd in [
            self.wake_pipe[0],
            self.wake_pipe[1],
            self.slave_fd,
            self.master_fd,
        ] {
            if fd >= 0 {
                unsafe {
                    libc::close(fd);
                }
            }
        }
    }
}

fn make_raw(fd: RawFd, preserve_signals: bool) -> Result<libc::termios, String> {
    let mut old = unsafe { mem::zeroed::<libc::termios>() };
    if unsafe { libc::tcgetattr(fd, &mut old) } < 0 {
        return Err(last_os_error("tcgetattr"));
    }
    let mut raw = old;
    unsafe {
        libc::cfmakeraw(&mut raw);
    }
    if preserve_signals {
        raw.c_lflag |= libc::ISIG;
    }
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } < 0 {
        return Err(last_os_error("tcsetattr"));
    }
    Ok(old)
}

fn restore_termios(old: Option<&libc::termios>) {
    if let Some(old) = old {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, old);
        }
    }
}

fn serial_pty_bridge_thread(master_fd: RawFd, slave_fd: RawFd, wake_fd: RawFd) {
    let mut buf = [0_u8; 4096];
    loop {
        copy_winsize_to_pty(slave_fd);
        let mut fds = [
            libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: master_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 1000) };
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break;
        }
        if ready == 0 {
            continue;
        }
        if fds[2].revents != 0 {
            break;
        }
        if fds[0].revents & libc::POLLIN != 0 {
            let count =
                unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
            if count > 0 {
                let _ = write_all(master_fd, &buf[..count as usize]);
            } else if count == 0 || !last_errno_is_retry() {
                break;
            }
        }
        if fds[1].revents & libc::POLLIN != 0 {
            let count = unsafe { libc::read(master_fd, buf.as_mut_ptr().cast(), buf.len()) };
            if count > 0 {
                let _ = write_all(libc::STDOUT_FILENO, &buf[..count as usize]);
            } else if count == 0 || !last_errno_is_retry() {
                break;
            }
        }
    }
}

fn write_all(fd: RawFd, mut buf: &[u8]) -> bool {
    while !buf.is_empty() {
        let written = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
        if written > 0 {
            buf = &buf[written as usize..];
            continue;
        }
        if written < 0 {
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) => {
                    let mut pfd = libc::pollfd {
                        fd,
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    unsafe {
                        libc::poll(&mut pfd, 1, -1);
                    }
                    continue;
                }
                _ => return false,
            }
        }
    }
    true
}

fn copy_winsize_to_pty(slave_fd: RawFd) {
    let mut size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        if libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut size) == 0 {
            if size.ws_row == 0 {
                size.ws_row = 24;
            }
            if size.ws_col == 0 {
                size.ws_col = 80;
            }
        }
        libc::ioctl(slave_fd, libc::TIOCSWINSZ, &size);
    }
}

fn last_errno_is_retry() -> bool {
    matches!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::EINTR) | Some(libc::EAGAIN)
    )
}

fn last_os_error(operation: &str) -> String {
    format!("{operation}: {}", io::Error::last_os_error())
}

fn print_help(name: &str) {
    eprintln!(
        "Usage: {name} [OPTIONS] --kernel PATH --root-disk PATH\n\
         \n\
         Options:\n\
         \x20 -k, --kernel PATH          Guest kernel image\n\
         \x20 -d, --root-disk PATH       Raw root disk image\n\
         \x20 -r, --root-device DEVICE   Guest root device (default: /dev/vda1)\n\
         \x20 -t, --root-fstype FSTYPE   Root filesystem type (default: ext4)\n\
         \x20 -o, --root-options OPTS    Root mount options\n\
         \x20     --guest-init PATH      Guest init path\n\
         \x20 -i, --initramfs PATH       Optional initramfs image\n\
         \x20 -c, --kernel-cmdline TEXT  Extra kernel command line\n\
         \x20 -C, --console NAME         Kernel console name\n\
         \x20 -P, --passt-socket PATH    Connect virtio-net to a passt unixstream socket\n\
         \x20 -G, --gvproxy-socket PATH  Connect virtio-net to a gvproxy unixgram socket\n\
         \x20 -V, --virtiofs TAG=PATH    Add a writable virtio-fs mount\n\
         \x20     --virtiofs-ro TAG=PATH Add a read-only virtio-fs mount\n\
         \x20     --virtiofs-dax-size N  DAX window bytes for virtio-fs mounts\n\
         \x20     --cpus NUM             Number of guest vCPUs\n\
         \x20     --memory-mib NUM       Guest RAM in MiB\n\
         \x20     --balloon-initial-mib NUM\n\
         \x20     --balloon-min-mib NUM\n\
         \x20     --control-socket PATH\n\
         \x20     --vsock-port PORT=PATH Bridge a guest vsock PORT to a host unix socket\n\
         \x20     --disk-sync MODE       relaxed, full, none, or 0-2\n\
         \x20     --gvproxy-vfkit-magic\n\
         \x20     --net-features NUM\n\
         \x20     --poweroff-after-ready\n\
         \x20     --serial-pty\n\
         \x20 -f, --kernel-format NUM\n\
         \x20 -h, --help"
    );
}
