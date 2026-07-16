//! Cloud Hypervisor's Firecracker-style *hybrid* vsock handshake (§6). There
//! is no host-side AF_VSOCK: the host connects to `/run/hearth/vsock/<vm>.sock`
//! and writes `CONNECT <port>\n`; CHV answers `OK <n>\n` and splices the raw
//! stream to whatever listens on that in-guest port. In test/emulation mode
//! guestd serves the same handshake on a plain unix socket, so every host-side
//! byte is identical with or without a hypervisor in the middle.

use crate::read_line_capped;
use std::io;
use std::path::Path;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

const HANDSHAKE_LINE_CAP: usize = 128;

/// Host side: connect to a hybrid vsock unix socket and request a guest port.
pub async fn connect_hybrid(socket: &Path, port: u32) -> io::Result<UnixStream> {
    let mut stream = UnixStream::connect(socket).await?;
    connect_handshake(&mut stream, port).await?;
    Ok(stream)
}

/// Host side, for an already-connected stream (e.g. an fd the broker passed):
/// perform the `CONNECT <port>` / `OK` exchange in-band.
pub async fn connect_handshake(stream: &mut UnixStream, port: u32) -> io::Result<()> {
    stream
        .write_all(format!("CONNECT {port}\n").as_bytes())
        .await?;
    let line = read_line_capped(stream, HANDSHAKE_LINE_CAP)
        .await?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "hybrid vsock peer closed during CONNECT",
            )
        })?;
    if line == "OK" || line.starts_with("OK ") {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("hybrid vsock CONNECT {port} refused: {line}"),
        ))
    }
}

/// Guest/emulation side: read the `CONNECT <port>` request and reply `OK`.
/// Returns the requested port so the acceptor can route the channel.
pub async fn accept_handshake(stream: &mut UnixStream) -> io::Result<u32> {
    let line = read_line_capped(stream, HANDSHAKE_LINE_CAP)
        .await?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "peer closed before CONNECT",
            )
        })?;
    let port = line
        .strip_prefix("CONNECT ")
        .and_then(|p| p.trim().parse::<u32>().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected CONNECT <port>, got: {line}"),
            )
        })?;
    stream.write_all(format!("OK {port}\n").as_bytes()).await?;
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn handshake_round_trips_and_stream_stays_clean() {
        let (mut host, mut guest) = UnixStream::pair().unwrap();
        let guest_task = tokio::spawn(async move {
            let port = accept_handshake(&mut guest).await.unwrap();
            assert_eq!(port, 1027);
            // First post-handshake byte from the host must arrive intact.
            let mut buf = [0u8; 5];
            guest.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"hello");
            guest.write_all(b"world").await.unwrap();
        });
        connect_handshake(&mut host, 1027).await.unwrap();
        host.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        host.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"world");
        guest_task.await.unwrap();
    }

    #[tokio::test]
    async fn refused_connect_is_an_error() {
        let (mut host, mut guest) = UnixStream::pair().unwrap();
        tokio::spawn(async move {
            let _ = read_line_capped(&mut guest, 128).await;
            let _ = guest.write_all(b"ERR no listener\n").await;
        });
        let err = connect_handshake(&mut host, 1027).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
