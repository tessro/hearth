use anyhow::{anyhow, Result};
use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};
use comfy_table::{presets::UTF8_FULL, Table};
use hearth_proto::{empty_args, Request, Response, StreamKind, Verb};
use serde_json::{json, Map, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use ulid::Ulid;

mod client;
mod image_build;
mod image_lint;
mod oci;
mod spawn;
mod wait;

#[derive(Debug, Parser)]
#[command(name = "hearthctl", version, about = "Operate hearthd")]
struct Cli {
    #[arg(
        long,
        global = true,
        env = "HEARTH_SOCKET",
        default_value = "/run/hearth.sock"
    )]
    socket: Utf8PathBuf,
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Ping,
    Version,
    Ls,
    Status {
        name: String,
    },
    Create {
        name: String,
        #[arg(long = "from")]
        image: Option<String>,
        #[arg(long)]
        cpu: Option<u32>,
        #[arg(long = "mem")]
        memory_mib: Option<u64>,
        #[arg(long = "disk")]
        disk_gib: Option<u64>,
        #[arg(long)]
        ssh_key: Vec<String>,
        #[arg(long = "authorized-keys-file")]
        authorized_keys_file: Vec<Utf8PathBuf>,
        #[arg(long)]
        agent_in_charge: bool,
    },
    /// Build (if needed), create, and start one VM from a template in a single
    /// command. Repeatable flags provision each VM independently.
    Spawn {
        name: String,
        /// Template image the VM is created from (required).
        #[arg(long)]
        image: String,
        /// Build this Dockerfile into `--image` first, but only if the image is
        /// not already on the daemon.
        #[arg(long)]
        dockerfile: Option<Utf8PathBuf>,
        /// Build context directory (only used on the build-if-missing path).
        #[arg(long, default_value = ".")]
        context: Utf8PathBuf,
        /// Root-disk size for the build-if-missing path, in GiB.
        #[arg(long = "build-disk", default_value_t = 20)]
        build_disk_gib: u64,
        /// Network namespace for build RUN steps (build-if-missing path).
        #[arg(long = "build-network", value_enum, default_value_t = oci::BuildNetwork::Host)]
        build_network: oci::BuildNetwork,
        /// Build argument forwarded verbatim as `--build-arg KEY=VALUE`
        /// (build-if-missing path). Repeatable.
        #[arg(long = "build-arg")]
        build_arg: Vec<String>,
        /// Provision a local file into the VM: `source=<path>,dest=<abs>[,mode=<octal>][,owner=<uid:gid>]`.
        /// Repeatable. `source` is read client-side and sent as literal content.
        /// mode/owner default to 0644/0:0; pass mode=0600 for secrets. Fields are
        /// comma-separated, so a `source` path may contain `=` but not a comma.
        #[arg(long = "provision-file")]
        provision_file: Vec<String>,
        /// VM hostname (default: the service name).
        #[arg(long)]
        hostname: Option<String>,
        #[arg(long)]
        cpu: Option<u32>,
        #[arg(long = "mem")]
        memory_mib: Option<u64>,
        #[arg(long = "disk")]
        disk_gib: Option<u64>,
        /// Publish a guest port: `<host>:<guest>[/tcp|udp][@bind]`. Repeatable.
        #[arg(long)]
        publish: Vec<String>,
        /// Delete the image's baked SSH host keys so this VM regenerates a
        /// unique set on first boot. Needed for images that bake `ssh_host_*`
        /// keys (e.g. a base whose openssh install ran `ssh-keygen -A` and does
        /// not `rm` them); vm-base already removes them, so this is a no-op there.
        #[arg(long = "reset-ssh-hostkeys")]
        reset_ssh_hostkeys: bool,
        /// Start the VM after creating it (the default).
        #[arg(long, overrides_with = "no_start")]
        start: bool,
        /// Create the VM but do not start it.
        #[arg(long = "no-start", overrides_with = "start")]
        no_start: bool,
    },
    Destroy {
        name: String,
    },
    Start {
        name: String,
    },
    Stop {
        name: String,
    },
    Restart {
        name: String,
    },
    Reboot {
        name: String,
    },
    Snapshot {
        name: String,
        #[arg(long)]
        tag: Option<String>,
    },
    Restore {
        name: String,
        #[arg(long)]
        tag: String,
    },
    Resize {
        name: String,
        #[arg(long)]
        cpu: Option<u32>,
        #[arg(long = "mem")]
        memory_mib: Option<u64>,
    },
    Logs {
        name: String,
        #[arg(long)]
        follow: bool,
    },
    /// Block until a marker substring appears in a service's console log, then
    /// exit 0 printing the matched line; exit non-zero on timeout. Client-side
    /// tail of the `logs --follow` stream — replaces the scripts' wait_for_log().
    Wait {
        name: String,
        /// Substring to wait for on any console line (e.g. `HERMES_PROBE ok`).
        #[arg(long)]
        marker: String,
        /// Give up after this many seconds.
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
    Image {
        #[command(subcommand)]
        command: ImageCommand,
    },
    Host {
        #[command(subcommand)]
        command: HostCommand,
    },
    /// Manage a running service's host->guest port forwards. Changes apply live
    /// (the nftables table is re-applied); the VM is not restarted.
    Publish {
        #[command(subcommand)]
        command: PublishCommand,
    },
}

