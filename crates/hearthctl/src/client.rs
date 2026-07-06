use anyhow::{anyhow, Context, Result};
use camino::Utf8Path;
use hearth_proto::{Request, Response, Verb};
use serde_json::{json, Map, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use ulid::Ulid;

pub async fn hearth_request(
    socket: &Utf8Path,
    verb: Verb,
    args: Map<String, Value>,
) -> Result<Value> {
    let req = Request::new(Ulid::new().to_string(), verb, args);
    let stream = UnixStream::connect(socket.as_str())
        .await
        .with_context(|| format!("connect hearthd socket {socket}"))?;
    let (read, mut write) = stream.into_split();
    write
        .write_all(serde_json::to_string(&req)?.as_bytes())
        .await?;
    write.write_all(b"\n").await?;
    write.shutdown().await?;
    let mut lines = BufReader::new(read).lines();
    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow!("hearthd closed connection without a response"))?;
    let response: Response = serde_json::from_str(&line)?;
    if response.ok {
        Ok(response.result.unwrap_or_else(|| json!({})))
    } else {
        let message = response
            .error
            .map(|err| format!("{}: {}", err.code, err.message))
            .unwrap_or_else(|| "unknown hearthd error".to_string());
        Err(anyhow!(message))
    }
}
