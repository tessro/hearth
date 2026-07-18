//! In-process acceptance harness (docs/agent-plane.md §11–12). Boots hearthd +
//! hearth-guestd(s) + hearth-agentd in one test process, wired through the real
//! hearthd socket broker over CHV-hybrid-emulated unix sockets. A fake
//! `codex app-server` binary stands in for the CLI. Every layer except a real
//! CHV guest and a real agent CLI is exercised on production code paths.

use anyhow::{anyhow, bail, Context, Result};
use camino::Utf8PathBuf;
use clap::Parser;
use hearth_agent_proto::{
    hybrid, read_line_capped, AgentRequest, AgentVerb, Hello, AGENT_PROTOCOL_VERSION,
    MAX_LINE_BYTES,
};
use hearth_proto::{Request, Response, Verb};
use serde_json::{json, Map, Value};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use ulid::Ulid;

pub struct AgentSpec {
    pub name: String,
    pub is_agent_in_charge: bool,
}

impl AgentSpec {
    pub fn worker(name: &str) -> Self {
        Self {
            name: name.to_string(),
            is_agent_in_charge: false,
        }
    }
    pub fn boss(name: &str) -> Self {
        Self {
            name: name.to_string(),
            is_agent_in_charge: true,
        }
    }
}

pub struct HarnessOptions {
    pub agents: Vec<AgentSpec>,
    pub delegators: Vec<String>,
    pub http: Option<HttpOptions>,
    pub codex_command: Option<String>,
    /// If set, guestds also register the claude adapter pointed at this binary
    /// (Phase 5).
    pub claude_command: Option<String>,
    /// If set, guestds register the Hermes adapter pointed at this binary.
    pub hermes_command: Option<String>,
}

impl HarnessOptions {
    /// Codex-only options (the common case for phases 0–4).
    pub fn codex(agents: Vec<AgentSpec>, codex_command: &str) -> Self {
        Self {
            agents,
            delegators: vec![],
            http: None,
            codex_command: Some(codex_command.to_string()),
            claude_command: None,
            hermes_command: None,
        }
    }
}

pub struct HttpOptions {
    pub bind: String,
    pub token: String,
    pub cors_origins: Vec<String>,
}

pub struct Harness {
    pub root: Utf8PathBuf,
    pub hearthd_socket: Utf8PathBuf,
    pub agent_socket: Utf8PathBuf,
    pub vsock_dir: Utf8PathBuf,
    pub http: Option<HttpOptions>,
    /// The agentd argv, so the daemon can be stopped and restarted mid-test
    /// (Phase 4: crash-and-recover). agentd's ledger/refs live on disk, so a
    /// restart is content-preserving.
    agentd_args: Vec<String>,
    agentd_handle: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    agent_names: Vec<String>,
    _tmp: tempfile::TempDir,
}