#[derive(Debug, Subcommand)]
enum PublishCommand {
    /// Add a named port forward and apply it live.
    Add {
        /// Service to publish from.
        service: String,
        /// Unique name for this forward (use it with `publish rm`).
        name: String,
        /// Forward spec: host:guest[/tcp|udp][@bind].
        spec: String,
    },
    /// Remove a named port forward and apply the change live.
    Rm {
        /// Service the forward belongs to.
        service: String,
        /// Name of the forward to remove (see `publish ls`).
        name: String,
    },
    /// List a service's port forwards.
    Ls { service: String },
}

#[derive(Debug, Subcommand)]
enum ImageCommand {
    Ls,
    Build {
        #[arg(long)]
        name: String,
        #[arg(long)]
        dockerfile: Utf8PathBuf,
        #[arg(long, default_value = ".")]
        context: Utf8PathBuf,
        #[arg(long = "disk", default_value_t = 20)]
        disk_gib: u64,
        #[arg(long)]
        rootless: bool,
        /// Network namespace for RUN steps. Defaults to `host`: netavark races
        /// its own iptables chains between consecutive RUN steps ("Chain
        /// already exists") on this host config as of buildah 1.43, so `host`
        /// is used until that is fixed or a multi-user host needs isolation.
        #[arg(long = "build-network", value_enum, default_value_t = oci::BuildNetwork::Host)]
        build_network: oci::BuildNetwork,
        /// Build argument forwarded verbatim to buildah as `--build-arg
        /// KEY=VALUE`. Repeatable; must contain `=`.
        #[arg(long = "build-arg")]
        build_arg: Vec<String>,
        /// Skip the build-time rootfs linter. Use only for images that boot
        /// something other than systemd, whose contract the linter models.
        #[arg(long = "skip-lint")]
        skip_lint: bool,
    },
    Pull {
        url: String,
        #[arg(long)]
        name: Option<String>,
    },
    Rm {
        name: String,
    },
}

#[derive(Debug, Subcommand)]
enum HostCommand {
    Check,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Command::Image {
        command:
            ImageCommand::Build {
                name,
                dockerfile,
                context,
                disk_gib,
                rootless,
                build_network,
                build_arg,
                skip_lint,
            },
    } = &cli.command
    {
        return image_build::build(image_build::BuildOptions {
            name: name.clone(),
            dockerfile: dockerfile.clone(),
            context: context.clone(),
            disk_gib: *disk_gib,
            rootless: *rootless,
            network: *build_network,
            build_args: build_arg.clone(),
            skip_lint: *skip_lint,
            socket: cli.socket.clone(),
        })
        .await;
    }

    if let Command::Spawn {
        name,
        image,
        dockerfile,
        context,
        build_disk_gib,
        build_network,
        build_arg,
        provision_file,
        hostname,
        cpu,
        memory_mib,
        disk_gib,
        publish,
        reset_ssh_hostkeys,
        no_start,
        // `start` only exists so `--start` can override a preceding `--no-start`
        // (last flag wins); the effective decision is `!no_start`.
        start: _,
    } = &cli.command
    {
        return spawn::run(
            &cli.socket,
            spawn::SpawnOptions {
                name: name.clone(),
                image: image.clone(),
                dockerfile: dockerfile.clone(),
                context: context.clone(),
                build_disk_gib: *build_disk_gib,
                build_network: *build_network,
                build_args: build_arg.clone(),
                provision_file: provision_file.clone(),
                hostname: hostname.clone(),
                cpu: *cpu,
                memory_mib: *memory_mib,
                disk_gib: *disk_gib,
                publish: publish.clone(),
                reset_ssh_hostkeys: *reset_ssh_hostkeys,
                start: !no_start,
            },
        )
        .await;
    }

