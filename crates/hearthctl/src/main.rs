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
mod docker_run;
mod image_build;
mod oci;

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
    Run {
        #[arg(long)]
        dockerfile: Utf8PathBuf,
        #[arg(long, default_value = ".")]
        context: Utf8PathBuf,
        #[arg(long, default_value = "hearth-test")]
        name: String,
        #[arg(long = "mem", default_value = "512M")]
        memory: String,
        #[arg(long, default_value_t = 1)]
        cpus: u32,
        #[arg(long, value_enum, default_value = "none")]
        network: docker_run::NetworkMode,
        #[arg(long, default_value = "hearth0")]
        bridge: String,
        #[arg(long)]
        tap: Option<String>,
        #[arg(long)]
        mac: Option<String>,
        #[arg(long)]
        no_tap_setup: bool,
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
    Image {
        #[command(subcommand)]
        command: ImageCommand,
    },
    Host {
        #[command(subcommand)]
        command: HostCommand,
    },
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
    if let Command::Run {
        dockerfile,
        context,
        name,
        memory,
        cpus,
        network,
        bridge,
        tap,
        mac,
        no_tap_setup,
    } = &cli.command
    {
        return docker_run::run(docker_run::RunOptions {
            dockerfile: dockerfile.clone(),
            context: context.clone(),
            name: name.clone(),
            memory: memory.clone(),
            cpus: *cpus,
            network: *network,
            bridge: bridge.clone(),
            tap: tap.clone(),
            mac: mac.clone(),
            tap_setup: !*no_tap_setup,
            socket: cli.socket.clone(),
        })
        .await;
    }
    if let Command::Image {
        command:
            ImageCommand::Build {
                name,
                dockerfile,
                context,
                disk_gib,
                rootless,
            },
    } = &cli.command
    {
        return image_build::build(image_build::BuildOptions {
            name: name.clone(),
            dockerfile: dockerfile.clone(),
            context: context.clone(),
            disk_gib: *disk_gib,
            rootless: *rootless,
            socket: cli.socket.clone(),
        })
        .await;
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
        Command::Run { .. } => return Err(anyhow!("run is handled locally")),
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
        Command::Ping => println!("pong"),
        Command::Ls => render_ls(first.result.as_ref())?,
        Command::Image {
            command: ImageCommand::Ls,
        } => render_images(first.result.as_ref())?,
        Command::Host {
            command: HostCommand::Check,
        } => render_checks(first.result.as_ref())?,
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
    table.set_header(["NAME", "ENABLED", "RUNNING", "IMAGE", "CPU", "MEM", "CID"]);
    for svc in services {
        table.add_row([
            cell(svc, "name"),
            cell(svc, "enabled"),
            cell(svc, "running"),
            cell(svc, "image"),
            cell(svc, "cpu"),
            cell(svc, "memory_mib"),
            cell(svc, "vsock_cid"),
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
    fn run_subcommand_parses_dockerfile() {
        let cli =
            Cli::try_parse_from(["hearthctl", "run", "--dockerfile", "./Dockerfile"]).unwrap();
        match cli.command {
            Command::Run {
                dockerfile,
                context,
                name,
                memory,
                cpus,
                network,
                bridge,
                tap,
                mac,
                no_tap_setup,
            } => {
                assert_eq!(dockerfile, Utf8PathBuf::from("./Dockerfile"));
                assert_eq!(context, Utf8PathBuf::from("."));
                assert_eq!(name, "hearth-test");
                assert_eq!(memory, "512M");
                assert_eq!(cpus, 1);
                assert_eq!(network, docker_run::NetworkMode::None);
                assert_eq!(bridge, "hearth0");
                assert_eq!(tap, None);
                assert_eq!(mac, None);
                assert!(!no_tap_setup);
            }
            other => panic!("expected run command, got {other:?}"),
        }
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
                    },
            } => {
                assert_eq!(name, "exeuntu");
                assert_eq!(dockerfile, Utf8PathBuf::from("./Dockerfile"));
                assert_eq!(context, Utf8PathBuf::from("."));
                assert_eq!(disk_gib, 40);
                assert!(!rootless);
            }
            other => panic!("expected image build command, got {other:?}"),
        }
    }
}
