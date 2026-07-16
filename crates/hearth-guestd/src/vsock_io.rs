//! Minimal async AF_VSOCK support (in-guest side only). Tokio has no native
//! vsock type and pulling a crate for two socket calls is not the house way:
//! a nonblocking fd wrapped in `AsyncFd` with hand-written AsyncRead/AsyncWrite
//! is all guestd needs. Host-side code never touches AF_VSOCK at all — CHV's
//! hybrid model lands every guest connection on a host unix socket.

use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub const VMADDR_CID_HOST: u32 = 2;

fn vsock_addr(cid: u32, port: u32) -> libc::sockaddr_vm {
    let mut addr: libc::sockaddr_vm = unsafe { mem::zeroed() };
    addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    addr.svm_cid = cid;
    addr.svm_port = port;
    addr
}

fn new_vsock_fd() -> io::Result<OwnedFd> {
    let fd = unsafe {
        libc::socket(
            libc::AF_VSOCK,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub struct VsockStream {
    fd: AsyncFd<OwnedFd>,
}

impl VsockStream {
    /// Connect to `(cid, port)` — for guestd, always `(2, port)`: the host.
    pub async fn connect(cid: u32, port: u32) -> io::Result<Self> {
        let fd = new_vsock_fd()?;
        let addr = vsock_addr(cid, port);
        let rc = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
                mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINPROGRESS) {
                return Err(err);
            }
        }
        let fd = AsyncFd::new(fd)?;
        // Wait for connect completion, then read SO_ERROR for the verdict.
        let mut guard = fd.writable().await?;
        guard.clear_ready();
        let mut err: libc::c_int = 0;
        let mut len = mem::size_of::<libc::c_int>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                &mut err as *mut libc::c_int as *mut libc::c_void,
                &mut len,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        if err != 0 {
            return Err(io::Error::from_raw_os_error(err));
        }
        Ok(Self { fd })
    }

    fn from_owned(fd: OwnedFd) -> io::Result<Self> {
        Ok(Self {
            fd: AsyncFd::new(fd)?,
        })
    }
}

impl AsyncRead for VsockStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = match self.fd.poll_read_ready(cx) {
                Poll::Ready(Ok(guard)) => guard,
                Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                Poll::Pending => return Poll::Pending,
            };
            let unfilled = buf.initialize_unfilled();
            let rc = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    unfilled.as_mut_ptr() as *mut libc::c_void,
                    unfilled.len(),
                )
            };
            if rc >= 0 {
                buf.advance(rc as usize);
                return Poll::Ready(Ok(()));
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                guard.clear_ready();
                continue;
            }
            return Poll::Ready(Err(err));
        }
    }
}

impl AsyncWrite for VsockStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = match self.fd.poll_write_ready(cx) {
                Poll::Ready(Ok(guard)) => guard,
                Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                Poll::Pending => return Poll::Pending,
            };
            let rc = unsafe {
                libc::send(
                    self.fd.as_raw_fd(),
                    buf.as_ptr() as *const libc::c_void,
                    buf.len(),
                    libc::MSG_NOSIGNAL,
                )
            };
            if rc >= 0 {
                return Poll::Ready(Ok(rc as usize));
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                guard.clear_ready();
                continue;
            }
            return Poll::Ready(Err(err));
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let rc = unsafe { libc::shutdown(self.fd.as_raw_fd(), libc::SHUT_WR) };
        if rc < 0 {
            return Poll::Ready(Err(io::Error::last_os_error()));
        }
        Poll::Ready(Ok(()))
    }
}

pub struct VsockListener {
    fd: AsyncFd<OwnedFd>,
}

impl VsockListener {
    /// Listen on an in-guest vsock port (any CID).
    pub fn bind(port: u32) -> io::Result<Self> {
        let fd = new_vsock_fd()?;
        let addr = vsock_addr(libc::VMADDR_CID_ANY, port);
        let rc = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
                mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        let rc = unsafe { libc::listen(fd.as_raw_fd(), 64) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            fd: AsyncFd::new(fd)?,
        })
    }

    pub async fn accept(&self) -> io::Result<VsockStream> {
        loop {
            let mut guard = self.fd.readable().await?;
            let rc = unsafe {
                libc::accept4(
                    self.fd.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                )
            };
            if rc >= 0 {
                return VsockStream::from_owned(unsafe { OwnedFd::from_raw_fd(rc) });
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                guard.clear_ready();
                continue;
            }
            return Err(err);
        }
    }
}