    if let Command::Wait {
        name,
        marker,
        timeout,
    } = &cli.command
    {
        return wait::run(&cli.socket, name, marker, *timeout).await;
    }

    let (verb, args) = to_request(&cli.command)?;
    let req = Request::new(Ulid::new().to_string(), verb, args);
    let responses = round_trip(&cli.socket, &req).await?;
    if cli.json {
        for response in &responses {
            println!("{}", serde_json::to_string(response)?);
        }
        return Ok(());
    }
    // Upgrade an unknown-verb serde failure into a stale-daemon message before it
    // reaches a human. Shared with the image-build path in client.rs.
    if let Some(err) = responses.first().and_then(|resp| resp.error.as_ref()) {
        if let Some(hint) =
            client::stale_daemon_hint(&cli.socket, &req.verb, &err.code, &err.message).await
        {
            return Err(anyhow!("{hint}"));
        }
    }
    render(&cli.command, responses)
}

fn read_authorized_keys(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

fn to_request(command: &Command) -> Result<(Verb, Map<String, Value>)> {
    Ok(match command {
        Command::Ping => (Verb::Ping, empty_args()),
        Command::Version => (Verb::Version, empty_args()),
        Command::Ls => (Verb::Ls, empty_args()),
        Command::Status { name } => (Verb::Status, args([("name", json!(name))])),
        Command::Create {
            name,
            image,
            cpu,
            memory_mib,
            disk_gib,
            ssh_key,
            authorized_keys_file,
            agent_in_charge,
        } => {
            let mut args = args([("name", json!(name))]);
            insert_opt(&mut args, "image", image.as_ref().map(|v| json!(v)));
            insert_opt(&mut args, "cpu", cpu.map(|v| json!(v)));
            insert_opt(&mut args, "memory_mib", memory_mib.map(|v| json!(v)));
            insert_opt(&mut args, "disk_gib", disk_gib.map(|v| json!(v)));
            let mut keys: Vec<String> = ssh_key.clone();
            for path in authorized_keys_file {
                let text =
                    std::fs::read_to_string(path).map_err(|e| anyhow!("read {path}: {e}"))?;
                keys.extend(read_authorized_keys(&text));
            }
            if !keys.is_empty() {
                args.insert("ssh_keys".into(), json!(keys));
            }
            if *agent_in_charge {
                args.insert("is_agent_in_charge".into(), json!(true));
            }
            (Verb::Create, args)
        }
        Command::Spawn { .. } => return Err(anyhow!("spawn is handled locally")),
        Command::Destroy { name } => (Verb::Destroy, args([("name", json!(name))])),
        Command::Start { name } => (Verb::Start, args([("name", json!(name))])),
        Command::Stop { name } => (Verb::Stop, args([("name", json!(name))])),
        Command::Restart { name } => (Verb::Restart, args([("name", json!(name))])),
        Command::Reboot { name } => (Verb::Reboot, args([("name", json!(name))])),
        Command::Snapshot { name, tag } => {
            let mut args = args([("name", json!(name))]);
            insert_opt(&mut args, "tag", tag.as_ref().map(|v| json!(v)));
            (Verb::Snapshot, args)
        }
        Command::Restore { name, tag } => (
            Verb::Restore,
            args([("name", json!(name)), ("tag", json!(tag))]),
        ),
        Command::Resize {
            name,
            cpu,
            memory_mib,
        } => {
            let mut args = args([("name", json!(name))]);
            insert_opt(&mut args, "cpu", cpu.map(|v| json!(v)));
            insert_opt(&mut args, "memory_mib", memory_mib.map(|v| json!(v)));
            (Verb::Resize, args)
        }
        Command::Logs { name, follow } => (
            Verb::Logs,
            args([("name", json!(name)), ("follow", json!(follow))]),
        ),
        Command::Wait { .. } => return Err(anyhow!("wait is handled locally")),
        Command::Image { command } => match command {
            ImageCommand::Ls => (Verb::ImageLs, empty_args()),
            ImageCommand::Build { .. } => return Err(anyhow!("image build is handled locally")),
            ImageCommand::Pull { url, name } => {
                let mut args = args([("url", json!(url))]);
                insert_opt(&mut args, "name", name.as_ref().map(|v| json!(v)));
                (Verb::ImagePull, args)
            }
            ImageCommand::Rm { name } => (Verb::ImageRm, args([("name", json!(name))])),
        },
        Command::Host { command } => match command {
            HostCommand::Check => (Verb::HostCheck, empty_args()),
        },
        Command::Publish { command } => match command {
            PublishCommand::Add {
                service,
                name,
                spec,
            } => {
                let parsed = spawn::parse_publish(spec)?;
                let mut publish = Map::new();
                publish.insert("name".to_string(), json!(name));
                publish.insert("host_port".to_string(), json!(parsed.host_port));
                publish.insert("guest_port".to_string(), json!(parsed.guest_port));
                publish.insert("protocol".to_string(), json!(parsed.protocol));
                if let Some(bind) = parsed.bind {
                    publish.insert("bind".to_string(), json!(bind));
                }
                (
                    Verb::Publish,
                    args([
                        ("name", json!(service)),
                        ("publish", Value::Object(publish)),
                    ]),
                )
            }
            PublishCommand::Rm { service, name } => (
                Verb::Unpublish,
                args([("name", json!(service)), ("publish_name", json!(name))]),
            ),
            PublishCommand::Ls { service } => (Verb::Status, args([("name", json!(service))])),
        },
    })
}

async fn round_trip(socket: &Utf8PathBuf, req: &Request) -> Result<Vec<Response>> {
    let stream = UnixStream::connect(socket.as_str()).await?;
    let (read, mut write) = stream.into_split();
    write
        .write_all(serde_json::to_string(req)?.as_bytes())
        .await?;
    write.write_all(b"\n").await?;
    write.shutdown().await?;
    let mut lines = BufReader::new(read).lines();
    let mut responses = Vec::new();
    while let Some(line) = lines.next_line().await? {
        let response: Response = serde_json::from_str(&line)?;
        let done = response.stream.is_none() || response.stream == Some(StreamKind::End);
        let failed = !response.ok;
        responses.push(response);
        if done || failed {
            break;
        }
    }
    Ok(responses)
}

fn render(command: &Command, responses: Vec<Response>) -> Result<()> {
    let first = responses
        .first()
        .ok_or_else(|| anyhow!("no response from hearthd"))?;
    if !first.ok {
        let err = first
            .error
            .as_ref()
            .ok_or_else(|| anyhow!("unknown error"))?;
        return Err(anyhow!("{}: {}", err.code, err.message));
    }
    match command {
        Command::Ping => println!("{}", format_pong(first.result.as_ref())),
        Command::Ls => render_ls(first.result.as_ref())?,
        Command::Image {
            command: ImageCommand::Ls,
        } => render_images(first.result.as_ref())?,
        Command::Host {
            command: HostCommand::Check,
        } => render_checks(first.result.as_ref())?,
        Command::Publish { .. } => render_publishes(first.result.as_ref())?,
        Command::Logs { .. } => {
            for response in responses {
                if response.stream == Some(StreamKind::Data) {
                    if let Some(line) = response
                        .result
                        .as_ref()
                        .and_then(|v| v.get("line"))
                        .and_then(Value::as_str)
                    {
                        println!("{line}");
                    }
                }
            }
        }
        _ => {
            if let Some(result) = &first.result {
                println!("{}", serde_json::to_string_pretty(result)?);
            }
        }
    }
    Ok(())
}

fn render_ls(result: Option<&Value>) -> Result<()> {
    let services = result
        .and_then(|v| v.get("services"))
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("malformed ls response"))?;
    let mut table = Table::new();
    table.load_preset(UTF8_FULL);
    table.set_header([
        "NAME", "ENABLED", "RUNNING", "IMAGE", "CPU", "MEM", "CID", "ADDRESS",
    ]);
    for svc in services {
        table.add_row([
            cell(svc, "name"),
            cell(svc, "enabled"),
            cell(svc, "running"),
            cell(svc, "image"),
            cell(svc, "cpu"),
            cell(svc, "memory_mib"),
            cell(svc, "vsock_cid"),
            address_cell(svc),
        ]);
    }
    println!("{table}");
    Ok(())
}

