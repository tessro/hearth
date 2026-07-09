use anyhow::{anyhow, Context, Result};
use camino::Utf8Path;
use hearth_proto::{empty_args, ErrorBody, Request, Response, Verb};
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
    let response = request_raw(socket, &req).await?;
    if response.ok {
        Ok(response.result.unwrap_or_else(|| json!({})))
    } else {
        Err(request_failure(socket, &req.verb, response.error).await)
    }
}

/// One request, one response, with no stale-daemon rewriting. Kept separate so
/// `stale_daemon_hint` can issue its follow-up `version` request without
/// recursing back into the hint logic.
async fn request_raw(socket: &Utf8Path, req: &Request) -> Result<Response> {
    let stream = UnixStream::connect(socket.as_str())
        .await
        .with_context(|| format!("connect hearthd socket {socket}"))?;
    let (read, mut write) = stream.into_split();
    write
        .write_all(serde_json::to_string(req)?.as_bytes())
        .await?;
    write.write_all(b"\n").await?;
    write.shutdown().await?;
    let mut lines = BufReader::new(read).lines();
    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow!("hearthd closed connection without a response"))?;
    Ok(serde_json::from_str(&line)?)
}

/// Turn a failed response into a caller-facing error, upgrading an unknown-verb
/// serde error into an actionable stale-daemon message when possible.
async fn request_failure(
    socket: &Utf8Path,
    verb: &Verb,
    error: Option<ErrorBody>,
) -> anyhow::Error {
    if let Some(err) = &error {
        if let Some(hint) = stale_daemon_hint(socket, verb, &err.code, &err.message).await {
            return anyhow!(hint);
        }
    }
    match error {
        Some(err) => anyhow!("{}: {}", err.code, err.message),
        None => anyhow!("unknown hearthd error"),
    }
}

/// A serde "unknown variant" failure means the daemon's `Verb` enum lacks the
/// verb the CLI just sent — i.e. the daemon is older than this CLI. Detect that
/// shape (it surfaces as `protocol.invalid_json` today) so the raw serde dump
/// never reaches a human.
fn is_unknown_verb_error(code: &str, message: &str) -> bool {
    matches!(code, "protocol.invalid_json" | "protocol.unknown_verb")
        && message.contains("unknown variant")
}

fn stale_daemon_message(daemon_version: &str, verb: &Verb) -> String {
    format!(
        "daemon {daemon_version} does not support '{verb}'; hearthctl is {} — restart hearthd",
        env!("CARGO_PKG_VERSION")
    )
}

/// If `code`/`message` is an unknown-verb error, ask the daemon its version on a
/// fresh connection and return an actionable stale-daemon message. Returns
/// `None` (keep the original error) when this isn't an unknown-verb error or the
/// follow-up `version` call fails.
pub async fn stale_daemon_hint(
    socket: &Utf8Path,
    verb: &Verb,
    code: &str,
    message: &str,
) -> Option<String> {
    // A failing `version` call is exactly what we'd fall back on; never recurse.
    if verb == &Verb::Version || !is_unknown_verb_error(code, message) {
        return None;
    }
    let req = Request::new(Ulid::new().to_string(), Verb::Version, empty_args());
    let response = request_raw(socket, &req).await.ok()?;
    if !response.ok {
        return None;
    }
    let daemon_version = response
        .result
        .as_ref()
        .and_then(|value| value.get("version"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    Some(stale_daemon_message(daemon_version, verb))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_real_unknown_variant_serde_message() {
        // Pin the detector to serde's actual wording, not a guess: deserializing
        // a bogus verb produces the same "unknown variant" error the daemon
        // returns as protocol.invalid_json.
        let err =
            serde_json::from_str::<Request>(r#"{"id":"1","verb":"totally-made-up"}"#).unwrap_err();
        assert!(is_unknown_verb_error(
            "protocol.invalid_json",
            &err.to_string()
        ));
    }

    #[test]
    fn ignores_errors_that_are_not_unknown_verbs() {
        assert!(!is_unknown_verb_error(
            "service.not_found",
            "no such service"
        ));
        assert!(!is_unknown_verb_error(
            "protocol.invalid_json",
            "expected value at line 1 column 1"
        ));
    }

    #[test]
    fn stale_message_names_both_versions_and_the_verb() {
        let msg = stale_daemon_message("0.1.0", &Verb::ImageImport);
        assert!(msg.contains("daemon 0.1.0 does not support 'image-import'"));
        assert!(msg.contains("restart hearthd"));
        assert!(msg.contains(env!("CARGO_PKG_VERSION")));
    }
}
