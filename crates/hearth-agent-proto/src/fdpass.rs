//! SCM_RIGHTS fd passing for the socket broker (§6). hearthd binds or connects
//! guest sockets and hands the fd to unprivileged agentd over its verb
//! channel; the vsock directory itself stays root-owned `0750`.
//!
//! Wire discipline: the fd rides a single one-byte (`F`) message sent *after*
//! the JSON response line that announced it. Both ends read lines
//! byte-at-a-time (`read_line_capped`), so no fd-bearing byte is ever
//! swallowed by read-ahead buffering.

use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use tokio::io::Interest;
use tokio::net::UnixStream;

/// Space for one 4-byte fd cmsg on 64-bit Linux: CMSG_ALIGN(sizeof(cmsghdr)) +
/// CMSG_ALIGN(4) = 16 + 8. Alignment matches cmsghdr's.
#[repr(align(8))]
struct CmsgBuf([u8; 24]);

fn send_fd_raw(sock: RawFd, fd: RawFd) -> io::Result<()> {
    let payload = [b'F'];
    let mut iov = libc::iovec {
        iov_base: payload.as_ptr() as *mut libc::c_void,
        iov_len: 1,
    };
    let mut cmsg_buf = CmsgBuf([0u8; 24]);
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.0.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(4) } as _;
    unsafe {
        let hdr = libc::CMSG_FIRSTHDR(&msg);
        (*hdr).cmsg_level = libc::SOL_SOCKET;
        (*hdr).cmsg_type = libc::SCM_RIGHTS;
        (*hdr).cmsg_len = libc::CMSG_LEN(4) as _;
        std::ptr::copy_nonoverlapping(&fd as *const RawFd as *const u8, libc::CMSG_DATA(hdr), 4);
    }
    let rc = unsafe { libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn recv_fd_raw(sock: RawFd) -> io::Result<OwnedFd> {
    let mut payload = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr() as *mut libc::c_void,
        iov_len: 1,
    };
    let mut cmsg_buf = CmsgBuf([0u8; 24]);
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.0.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(4) } as _;
    let rc = unsafe { libc::recvmsg(sock, &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    if rc == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "peer closed before sending fd",
        ));
    }
    unsafe {
        let mut hdr = libc::CMSG_FIRSTHDR(&msg);
        while !hdr.is_null() {
            if (*hdr).cmsg_level == libc::SOL_SOCKET && (*hdr).cmsg_type == libc::SCM_RIGHTS {
                let mut fd: RawFd = -1;
                std::ptr::copy_nonoverlapping(
                    libc::CMSG_DATA(hdr),
                    &mut fd as *mut RawFd as *mut u8,
                    4,
                );
                if fd >= 0 {
                    return Ok(OwnedFd::from_raw_fd(fd));
                }
            }
            hdr = libc::CMSG_NXTHDR(&msg, hdr);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "message carried no fd",
    ))
}

/// Send `fd` over a connected unix stream as a one-byte SCM_RIGHTS message.
pub async fn send_fd(stream: &UnixStream, fd: RawFd) -> io::Result<()> {
    loop {
        stream.writable().await?;
        match stream.try_io(Interest::WRITABLE, || send_fd_raw(stream.as_raw_fd(), fd)) {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
            Err(err) => return Err(err),
        }
    }
}

/// Receive one fd sent by [`send_fd`].
pub async fn recv_fd(stream: &UnixStream) -> io::Result<OwnedFd> {
    loop {
        stream.readable().await?;
        match stream.try_io(Interest::READABLE, || recv_fd_raw(stream.as_raw_fd())) {
            Ok(fd) => return Ok(fd),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
            Err(err) => return Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::IntoRawFd;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn passes_a_listener_fd_that_still_accepts() {
        let dir = std::env::temp_dir().join(format!("hearth-fdpass-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let listener_path = dir.join("passed.sock");
        let _ = std::fs::remove_file(&listener_path);
        let listener = std::os::unix::net::UnixListener::bind(&listener_path).unwrap();

        let (left, right) = UnixStream::pair().unwrap();
        send_fd(&left, listener.as_raw_fd()).await.unwrap();
        let received = recv_fd(&right).await.unwrap();

        // The received fd must be the same listening socket: accept a
        // connection through it.
        let std_listener =
            unsafe { std::os::unix::net::UnixListener::from_raw_fd(received.into_raw_fd()) };
        std_listener.set_nonblocking(true).unwrap();
        let tokio_listener = tokio::net::UnixListener::from_std(std_listener).unwrap();
        let mut client = UnixStream::connect(&listener_path).await.unwrap();
        let (mut accepted, _) = tokio_listener.accept().await.unwrap();
        client.write_all(b"ping").await.unwrap();
        drop(client);
        let mut buf = Vec::new();
        accepted.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, b"ping");
        let _ = std::fs::remove_file(&listener_path);
    }
}