fn render_images(result: Option<&Value>) -> Result<()> {
    let images = result
        .and_then(|v| v.get("images"))
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("malformed image ls response"))?;
    let mut table = Table::new();
    table.load_preset(UTF8_FULL);
    table.set_header(["NAME", "KIND", "BYTES", "SHA256"]);
    for image in images {
        table.add_row([
            cell(image, "name"),
            cell(image, "kind"),
            cell(image, "bytes"),
            cell(image, "sha256"),
        ]);
    }
    println!("{table}");
    Ok(())
}

fn render_publishes(result: Option<&Value>) -> Result<()> {
    let service = result
        .and_then(|v| v.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let empty = Vec::new();
    let publishes = result
        .and_then(|v| v.get("publish"))
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    if publishes.is_empty() {
        println!("{service} has no published ports");
        return Ok(());
    }
    let mut table = Table::new();
    table.load_preset(UTF8_FULL);
    table.set_header(["NAME", "PROTO", "HOST", "GUEST", "BIND"]);
    for p in publishes {
        let host = p.get("host_port").and_then(Value::as_u64).unwrap_or(0);
        let proto = p
            .get("protocol")
            .and_then(Value::as_str)
            .unwrap_or("tcp")
            .to_string();
        // Mirror Publish::effective_name so unnamed forwards still show a handle.
        let name = p
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("{host}-{proto}"));
        table.add_row([
            name,
            proto,
            host.to_string(),
            cell(p, "guest_port"),
            p.get("bind")
                .and_then(Value::as_str)
                .unwrap_or("*")
                .to_string(),
        ]);
    }
    println!("{table}");
    Ok(())
}