impl Harness {
    pub async fn start(opts: HarnessOptions) -> Result<Self> {
        let agent_names = opts.agents.iter().map(|a| a.name.clone()).collect();
        let tmp = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .map_err(|_| anyhow!("non-utf8 tmp path"))?;
        let services = root.join("services");
        let images = root.join("images");
        let run = root.join("run");
        let vsock_dir = run.join("vsock");
        for dir in [&services, &images, &run, &vsock_dir] {
            tokio::fs::create_dir_all(dir).await?;
        }

        // Verb policy: this test's uid gets exactly the agentd allowlist plus
        // `wait`, so the same process both (a) exercises agentd's brokered calls
        // and (b) proves lifecycle verbs are denied (Phase 0).
        let uid = unsafe { libc::getuid() };
        let policy = root.join("verb-policy.toml");
        tokio::fs::write(
            &policy,
            format!(
                "[[peer]]\nuid = {uid}\nverbs = [\"ping\", \"version\", \"ls\", \"status\", \
                 \"wait\", \"rename\", \"agent-endpoints\", \"guest-listener\", \"guest-connect\"]\n"
            ),
        )
        .await?;

        // A guestd-declaring image manifest.
        write_image(&images, "agent-img", true).await?;
        // Recovery key so services look well-formed.
        let keys = root.join("authorized_keys");
        tokio::fs::write(&keys, "ssh-ed25519 AAAATEST harness\n").await?;

        for spec in &opts.agents {
            write_service_toml(&services, &spec.name, true, spec.is_agent_in_charge).await?;
        }

        // hearthd.
        let hearthd_socket = run.join("hearth.sock");
        let cfg = hearthd::config::Config::parse_from([
            "hearthd",
            "--socket",
            hearthd_socket.as_str(),
            "--services-dir",
            services.as_str(),
            "--allocations",
            root.join("allocations.toml").as_str(),
            "--images-dir",
            images.as_str(),
            "--disks-dir",
            root.join("disks").as_str(),
            "--snapshots-dir",
            root.join("snapshots").as_str(),
            "--run-dir",
            run.as_str(),
            "--log-dir",
            root.join("log").as_str(),
            "--authorized-keys-file",
            keys.as_str(),
            "--guest-kernel",
            root.join("kernel").as_str(),
            "--lease-file",
            root.join("leases").as_str(),
            "--dnsmasq-dropin-dir",
            root.join("dnsmasq.d").as_str(),
            "--verb-policy",
            policy.as_str(),
        ]);
        hearthd::ensure_dirs(&cfg).await?;
        let daemon = hearthd::Daemon::new(cfg, hearthd::testing::FakeHost::running());
        tokio::spawn(async move {
            let _ = daemon.serve().await;
        });
        wait_for_path(&hearthd_socket).await?;

        // guestds (one per agent VM), unix transport over the shared vsock dir.
        for spec in &opts.agents {
            let state = root.join(format!("guest-{}", spec.name));
            tokio::fs::create_dir_all(&state).await?;
            let engine = hearth_guestd::build_engine_with(
                &state,
                hearth_guestd::AdapterConfig {
                    codex_command: opts.codex_command.clone(),
                    claude_command: opts.claude_command.clone(),
                    hermes: opts
                        .hermes_command
                        .clone()
                        .map(hearth_guestd::HermesConfig::current_user),
                },
            )?;
            let transport = hearth_guestd::transport::Transport::Unix {
                dir: vsock_dir.clone(),
                vm: vm_id(&spec.name),
            };
            let name = spec.name.clone();
            tokio::spawn(async move {
                let _ = hearth_guestd::serve(
                    transport,
                    engine,
                    format!("boot-{name}"),
                    name.clone(),
                    vec![],
                )
                .await;
            });
        }

        // agentd.
        let agent_socket = run.join("agent.sock");
        let ledger_dir = root.join("ledger");
        let ref_key = root.join("ref.key");
        tokio::fs::write(&ref_key, b"e2e-ref-key-0123456789").await?;
        let mut agentd_args = vec![
            "hearth-agentd".to_string(),
            "--hearthd-socket".to_string(),
            hearthd_socket.to_string(),
            "--control-socket".to_string(),
            agent_socket.to_string(),
            "--ledger-dir".to_string(),
            ledger_dir.to_string(),
            "--ref-key-file".to_string(),
            ref_key.to_string(),
            "--delegators".to_string(),
            opts.delegators.join(","),
        ];
        match &opts.http {
            Some(http) => {
                let token_file = root.join("token");
                tokio::fs::write(&token_file, http.token.as_bytes()).await?;
                agentd_args.push("--http-bind".to_string());
                agentd_args.push(http.bind.clone());
                agentd_args.push("--token-file".to_string());
                agentd_args.push(token_file.to_string());
                agentd_args.push("--cors-origins".to_string());
                agentd_args.push(http.cors_origins.join(","));
            }
            None => {
                agentd_args.push("--no-http".to_string());
            }
        }
        let handle = spawn_agentd(&agentd_args).await?;
        wait_for_path(&agent_socket).await?;

        let http = opts.http;
        let harness = Self {
            root,
            hearthd_socket,
            agent_socket,
            vsock_dir,
            http,
            agentd_args,
            agentd_handle: std::sync::Mutex::new(Some(handle)),
            agent_names,
            _tmp: tmp,
        };

        // Wait for every guestd's task server to answer over the emulated
        // hybrid path (readiness without depending on hearthd's `wait`).
        for spec in &harness_agent_names(&harness) {
            harness.wait_guest_ready(spec).await?;
        }
        Ok(harness)
    }

