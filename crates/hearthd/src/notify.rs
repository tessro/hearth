use anyhow::{anyhow, Context, Result};
use libc::{sockaddr, sockaddr_un, socklen_t, AF_UNIX, SOCK_CLOEXEC, SOCK_DGRAM};
use std::{env, ffi::OsString, mem, os::fd::RawFd, os::unix::ffi::OsStrExt};

pub fn ready() -> Result<()> {
    let Some(socket) = env::var_os("NOTIFY_SOCKET") else {
        return Ok(());
    };
    send_notify(socket, b"READY=1")
}

fn send_notify(socket: OsString, message: &[u8]) -> Result<()> {
    let fd = unsafe { libc::socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("create notify socket");
    }
    let result = send_notify_fd(fd, socket, message);
    unsafe {
        libc::close(fd);
    }
    result
}

fn send_notify_fd(fd: RawFd, socket: OsString, message: &[u8]) -> Result<()> {
    let bytes = socket.as_os_str().as_bytes();
    if bytes.is_empty() {
        return Ok(());
    }
    let mut addr: sockaddr_un = unsafe { mem::zeroed() };
    addr.sun_family = AF_UNIX as libc::sa_family_t;
    let len = if bytes[0] == b'@' {
        if bytes.len() > addr.sun_path.len() {
            return Err(anyhow!("NOTIFY_SOCKET path is too long"));
        }
        addr.sun_path[0] = 0;
        for (idx, byte) in bytes[1..].iter().enumerate() {
            addr.sun_path[idx + 1] = *byte as libc::c_char;
        }
        mem::size_of_val(&addr.sun_family) + bytes.len()
    } else {
        if bytes.len() >= addr.sun_path.len() {
            return Err(anyhow!("NOTIFY_SOCKET path is too long"));
        }
        for (idx, byte) in bytes.iter().enumerate() {
            addr.sun_path[idx] = *byte as libc::c_char;
        }
        mem::size_of_val(&addr.sun_family) + bytes.len() + 1
    };
    let sent = unsafe {
        libc::sendto(
            fd,
            message.as_ptr().cast(),
            message.len(),
            0,
            &addr as *const sockaddr_un as *const sockaddr,
            len as socklen_t,
        )
    };
    if sent < 0 {
        return Err(std::io::Error::last_os_error()).context("send systemd notify message");
    }
    Ok(())
}
