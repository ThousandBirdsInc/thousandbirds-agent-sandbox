//! Minimal async `AF_VSOCK` listener for the guest side. Tokio has no native
//! vsock type, so this wraps a raw libc socket in an `AsyncFd` and exposes an
//! `accept` plus an `AsyncRead`/`AsyncWrite` stream. Linux-only — the daemon's
//! Unix-socket path covers macOS-host tests and local debugging.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

// VMADDR_CID_ANY = bind on any CID (the guest accepts host-initiated streams).
const VMADDR_CID_ANY: libc::c_uint = 0xffffffff;

#[repr(C)]
struct SockaddrVm {
    svm_family: libc::sa_family_t,
    svm_reserved1: libc::c_ushort,
    svm_port: libc::c_uint,
    svm_cid: libc::c_uint,
    svm_zero: [u8; 4],
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL, 0) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// An accepted vsock connection.
pub struct VsockStream {
    inner: AsyncFd<OwnedFd>,
}

impl VsockStream {
    fn from_raw(fd: RawFd) -> io::Result<Self> {
        set_nonblocking(fd)?;
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Self {
            inner: AsyncFd::new(owned)?,
        })
    }
}

impl AsyncRead for VsockStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            let mut guard = match this.inner.poll_read_ready(cx) {
                Poll::Ready(Ok(g)) => g,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };
            let unfilled = buf.initialize_unfilled();
            let res = guard.try_io(|fd| {
                let n = unsafe {
                    libc::read(
                        fd.as_raw_fd(),
                        unfilled.as_mut_ptr() as *mut libc::c_void,
                        unfilled.len(),
                    )
                };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match res {
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                Err(_would_block) => continue,
            }
        }
    }
}

impl AsyncWrite for VsockStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        loop {
            let mut guard = match this.inner.poll_write_ready(cx) {
                Poll::Ready(Ok(g)) => g,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };
            let res = guard.try_io(|fd| {
                let n = unsafe {
                    libc::write(
                        fd.as_raw_fd(),
                        data.as_ptr() as *const libc::c_void,
                        data.len(),
                    )
                };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match res {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let fd = self.inner.get_ref().as_raw_fd();
        unsafe {
            libc::shutdown(fd, libc::SHUT_WR);
        }
        Poll::Ready(Ok(()))
    }
}

/// Bind an `AF_VSOCK` listener on `port` and serve each accepted connection
/// with [`crate::server::handle_connection`].
pub async fn serve_vsock(port: u32) -> io::Result<()> {
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    let addr = SockaddrVm {
        svm_family: libc::AF_VSOCK as libc::sa_family_t,
        svm_reserved1: 0,
        svm_port: port,
        svm_cid: VMADDR_CID_ANY,
        svm_zero: [0; 4],
    };
    let rc = unsafe {
        libc::bind(
            fd,
            &addr as *const SockaddrVm as *const libc::sockaddr,
            std::mem::size_of::<SockaddrVm>() as libc::socklen_t,
        )
    };
    if rc == -1 {
        let err = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err);
    }
    if unsafe { libc::listen(fd, 64) } == -1 {
        let err = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err);
    }
    set_nonblocking(fd)?;
    let listener = AsyncFd::new(unsafe { OwnedFd::from_raw_fd(fd) })?;

    loop {
        let mut guard = listener.readable().await?;
        let accepted = guard.try_io(|l| {
            let conn = unsafe { libc::accept(l.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut()) };
            if conn == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(conn)
            }
        });
        match accepted {
            Ok(Ok(conn_fd)) => {
                if let Ok(stream) = VsockStream::from_raw(conn_fd) {
                    tokio::spawn(crate::server::handle_connection(stream));
                }
            }
            Ok(Err(_)) => continue,
            Err(_would_block) => continue,
        }
    }
}