fn render_checks(result: Option<&Value>) -> Result<()> {
    let checks = result
        .and_then(|v| v.get("checks"))
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("malformed host check response"))?;
    let mut table = Table::new();
    table.load_preset(UTF8_FULL);
    table.set_header(["CHECK", "OK", "PATH"]);
    for check in checks {
        table.add_row([cell(check, "name"), cell(check, "ok"), cell(check, "path")]);
    }
    println!("{table}");
    Ok(())
}

/// Render `ping` so the operator always knows which daemon answered. Falls back
/// to a bare "pong" when talking to an older daemon that omits version/pid.
fn format_pong(result: Option<&Value>) -> String {
    let version = result
        .and_then(|value| value.get("version"))
        .and_then(Value::as_str);
    let pid = result
        .and_then(|value| value.get("pid"))
        .and_then(Value::as_u64);
    match (version, pid) {
        (Some(version), Some(pid)) => format!("pong — hearthd {version} (pid {pid})"),
        _ => "pong".to_string(),
    }
}

fn cell(value: &Value, key: &str) -> String {
    value
        .get(key)
        .map(|v| {
            v.as_str()
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| v.to_string())
        })
        .unwrap_or_default()
}

/// Render a service's `address`, which is JSON null until a lease or static
/// reservation exists. A bare "-" reads better in the table than "null".
fn address_cell(value: &Value) -> String {
    match value.get("address").and_then(Value::as_str) {
        Some(addr) => addr.to_string(),
        None => "-".to_string(),
    }
}

