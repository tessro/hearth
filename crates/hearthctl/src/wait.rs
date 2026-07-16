//! `hearthctl wait <name> --marker <string> [--timeout <secs>]`: block until a
//! marker substring appears in a service's console log, then exit 0 printing the
//! matched line; exit non-zero on timeout (or if the stream closes first) with
//! the last few lines for context.
//!
//! This is the client-side consumer of the daemon's existing `logs --follow`
//! stream (`Verb::Logs` with `follow = true`). It replaces the three copy-pasted
//! `wait_for_log()` bash loops in the acceptance scripts with one first-class
//! readiness signal (REFACTOR_PROPOSAL.md §5). The daemon protocol is unchanged.

use anyhow::{anyhow, bail, Context, Result};
use camino::Utf8Path;
use hearth_proto::{Request, Response, StreamKind, Verb};
use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::{timeout, Duration, Instant},
};
use ulid::Ulid;

/// How many trailing log lines to show as context when the wait fails.
const CONTEXT_LINES: usize = 10;

/// A bounded, pure scan of a log stream for a marker substring. Feed each log
/// line to [`MarkerScan::push`]; it records the first line that contained the
/// marker and keeps the last `context_cap` lines for a failure report. Pure so
/// the match/tail behavior is unit-testable without a socket.
#[derive(Debug)]
pub struct MarkerScan {
    marker: String,
    context_cap: usize,
    context: VecDeque<String>,
    matched: Option<String>,
}

impl MarkerScan {
    pub fn new(marker: impl Into<String>, context_cap: usize) -> Self {
        Self {
            marker: marker.into(),
            context_cap,
            context: VecDeque::new(),
            matched: None,
        }
    }

    /// Record one log line. Returns `true` the first time a line contains the
    /// marker (the matched line is then available via [`MarkerScan::matched`]).
    /// Later matching lines are still buffered for context but never replace the
    /// first match, so the reported line is deterministic.
    pub fn push(&mut self, line: &str) -> bool {
        let first_hit = self.matched.is_none() && line.contains(&self.marker);
        if first_hit {
            self.matched = Some(line.to_string());
        }
        if self.context_cap > 0 {
            if self.context.len() == self.context_cap {
                self.context.pop_front();
            }
            self.context.push_back(line.to_string());
        }
        first_hit
    }

    pub fn matched(&self) -> Option<&str> {
        self.matched.as_deref()
    }

    /// The buffered trailing lines, oldest first, for a failure report.
    pub fn context(&self) -> impl Iterator<Item = &str> {
        self.context.iter().map(String::as_str)
    }
}

/// Block until `name` is ready. With a `marker`, tail the console log for it
/// (the legacy signal). Without one, block on the guestd boot report via the
/// daemon `wait` verb (docs/agent-plane.md §2.1). On success prints to stdout
/// and returns `Ok`; on timeout returns an error (non-zero exit).
pub async fn run(
    socket: &Utf8Path,
    name: &str,
    marker: Option<&str>,
    timeout_secs: u64,
) -> Result<()> {
    match marker {
        Some(marker) => run_marker(socket, name, marker, timeout_secs).await,
        None => run_boot_report(socket, name, timeout_secs).await,
    }
}

/// Block on the guestd boot report (no marker). One `wait` request; the daemon
/// blocks until the report or the timeout.
async fn run_boot_report(socket: &Utf8Path, name: &str, timeout_secs: u64) -> Result<()> {
    let stream = UnixStream::connect(socket.as_str())
        .await
        .with_context(|| format!("connect hearthd socket {socket}"))?;
    let (read, mut write) = stream.into_split();
    let mut args = Map::new();
    args.insert("name".to_string(), json!(name));
    args.insert("timeout".to_string(), json!(timeout_secs));
    let req = Request::new(Ulid::new().to_string(), Verb::Wait, args);
    write
        .write_all((serde_json::to_string(&req)? + "\n").as_bytes())
        .await?;
    write.shutdown().await?;
    let mut lines = BufReader::new(read).lines();
    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow!("hearthd closed the wait connection without a response"))?;
    let resp: Response = serde_json::from_str(&line).context("parse wait response")?;
    if resp.ok {
        println!("{name} ready (boot report)");
        Ok(())
    } else {
        let detail = resp
            .error
            .map(|e| format!("{}: {}", e.code, e.message))
            .unwrap_or_else(|| "unknown hearthd error".to_string());
        bail!("wait for {name} failed: {detail}");
    }
}