    /// Stop agentd (Phase 4: crash it while a callee is awaiting_input). The
    /// ledger/outbox/dedup all live on disk, so this loses nothing.
    pub async fn stop_agentd(&self) {
        let handle = self.agentd_handle.lock().unwrap().take();
        if let Some(handle) = handle {
            handle.abort();
            let _ = handle.await;
        }
        // The control socket lingers as a file; the next start unlinks it.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    /// Restart agentd from the same argv. It re-brokers listeners, reloads the
    /// ledger, and replays whatever the callees have queued.
    pub async fn start_agentd(&self) -> Result<()> {
        let handle = spawn_agentd(&self.agentd_args).await?;
        *self.agentd_handle.lock().unwrap() = Some(handle);
        wait_for_path(&self.root.join("run").join("agent.sock")).await
    }

    /// Direct guestd ping over the CHV-hybrid emulation (host→guest, port 1027).
    pub async fn wait_guest_ready(&self, vm: &str) -> Result<()> {
        let path = self.vsock_dir.join(format!("{}.sock", vm_id(vm)));
        for _ in 0..200 {
            if let Ok(mut stream) = UnixStream::connect(path.as_str()).await {
                if hybrid::connect_handshake(&mut stream, hearth_agent_proto::PORT_GUESTD)
                    .await
                    .is_ok()
                    && guest_hello(&mut stream, "hearthctl-agent").await.is_ok()
                {
                    if let Ok(resp) = guest_verb(&mut stream, AgentVerb::Ping, Map::new()).await {
                        if resp.get("pong").and_then(Value::as_bool) == Some(true) {
                            return Ok(());
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        bail!("guestd {vm} never became ready")
    }

    /// Open a direct guestd task-verb connection (bypasses agentd) — used by
    /// tests that need to poke a guest without the host relay.
    pub async fn guest_connect(&self, vm: &str) -> Result<UnixStream> {
        let mut stream = self.guest_connect_transport_only(vm).await?;
        guest_hello(&mut stream, "hearthctl-agent").await?;
        Ok(stream)
    }

    /// Open only the CHV-hybrid transport to guestd, without the mandatory
    /// agent-plane hello. Protocol-negative tests use this to prove guestd
    /// refuses missing or incompatible first frames.
    pub async fn guest_connect_transport_only(&self, vm: &str) -> Result<UnixStream> {
        let path = self.vsock_dir.join(format!("{}.sock", vm_id(vm)));
        let mut stream = UnixStream::connect(path.as_str())
            .await
            .with_context(|| format!("connect guest {vm}"))?;
        hybrid::connect_handshake(&mut stream, hearth_agent_proto::PORT_GUESTD).await?;
        Ok(stream)
    }

    /// Connect to a guest's machine-plane verb channel (`<vm>.sock_1024`) — the
    /// agent-in-charge path (Phase 0).
    pub async fn guest_verb_channel(&self, vm: &str) -> Result<UnixStream> {
        let path = self.vsock_dir.join(format!("{}.sock_1024", vm_id(vm)));
        UnixStream::connect(path.as_str())
            .await
            .with_context(|| format!("connect {vm} verb channel"))
    }

    /// A machine-plane request to hearthd (as this test's uid).
    pub async fn hearthd(&self, verb: Verb, args: Map<String, Value>) -> Result<Response> {
        let mut stream = UnixStream::connect(self.hearthd_socket.as_str()).await?;
        let req = Request::new(Ulid::new().to_string(), verb, args);
        stream
            .write_all((serde_json::to_string(&req)? + "\n").as_bytes())
            .await?;
        stream.shutdown().await?;
        let mut lines = BufReader::new(stream).lines();
        let line = lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("hearthd closed"))?;
        Ok(serde_json::from_str(&line)?)
    }

    /// An agent-plane request to agentd's control socket.
    pub async fn agent(&self, verb: AgentVerb, args: Map<String, Value>) -> Result<Value> {
        let mut stream = UnixStream::connect(self.agent_socket.as_str()).await?;
        let req = AgentRequest::new(Ulid::new().to_string(), verb, args);
        stream
            .write_all((serde_json::to_string(&req)? + "\n").as_bytes())
            .await?;
        stream.shutdown().await?;
        let mut lines = BufReader::new(stream).lines();
        let line = lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("agentd closed"))?;
        let resp: Response = serde_json::from_str(&line)?;
        if resp.ok {
            Ok(resp.result.unwrap_or(Value::Null))
        } else {
            let err = resp.error.unwrap();
            bail!("{}: {}", err.code, err.message)
        }
    }

    /// Agent-plane attach over the control socket, collecting streamed frames
    /// until stream end.
    pub async fn agent_attach(&self, task_ref: &str, cursor: Option<&str>) -> Result<Vec<Value>> {
        let mut stream = UnixStream::connect(self.agent_socket.as_str()).await?;
        let mut args = Map::new();
        args.insert("task_ref".to_string(), json!(task_ref));
        if let Some(cursor) = cursor {
            args.insert("cursor".to_string(), json!(cursor));
        }
        let req = AgentRequest::new(Ulid::new().to_string(), AgentVerb::TaskAttach, args);
        stream
            .write_all((serde_json::to_string(&req)? + "\n").as_bytes())
            .await?;
        stream.shutdown().await?;
        let mut lines = BufReader::new(stream).lines();
        let mut frames = Vec::new();
        while let Some(line) = lines.next_line().await? {
            let resp: Response = serde_json::from_str(&line)?;
            if !resp.ok {
                let err = resp.error.unwrap();
                bail!("attach error {}: {}", err.code, err.message);
            }
            if resp.stream == Some(hearth_proto::StreamKind::End) {
                break;
            }
            if let Some(frame) = resp.result {
                frames.push(frame);
            }
        }
        Ok(frames)
    }
}

fn harness_agent_names(h: &Harness) -> Vec<String> {
    let mut names = h.agent_names.clone();
    names.sort();
    names
}

pub fn vm_id(hostname: &str) -> String {
    fn fnv(seed: u64, bytes: &[u8]) -> u64 {
        bytes.iter().fold(seed, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        })
    }
    format!(
        "vm-{:016x}{:016x}",
        fnv(0xcbf29ce484222325, hostname.as_bytes()),
        fnv(0x84222325cbf29ce4, hostname.as_bytes())
    )
}

/// One request/response on a guest task-verb stream (after CONNECT handshake).
pub async fn guest_verb(
    stream: &mut UnixStream,
    verb: AgentVerb,
    args: Map<String, Value>,
) -> Result<Value> {
    let req = AgentRequest::new(Ulid::new().to_string(), verb, args);
    stream
        .write_all((serde_json::to_string(&req)? + "\n").as_bytes())
        .await?;
    let line = read_line_capped(stream, MAX_LINE_BYTES)
        .await?
        .ok_or_else(|| anyhow!("guest closed"))?;
    let resp: Response = serde_json::from_str(&line)?;
    if resp.ok {
        Ok(resp.result.unwrap_or(Value::Null))
    } else {
        let err = resp.error.unwrap();
        bail!("{}: {}", err.code, err.message)
    }
}

/// A machine-plane request on a guest's `<vm>.sock_1024` verb channel.
pub async fn machine_verb(
    stream: &mut UnixStream,
    verb: Verb,
    args: Map<String, Value>,
) -> Result<Response> {
    let req = Request::new(Ulid::new().to_string(), verb, args);
    stream
        .write_all((serde_json::to_string(&req)? + "\n").as_bytes())
        .await?;
    let line = read_line_capped(stream, MAX_LINE_BYTES)
        .await?
        .ok_or_else(|| anyhow!("guest verb channel closed"))?;
    Ok(serde_json::from_str(&line)?)
}

/// The hello agentd/hearthctl send on the guest verb/mcp channels. Provided so
/// tests can emulate a shim if needed.
pub fn hello(component: &str) -> Hello {
    Hello::new(component, "test")
}

async fn guest_hello(stream: &mut UnixStream, component: &str) -> Result<()> {
    stream
        .write_all((serde_json::to_string(&hello(component))? + "\n").as_bytes())
        .await?;
    let line = read_line_capped(stream, MAX_LINE_BYTES)
        .await?
        .ok_or_else(|| anyhow!("guest closed during hello"))?;
    let response: Response = serde_json::from_str(&line)?;
    if !response.ok {
        let err = response
            .error
            .map(|err| format!("{}: {}", err.code, err.message))
            .unwrap_or_else(|| "guest rejected hello".to_string());
        bail!("{err}");
    }
    let proto = response
        .result
        .as_ref()
        .and_then(|result| result.get("proto"))
        .and_then(Value::as_u64);
    if proto != Some(u64::from(AGENT_PROTOCOL_VERSION)) {
        bail!("guest returned incompatible protocol {proto:?}");
    }
    Ok(())
}

async fn write_service_toml(
    services: &Utf8PathBuf,
    name: &str,
    agent: bool,
    is_agent_in_charge: bool,
) -> Result<()> {
    let id = vm_id(name);
    let toml = format!(
        r#"id = "{id}"
hostname = "{name}"
enabled = true
image = "agent-img"
cpu = 2
memory_mib = 2048
disk_gib = 20
vsock_cid = {cid}
mac = "52:54:00:00:00:{mac:02x}"
is_agent_in_charge = {is_agent_in_charge}
agent = {agent}
"#,
        cid = 100 + (name.len() as u32),
        mac = name.bytes().next().unwrap_or(1),
    );
    tokio::fs::write(services.join(format!("{id}.toml")), toml).await?;
    Ok(())
}

async fn write_image(images: &Utf8PathBuf, name: &str, guestd: bool) -> Result<()> {
    tokio::fs::write(images.join(format!("{name}.qcow2")), b"fake").await?;
    let manifest = format!(
        r#"version = 1
root_device = "/dev/vda"
root_fstype = "ext4"
init = "/usr/local/bin/init"
guestd = {guestd}

[oci]
args = ["/usr/local/bin/init"]
"#
    );
    tokio::fs::write(images.join(format!("{name}.hearth.toml")), manifest).await?;
    Ok(())
}

async fn wait_for_path(path: &Utf8PathBuf) -> Result<()> {
    for _ in 0..400 {
        if path.exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    bail!("path never appeared: {path}")
}

async fn spawn_agentd(args: &[String]) -> Result<tokio::task::JoinHandle<()>> {
    let cfg = hearth_agentd::config::Config::parse_from(args);
    let agentd = hearth_agentd::build(cfg).await?;
    Ok(tokio::spawn(async move {
        let _ = hearth_agentd::run(agentd).await;
    }))
}

/// A minimal MCP client speaking the same stdio JSON-RPC framing a guest shim
/// would splice, but connecting to agentd's brokered listener directly (the
/// shim is a dumb pipe, §2.4). Used to drive delegation as a given VM.
pub struct McpClient {
    stream: UnixStream,
    next_id: u64,
}

impl McpClient {
    /// Connect as `vm`'s shim: dial the emulated host port 1026 and send the
    /// MCP-channel hello.
    pub async fn connect(h: &Harness, vm: &str, thread_id: &str) -> Result<Self> {
        let path = h.vsock_dir.join(format!("{}.sock_1026", vm_id(vm)));
        // The broker may not have bound the listener yet; retry briefly.
        let mut stream = None;
        for _ in 0..200 {
            if let Ok(s) = UnixStream::connect(path.as_str()).await {
                stream = Some(s);
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let mut stream = stream.ok_or_else(|| anyhow!("mcp listener for {vm} never appeared"))?;
        let mut hello = Hello::new("mcp-shim", "test");
        hello.channel = Some(hearth_agent_proto::HelloChannel::Mcp);
        hello.thread_id = Some(thread_id.to_string());
        stream
            .write_all((serde_json::to_string(&hello)? + "\n").as_bytes())
            .await?;
        let mut client = Self { stream, next_id: 1 };
        client.rpc("initialize", json!({})).await?;
        Ok(client)
    }

    async fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.stream
            .write_all((serde_json::to_string(&msg)? + "\n").as_bytes())
            .await?;
        let line = read_line_capped(&mut self.stream, MAX_LINE_BYTES)
            .await?
            .ok_or_else(|| anyhow!("mcp server closed"))?;
        let resp: Value = serde_json::from_str(&line)?;
        if let Some(err) = resp.get("error") {
            bail!("mcp error: {err}");
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Call a tool; returns the parsed JSON text content (tools return a single
    /// text block carrying JSON).
    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        let result = self
            .rpc(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await?;
        let text = result["content"][0]["text"]
            .as_str()
            .ok_or_else(|| anyhow!("tool result had no text content: {result}"))?;
        serde_json::from_str(text).context("parse tool result json")
    }
}

/// POST a RunAgentInput to the AG-UI endpoint and collect the streamed
/// `data:` SSE events as parsed JSON (what an AG-UI HttpAgent consumes).
pub async fn agui_post(
    bind: &str,
    token: &str,
    agent_vm: &str,
    body: &Value,
) -> Result<Vec<Value>> {
    let path = format!("/v1/agents/{agent_vm}/agui");
    http_sse(bind, "POST", &path, Some(token), Some(body)).await
}

/// GET an SSE endpoint and collect its `data:` events.
pub async fn http_sse(
    bind: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> Result<Vec<Value>> {
    let mut stream = tokio::net::TcpStream::connect(bind).await?;
    write_http(&mut stream, method, path, token, body, None).await?;
    let mut reader = BufReader::new(stream);
    // Skip the status + headers.
    let mut status_line = String::new();
    reader.read_line(&mut status_line).await?;
    if !status_line.contains(" 200 ") {
        bail!("SSE endpoint returned: {}", status_line.trim());
    }
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).await?;
        if header == "\r\n" || header == "\n" || header.is_empty() {
            break;
        }
    }
    let mut events = Vec::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        let line = line.trim_end();
        if line == "event: done" {
            // Read its data line and stop.
            let mut _data = String::new();
            let _ = reader.read_line(&mut _data).await;
            break;
        }
        if let Some(data) = line.strip_prefix("data: ") {
            if let Ok(value) = serde_json::from_str::<Value>(data) {
                events.push(value);
            }
        }
    }
    Ok(events)
}

/// A plain JSON GET/POST returning (status, body).
pub async fn http_json(
    bind: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
    origin: Option<&str>,
) -> Result<(u16, Value, Vec<(String, String)>)> {
    let mut stream = tokio::net::TcpStream::connect(bind).await?;
    write_http(&mut stream, method, path, token, body, origin).await?;
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).await?;
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| anyhow!("bad status line {status_line:?}"))?;
    let mut headers = Vec::new();
    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).await?;
        if header == "\r\n" || header == "\n" || header.is_empty() {
            break;
        }
        if let Some((k, v)) = header.split_once(':') {
            let (k, v) = (k.trim().to_string(), v.trim().to_string());
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.parse().unwrap_or(0);
            }
            headers.push((k, v));
        }
    }
    let mut buf = vec![0u8; content_length];
    if content_length > 0 {
        use tokio::io::AsyncReadExt;
        reader.read_exact(&mut buf).await?;
    }
    let value = serde_json::from_slice(&buf).unwrap_or(Value::Null);
    Ok((code, value, headers))
}

async fn write_http(
    stream: &mut tokio::net::TcpStream,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
    origin: Option<&str>,
) -> Result<()> {
    let body_bytes = body
        .map(|b| serde_json::to_vec(b).unwrap())
        .unwrap_or_default();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n");
    if let Some(token) = token {
        req.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if let Some(origin) = origin {
        req.push_str(&format!("Origin: {origin}\r\n"));
    }
    req.push_str("Content-Type: application/json\r\n");
    req.push_str(&format!("Content-Length: {}\r\n", body_bytes.len()));
    req.push_str("Connection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await?;
    stream.write_all(&body_bytes).await?;
    stream.flush().await?;
    Ok(())
}