fn args<const N: usize>(items: [(&str, Value); N]) -> Map<String, Value> {
    items
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

fn insert_opt(args: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        args.insert(key.to_string(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pong_includes_daemon_version_and_pid_when_present() {
        let result = json!({ "pong": true, "version": "0.1.0", "pid": 4321 });
        assert_eq!(
            format_pong(Some(&result)),
            "pong — hearthd 0.1.0 (pid 4321)"
        );
    }

    #[test]
    fn pong_falls_back_when_daemon_omits_version_or_pid() {
        assert_eq!(format_pong(Some(&json!({ "pong": true }))), "pong");
        assert_eq!(format_pong(None), "pong");
    }

    #[test]
    fn image_ls_maps_to_protocol_verb() {
        let (verb, args) = to_request(&Command::Image {
            command: ImageCommand::Ls,
        })
        .unwrap();
        assert_eq!(verb, Verb::ImageLs);
        assert!(args.is_empty());
    }

    #[test]
    fn image_pull_maps_url_and_optional_name() {
        let (verb, args) = to_request(&Command::Image {
            command: ImageCommand::Pull {
                url: "https://example.invalid/debian.qcow2".to_string(),
                name: Some("debian".to_string()),
            },
        })
        .unwrap();
        assert_eq!(verb, Verb::ImagePull);
        assert_eq!(
            args.get("url"),
            Some(&json!("https://example.invalid/debian.qcow2"))
        );
        assert_eq!(args.get("name"), Some(&json!("debian")));
    }

    #[test]
    fn image_build_is_local_only() {
        let err = to_request(&Command::Image {
            command: ImageCommand::Build {
                name: "exeuntu".to_string(),
                dockerfile: Utf8PathBuf::from("./Dockerfile"),
                context: Utf8PathBuf::from("."),
                disk_gib: 40,
                rootless: false,
                build_network: oci::BuildNetwork::Host,
                build_arg: vec![],
                skip_lint: false,
            },
        })
        .unwrap_err();

        assert!(err.to_string().contains("handled locally"));
    }

    #[test]
    fn image_rm_maps_name() {
        let (verb, args) = to_request(&Command::Image {
            command: ImageCommand::Rm {
                name: "debian".to_string(),
            },
        })
        .unwrap();
        assert_eq!(verb, Verb::ImageRm);
        assert_eq!(args.get("name"), Some(&json!("debian")));
    }

    #[test]
    fn publish_add_maps_spec_and_name_to_publish_verb() {
        let (verb, args) = to_request(&Command::Publish {
            command: PublishCommand::Add {
                service: "web".to_string(),
                name: "dashboard".to_string(),
                spec: "8080:80/tcp@100.121.19.41".to_string(),
            },
        })
        .unwrap();
        assert_eq!(verb, Verb::Publish);
        assert_eq!(args.get("name"), Some(&json!("web")));
        let p = args.get("publish").unwrap();
        assert_eq!(p["name"], json!("dashboard"));
        assert_eq!(p["host_port"], json!(8080));
        assert_eq!(p["guest_port"], json!(80));
        assert_eq!(p["protocol"], json!("tcp"));
        assert_eq!(p["bind"], json!("100.121.19.41"));
    }

    #[test]
    fn publish_rm_and_ls_map_to_verbs() {
        let (verb, args) = to_request(&Command::Publish {
            command: PublishCommand::Rm {
                service: "web".to_string(),
                name: "dashboard".to_string(),
            },
        })
        .unwrap();
        assert_eq!(verb, Verb::Unpublish);
        assert_eq!(args.get("name"), Some(&json!("web")));
        assert_eq!(args.get("publish_name"), Some(&json!("dashboard")));

        let (verb, args) = to_request(&Command::Publish {
            command: PublishCommand::Ls {
                service: "web".to_string(),
            },
        })
        .unwrap();
        assert_eq!(verb, Verb::Status);
        assert_eq!(args.get("name"), Some(&json!("web")));
    }

    #[test]
    fn publish_add_requires_service_name_and_spec() {
        // Missing the spec positional is a parse error (all three are required).
        assert!(Cli::try_parse_from(["hearthctl", "publish", "add", "web", "dashboard"]).is_err());
        let cli =
            Cli::try_parse_from(["hearthctl", "publish", "add", "web", "dashboard", "8080:80"])
                .unwrap();
        match cli.command {
            Command::Publish {
                command:
                    PublishCommand::Add {
                        service,
                        name,
                        spec,
                    },
            } => {
                assert_eq!(service, "web");
                assert_eq!(name, "dashboard");
                assert_eq!(spec, "8080:80");
            }
            other => panic!("expected publish add, got {other:?}"),
        }
    }

    #[test]
    fn create_maps_resource_arguments() {
        let (verb, args) = to_request(&Command::Create {
            name: "web".to_string(),
            image: Some("debian".to_string()),
            cpu: Some(4),
            memory_mib: Some(4096),
            disk_gib: Some(30),
            ssh_key: vec!["ssh-ed25519 AAAA test".to_string()],
            authorized_keys_file: vec![],
            agent_in_charge: true,
        })
        .unwrap();
        assert_eq!(verb, Verb::Create);
        assert_eq!(args.get("name"), Some(&json!("web")));
        assert_eq!(args.get("image"), Some(&json!("debian")));
        assert_eq!(args.get("cpu"), Some(&json!(4)));
        assert_eq!(args.get("memory_mib"), Some(&json!(4096)));
        assert_eq!(args.get("disk_gib"), Some(&json!(30)));
        assert_eq!(
            args.get("ssh_keys"),
            Some(&json!(["ssh-ed25519 AAAA test"]))
        );
        assert_eq!(args.get("is_agent_in_charge"), Some(&json!(true)));
    }

    #[test]
    fn image_build_subcommand_parses_required_shape() {
        let cli = Cli::try_parse_from([
            "hearthctl",
            "image",
            "build",
            "--name",
            "exeuntu",
            "--dockerfile",
            "./Dockerfile",
            "--context",
            ".",
            "--disk",
            "40",
        ])
        .unwrap();
        match cli.command {
            Command::Image {
                command:
                    ImageCommand::Build {
                        name,
                        dockerfile,
                        context,
                        disk_gib,
                        rootless,
                        build_network,
                        build_arg,
                        skip_lint,
                    },
            } => {
                assert_eq!(name, "exeuntu");
                assert_eq!(dockerfile, Utf8PathBuf::from("./Dockerfile"));
                assert_eq!(context, Utf8PathBuf::from("."));
                assert_eq!(disk_gib, 40);
                assert!(!rootless);
                // Defaults: host network, no build args, lint on.
                assert_eq!(build_network, oci::BuildNetwork::Host);
                assert!(build_arg.is_empty());
                assert!(!skip_lint);
            }
            other => panic!("expected image build command, got {other:?}"),
        }
    }

    #[test]
    fn spawn_parses_repeatable_flags_and_defaults_to_start() {
        let cli = Cli::try_parse_from([
            "hearthctl",
            "spawn",
            "hermes-a",
            "--image",
            "hermes-vm",
            "--provision-file",
            "source=./a.env,dest=/home/agent/.hermes/.env,mode=0600,owner=1000:1000",
            "--publish",
            "9119:9119",
            "--cpu",
            "4",
            "--mem",
            "4096",
        ])
        .unwrap();
        match cli.command {
            Command::Spawn {
                name,
                image,
                provision_file,
                publish,
                cpu,
                memory_mib,
                no_start,
                dockerfile,
                ..
            } => {
                assert_eq!(name, "hermes-a");
                assert_eq!(image, "hermes-vm");
                assert_eq!(provision_file.len(), 1);
                assert_eq!(publish, vec!["9119:9119"]);
                assert_eq!(cpu, Some(4));
                assert_eq!(memory_mib, Some(4096));
                assert!(dockerfile.is_none());
                // Default is to start; --no-start absent.
                assert!(!no_start);
            }
            other => panic!("expected spawn command, got {other:?}"),
        }
    }

    #[test]
    fn spawn_start_overrides_a_preceding_no_start() {
        // Last of --no-start/--start wins, so the effective `!no_start` is true.
        let cli = Cli::try_parse_from([
            "hearthctl",
            "spawn",
            "dev",
            "--image",
            "exeuntu",
            "--no-start",
            "--start",
        ])
        .unwrap();
        match cli.command {
            Command::Spawn { no_start, .. } => assert!(!no_start),
            other => panic!("expected spawn command, got {other:?}"),
        }
    }

    #[test]
    fn image_build_subcommand_parses_network_and_build_args() {
        let cli = Cli::try_parse_from([
            "hearthctl",
            "image",
            "build",
            "--name",
            "exeuntu",
            "--dockerfile",
            "./Dockerfile",
            "--build-network",
            "netavark",
            "--build-arg",
            "HERMES_BRANCH=main",
            "--build-arg",
            "HERMES_COMMIT=abc123",
            "--skip-lint",
        ])
        .unwrap();
        match cli.command {
            Command::Image {
                command:
                    ImageCommand::Build {
                        build_network,
                        build_arg,
                        skip_lint,
                        ..
                    },
            } => {
                assert_eq!(build_network, oci::BuildNetwork::Netavark);
                assert_eq!(
                    build_arg,
                    vec!["HERMES_BRANCH=main", "HERMES_COMMIT=abc123"]
                );
                assert!(skip_lint);
            }
            other => panic!("expected image build command, got {other:?}"),
        }
    }
}