async fn run_marker(
    socket: &Utf8Path,
    name: &str,
    marker: &str,
    timeout_secs: u64,
) -> Result<()> {
    if marker.is_empty() {
        bail!("--marker must not be empty");
    }
    let mut scan = MarkerScan::new(marker, CONTEXT_LINES);
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);

    // Follow the existing logs stream: send one request, then the daemon streams
    // `{"line": ...}` data frames until we disconnect (or it errors). Shutting
    // down our write half is harmless — the daemon is busy streaming and only
    // reads the next request after this one finishes, which for a follow never
    // happens; it just sees EOF.
    let stream = UnixStream::connect(socket.as_str())
        .await
        .with_context(|| format!("connect hearthd socket {socket}"))?;
    let (read, mut write) = stream.into_split();
    let mut args = Map::new();
    args.insert("name".to_string(), json!(name));
    args.insert("follow".to_string(), json!(true));
    let req = Request::new(Ulid::new().to_string(), Verb::Logs, args);
    write
        .write_all(serde_json::to_string(&req)?.as_bytes())
        .await?;
    write.write_all(b"\n").await?;
    write.shutdown().await?;

    let mut lines = BufReader::new(read).lines();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timeout_error(name, marker, timeout_secs, &scan));
        }
        match timeout(remaining, lines.next_line()).await {
            // Deadline hit mid-read.
            Err(_elapsed) => return Err(timeout_error(name, marker, timeout_secs, &scan)),
            // Connection closed before the marker appeared.
            Ok(Ok(None)) => return Err(stream_ended_error(name, marker, &scan)),
            Ok(Err(err)) => return Err(anyhow::Error::new(err).context("read hearthd log stream")),
            Ok(Ok(Some(line))) => {
                let resp: Response =
                    serde_json::from_str(&line).context("parse hearthd log stream response")?;
                if !resp.ok {
                    let detail = resp
                        .error
                        .map(|e| format!("{}: {}", e.code, e.message))
                        .unwrap_or_else(|| "unknown hearthd error".to_string());
                    bail!("hearthd refused the log stream for {name}: {detail}");
                }
                if resp.stream == Some(StreamKind::End) {
                    return Err(stream_ended_error(name, marker, &scan));
                }
                if let Some(text) = resp
                    .result
                    .as_ref()
                    .and_then(|value| value.get("line"))
                    .and_then(Value::as_str)
                {
                    if scan.push(text) {
                        // Print the matched line to stdout so scripts can capture it.
                        println!("{}", scan.matched().unwrap_or(text));
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn timeout_error(name: &str, marker: &str, secs: u64, scan: &MarkerScan) -> anyhow::Error {
    eprint_context(name, scan);
    anyhow!("timed out after {secs}s waiting for marker {marker:?} in {name} logs")
}

fn stream_ended_error(name: &str, marker: &str, scan: &MarkerScan) -> anyhow::Error {
    eprint_context(name, scan);
    anyhow!("hearthd closed the {name} log stream before marker {marker:?} appeared")
}

fn eprint_context(name: &str, scan: &MarkerScan) {
    let tail: Vec<&str> = scan.context().collect();
    if tail.is_empty() {
        eprintln!("no log output was received from {name}");
        return;
    }
    eprintln!("last {} log line(s) from {name}:", tail.len());
    for line in tail {
        eprintln!("  {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_first_line_containing_marker() {
        let mut scan = MarkerScan::new("HERMES_PROBE ok", CONTEXT_LINES);
        assert!(!scan.push("HERMES_PROBE start"));
        assert!(scan.push("HERMES_PROBE ok port=9119 addr=10.26.8.23"));
        assert_eq!(
            scan.matched(),
            Some("HERMES_PROBE ok port=9119 addr=10.26.8.23")
        );
    }

    #[test]
    fn marker_is_a_substring_not_a_whole_line_match() {
        let mut scan = MarkerScan::new("ok boot_count=2", 4);
        assert!(!scan.push("HEARTH_AGENT_PROBE ok boot_count=1"));
        assert!(scan.push("HEARTH_AGENT_PROBE ok boot_count=2"));
        assert!(scan.matched().unwrap().contains("boot_count=2"));
    }

    #[test]
    fn first_match_is_sticky() {
        let mut scan = MarkerScan::new("ok", CONTEXT_LINES);
        assert!(scan.push("first ok"));
        // Already matched: a later matching line is not a *new* match and does
        // not replace the recorded one.
        assert!(!scan.push("second ok"));
        assert_eq!(scan.matched(), Some("first ok"));
    }

    #[test]
    fn context_keeps_only_the_last_n_lines() {
        let mut scan = MarkerScan::new("never", 3);
        for i in 0..6 {
            scan.push(&format!("line {i}"));
        }
        let tail: Vec<&str> = scan.context().collect();
        assert_eq!(tail, vec!["line 3", "line 4", "line 5"]);
        assert!(scan.matched().is_none());
    }

    #[test]
    fn zero_context_cap_buffers_nothing_but_still_matches() {
        let mut scan = MarkerScan::new("m", 0);
        assert!(!scan.push("a"));
        assert!(scan.push("m matched"));
        assert_eq!(scan.context().count(), 0);
        assert_eq!(scan.matched(), Some("m matched"));
    }
}
