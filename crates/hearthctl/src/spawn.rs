//! `hearthctl spawn`: one command from a built template (or straight from a
//! Dockerfile) to a running, individually-provisioned VM. Pure CLI-side
//! composition of the existing `image build` → `create` → `start` verbs
//! (REFACTOR_PROPOSAL.md §10); the daemon contract is unchanged. The provision
//! and publish arg shapes here mirror exactly what `hearthd` create() parses.

use crate::{client::hearth_request, image_build, oci::BuildNetwork};
use anyhow::{anyhow, bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use hearth_proto::{empty_args, Verb};
use serde_json::{json, Map, Value};

/// A single `--provision-file` value, parsed but not yet read. `source` is a
/// client-side path; its bytes are read at spawn time and sent to the daemon as
/// `from_literal` so the daemon never resolves a CLI-relative path (§10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionFileSpec {
    pub source: Utf8PathBuf,
    pub dest: String,
    pub mode: String,
    pub owner: String,
}

/// A single `--publish` value, parsed into the daemon's `[[publish]]` shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishSpec {
    pub host_port: u16,
    pub guest_port: u16,
    pub protocol: String,
    pub bind: Option<String>,
}

/// Everything `main.rs` collects from the `spawn` clap args. Kept as a plain
/// struct so `run` stays independent of the CLI parser.
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    pub name: String,
    pub image: String,
    pub dockerfile: Option<Utf8PathBuf>,
    pub context: Utf8PathBuf,
    pub build_disk_gib: u64,
    pub build_network: BuildNetwork,
    pub build_args: Vec<String>,
    pub provision_file: Vec<String>,
    pub hostname: Option<String>,
    pub cpu: Option<u32>,
    pub memory_mib: Option<u64>,
    pub disk_gib: Option<u64>,
    pub publish: Vec<String>,
    /// Ask the daemon to delete the image's baked SSH host keys so each VM
    /// regenerates a unique set on first boot (maps to `[provision]
    /// reset_ssh_hostkeys`, which serde-defaults to false).
    pub reset_ssh_hostkeys: bool,
    /// Start the service after create. Defaults to true; `--no-start` clears it.
    pub start: bool,
}

/// What to do about the target image before creating the service.
#[derive(Debug, PartialEq, Eq)]
enum BuildDecision {
    /// Image already exists on the daemon: skip the build.
    Skip,
    /// Image absent and a Dockerfile was given: build it locally first.
    Build,
    /// Image absent and no Dockerfile to build from: hard error.
    MissingNoDockerfile,
}

/// Decide the build-if-missing path from image presence and whether a
/// `--dockerfile` was supplied. Present always skips (with or without a
/// Dockerfile); absent builds only when a Dockerfile is available.
fn build_decision(image_present: bool, dockerfile_given: bool) -> BuildDecision {
    match (image_present, dockerfile_given) {
        (true, _) => BuildDecision::Skip,
        (false, true) => BuildDecision::Build,
        (false, false) => BuildDecision::MissingNoDockerfile,
    }
}

/// Does the `image ls` response name `image`? The daemon returns
/// `{ "images": [ { "name": ... }, ... ] }`.
fn image_exists(images_response: &Value, image: &str) -> bool {
    images_response
        .get("images")
        .and_then(Value::as_array)
        .map(|images| {
            images
                .iter()
                .any(|img| img.get("name").and_then(Value::as_str) == Some(image))
        })
        .unwrap_or(false)
}

/// Parse one `--provision-file` value: comma-separated `key=value` fields with
/// keys `source`, `dest`, `mode`, `owner`. Fields are split on commas first,
/// then each on the FIRST `=` only — so a `source` path may contain `=`, but it
/// may NOT contain a comma (that would be read as a field separator). `mode` and
/// `owner` are optional (default `0644` / `0:0`); pass `mode=0600` for secrets.
pub fn parse_provision_file(value: &str) -> Result<ProvisionFileSpec> {
    let mut source: Option<String> = None;
    let mut dest: Option<String> = None;
    let mut mode: Option<String> = None;
    let mut owner: Option<String> = None;
    for field in value.split(',') {
        let field = field.trim();
        if field.is_empty() {
            continue;
        }
        let (key, val) = field.split_once('=').ok_or_else(|| {
            anyhow!("--provision-file field {field:?} must be key=value (keys: source, dest, mode, owner)")
        })?;
        match key.trim() {
            "source" => source = Some(val.to_string()),
            "dest" => dest = Some(val.to_string()),
            "mode" => mode = Some(val.to_string()),
            "owner" => owner = Some(val.to_string()),
            other => bail!(
                "--provision-file: unknown key {other:?} (expected source, dest, mode, owner)"
            ),
        }
    }
    let source = source
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("--provision-file requires source=<local path>"))?;
    let dest =
        dest.ok_or_else(|| anyhow!("--provision-file requires dest=<absolute path in guest>"))?;
    if !dest.starts_with('/') {
        bail!("--provision-file dest must be an absolute path, got {dest:?}");
    }
    let mode = mode.unwrap_or_else(|| "0644".to_string());
    let owner = owner.unwrap_or_else(|| "0:0".to_string());
    validate_mode(&mode)?;
    validate_owner(&owner)?;
    Ok(ProvisionFileSpec {
        source: Utf8PathBuf::from(source),
        dest,
        mode,
        owner,
    })
}

/// Parse one `--publish` value: `host:guest[/tcp|udp][@bind]`. The optional
/// `@bind` restricts the forward to one host IP; the optional `/proto` defaults
/// to `tcp`. Mirrors the daemon's `Publish::validate`.
pub fn parse_publish(value: &str) -> Result<PublishSpec> {
    let (left, bind) = match value.split_once('@') {
        Some((left, bind)) => (left, Some(bind.to_string())),
        None => (value, None),
    };
    let (ports, protocol) = match left.split_once('/') {
        Some((ports, proto)) => (ports, proto.to_string()),
        None => (left, "tcp".to_string()),
    };
    let (host, guest) = ports.split_once(':').ok_or_else(|| {
        anyhow!("--publish must be host:guest[/tcp|udp][@bind], got {value:?}")
    })?;
    let host_port = parse_port(host, "host")?;
    let guest_port = parse_port(guest, "guest")?;
    if protocol != "tcp" && protocol != "udp" {
        bail!("--publish protocol must be tcp or udp, got {protocol:?}");
    }
    if let Some(bind) = &bind {
        bind.parse::<std::net::IpAddr>()
            .map_err(|_| anyhow!("--publish bind must be an IP address, got {bind:?}"))?;
    }
    Ok(PublishSpec {
        host_port,
        guest_port,
        protocol,
        bind,
    })
}

fn parse_port(text: &str, which: &str) -> Result<u16> {
    let port = text
        .trim()
        .parse::<u16>()
        .map_err(|_| anyhow!("--publish {which} port must be 1-65535, got {text:?}"))?;
    if port == 0 {
        bail!("--publish {which} port must be 1-65535, got 0");
    }
    Ok(port)
}

/// Validate an octal mode string (e.g. `"0600"`). Mirrors the daemon's
/// `parse_mode` so a bad mode fails client-side with the same rules.
fn validate_mode(mode: &str) -> Result<()> {
    let trimmed = mode.trim();
    let digits = trimmed.strip_prefix("0o").unwrap_or(trimmed);
    if digits.is_empty() {
        bail!("mode must be an octal string like \"0600\", got {mode:?}");
    }
    let bits = u32::from_str_radix(digits, 8)
        .map_err(|_| anyhow!("mode must be an octal string like \"0600\", got {mode:?}"))?;
    if bits > 0o7777 {
        bail!("mode {mode:?} is out of range (max 0o7777)");
    }
    Ok(())
}

/// Validate a numeric `uid:gid` owner string. Names are rejected (the unbooted
/// rootfs cannot be consulted), matching the daemon's `parse_owner`.
fn validate_owner(owner: &str) -> Result<()> {
    let (uid, gid) = owner
        .split_once(':')
        .ok_or_else(|| anyhow!("owner must be numeric uid:gid, got {owner:?}"))?;
    uid.trim()
        .parse::<u32>()
        .map_err(|_| anyhow!("owner uid must be numeric, got {uid:?}"))?;
    gid.trim()
        .parse::<u32>()
        .map_err(|_| anyhow!("owner gid must be numeric, got {gid:?}"))?;
    Ok(())
}

/// Resolved inputs to a `create` request. Groups the scalars so `create_args`
/// stays a one-argument pure function (and under clippy's arg-count limit).
struct CreateInputs<'a> {
    name: &'a str,
    image: &'a str,
    hostname: &'a str,
    cpu: Option<u32>,
    memory_mib: Option<u64>,
    disk_gib: Option<u64>,
    /// Each parsed spec paired with its already-read literal content.
    provision: &'a [(ProvisionFileSpec, String)],
    publish: &'a [PublishSpec],
    /// Emit `reset_ssh_hostkeys = true` in the provision block.
    reset_ssh_hostkeys: bool,
}

/// Build the exact `create` request the daemon expects. Provision files are sent
/// as `from_literal` content. Pure, so the wire shape is unit-tested without any
/// daemon or filesystem.
fn create_args(inputs: &CreateInputs) -> Map<String, Value> {
    let mut args = Map::new();
    args.insert("name".to_string(), json!(inputs.name));
    args.insert("image".to_string(), json!(inputs.image));
    args.insert("hostname".to_string(), json!(inputs.hostname));
    if let Some(cpu) = inputs.cpu {
        args.insert("cpu".to_string(), json!(cpu));
    }
    if let Some(memory_mib) = inputs.memory_mib {
        args.insert("memory_mib".to_string(), json!(memory_mib));
    }
    if let Some(disk_gib) = inputs.disk_gib {
        args.insert("disk_gib".to_string(), json!(disk_gib));
    }
    // Emit a provision block when there are files to write OR host-key reset was
    // requested. reset_ssh_hostkeys is only inserted when true so the common
    // (files-only) case keeps the exact `{ "files": [...] }` shape the daemon
    // parsed before, and reset_machine_id keeps its daemon-side default of true.
    if !inputs.provision.is_empty() || inputs.reset_ssh_hostkeys {
        let files: Vec<Value> = inputs
            .provision
            .iter()
            .map(|(spec, content)| {
                json!({
                    "from_literal": content,
                    "dest": spec.dest,
                    "mode": spec.mode,
                    "owner": spec.owner,
                })
            })
            .collect();
        let mut provision = Map::new();
        provision.insert("files".to_string(), json!(files));
        if inputs.reset_ssh_hostkeys {
            provision.insert("reset_ssh_hostkeys".to_string(), json!(true));
        }
        args.insert("provision".to_string(), Value::Object(provision));
    }
    if !inputs.publish.is_empty() {
        let entries: Vec<Value> = inputs
            .publish
            .iter()
            .map(|p| {
                let mut obj = Map::new();
                obj.insert("host_port".to_string(), json!(p.host_port));
                obj.insert("guest_port".to_string(), json!(p.guest_port));
                obj.insert("protocol".to_string(), json!(p.protocol));
                if let Some(bind) = &p.bind {
                    obj.insert("bind".to_string(), json!(bind));
                }
                Value::Object(obj)
            })
            .collect();
        args.insert("publish".to_string(), json!(entries));
    }
    args
}

fn name_args(name: &str) -> Map<String, Value> {
    Map::from_iter([("name".to_string(), json!(name))])
}

/// Run `spawn`: (build-if-missing) → create → start → print status.
pub async fn run(socket: &Utf8Path, opts: SpawnOptions) -> Result<()> {
    // Parse every repeatable flag up front so a malformed value fails before any
    // daemon round-trip, build, or file read.
    let provision_specs = opts
        .provision_file
        .iter()
        .map(|v| parse_provision_file(v))
        .collect::<Result<Vec<_>>>()?;
    let publishes = opts
        .publish
        .iter()
        .map(|v| parse_publish(v))
        .collect::<Result<Vec<_>>>()?;

    // Build-if-missing: consult the daemon's image list, then decide.
    let images = hearth_request(socket, Verb::ImageLs, empty_args()).await?;
    let present = image_exists(&images, &opts.image);
    match build_decision(present, opts.dockerfile.is_some()) {
        BuildDecision::Skip => {
            if opts.dockerfile.is_some() {
                eprintln!("hearthctl: image {} exists, skipping build", opts.image);
            }
        }
        BuildDecision::Build => {
            let dockerfile = opts
                .dockerfile
                .clone()
                .expect("Build decision implies a dockerfile");
            eprintln!(
                "hearthctl: image {} not found; building from {dockerfile}",
                opts.image
            );
            image_build::build(image_build::BuildOptions {
                name: opts.image.clone(),
                dockerfile,
                context: opts.context.clone(),
                disk_gib: opts.build_disk_gib,
                rootless: false,
                network: opts.build_network,
                build_args: opts.build_args.clone(),
                skip_lint: false,
                socket: socket.to_path_buf(),
            })
            .await?;
        }
        BuildDecision::MissingNoDockerfile => bail!(
            "image {:?} not found on the daemon and no --dockerfile was given to build it; \
             pass --dockerfile/--context to build it, or run `hearthctl image build` first",
            opts.image
        ),
    }

    // Read each provision source client-side; the daemon only ever sees the
    // literal content. Non-UTF-8 is rejected with a clear message for now.
    let mut provision = Vec::with_capacity(provision_specs.len());
    for spec in provision_specs {
        let bytes = std::fs::read(&spec.source)
            .with_context(|| format!("read provision file source {}", spec.source))?;
        let content = String::from_utf8(bytes).map_err(|_| {
            anyhow!(
                "provision file source {} is not valid UTF-8; only text files are supported \
                 (binary provisioning is not implemented yet)",
                spec.source
            )
        })?;
        provision.push((spec, content));
    }

    // --hostname wins; otherwise the service name is the hostname (§10).
    let hostname = opts
        .hostname
        .clone()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| opts.name.clone());
    let args = create_args(&CreateInputs {
        name: &opts.name,
        image: &opts.image,
        hostname: &hostname,
        cpu: opts.cpu,
        memory_mib: opts.memory_mib,
        disk_gib: opts.disk_gib,
        provision: &provision,
        publish: &publishes,
        reset_ssh_hostkeys: opts.reset_ssh_hostkeys,
    });
    hearth_request(socket, Verb::Create, args).await?;

    if opts.start {
        hearth_request(socket, Verb::Start, name_args(&opts.name)).await?;
    }

    let status = hearth_request(socket, Verb::Status, name_args(&opts.name)).await?;
    println!("{}", serde_json::to_string_pretty(&status)?);
    // The address is null until DHCP assigns one (or a static lease lands); say
    // so rather than leave the operator staring at a null field.
    if status.get("address").map(Value::is_null).unwrap_or(true) {
        eprintln!(
            "note: {} has no address yet; rerun `hearthctl status {}` once the DHCP lease lands",
            opts.name, opts.name
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_decision_table() {
        // present x dockerfile-given: present always skips; absent needs a file.
        assert_eq!(build_decision(true, true), BuildDecision::Skip);
        assert_eq!(build_decision(true, false), BuildDecision::Skip);
        assert_eq!(build_decision(false, true), BuildDecision::Build);
        assert_eq!(
            build_decision(false, false),
            BuildDecision::MissingNoDockerfile
        );
    }

    #[test]
    fn image_exists_matches_by_name() {
        let response = json!({ "images": [
            { "name": "vm-base" },
            { "name": "hermes-vm" },
        ] });
        assert!(image_exists(&response, "hermes-vm"));
        assert!(!image_exists(&response, "missing"));
        // Malformed / empty responses read as "absent".
        assert!(!image_exists(&json!({}), "hermes-vm"));
        assert!(!image_exists(&json!({ "images": [] }), "hermes-vm"));
    }

    #[test]
    fn provision_file_full_spec() {
        let spec = parse_provision_file(
            "source=./a.env,dest=/home/agent/.hermes/.env,mode=0600,owner=1000:1000",
        )
        .unwrap();
        assert_eq!(
            spec,
            ProvisionFileSpec {
                source: Utf8PathBuf::from("./a.env"),
                dest: "/home/agent/.hermes/.env".to_string(),
                mode: "0600".to_string(),
                owner: "1000:1000".to_string(),
            }
        );
    }

    #[test]
    fn provision_file_defaults_mode_and_owner() {
        let spec = parse_provision_file("source=./motd,dest=/etc/motd").unwrap();
        assert_eq!(spec.mode, "0644");
        assert_eq!(spec.owner, "0:0");
    }

    #[test]
    fn provision_file_source_may_contain_equals() {
        // Split on the FIRST '=' only: a source path (or query string) may
        // itself contain '='.
        let spec = parse_provision_file("source=./q?a=b,dest=/etc/q").unwrap();
        assert_eq!(spec.source, Utf8PathBuf::from("./q?a=b"));
    }

    #[test]
    fn provision_file_rejects_missing_dest() {
        let err = parse_provision_file("source=./a.env,mode=0600").unwrap_err();
        assert!(err.to_string().contains("dest"));
    }

    #[test]
    fn provision_file_rejects_missing_source() {
        let err = parse_provision_file("dest=/etc/x,mode=0600").unwrap_err();
        assert!(err.to_string().contains("source"));
    }

    #[test]
    fn provision_file_rejects_relative_dest() {
        let err = parse_provision_file("source=./a,dest=etc/x").unwrap_err();
        assert!(err.to_string().contains("absolute"));
    }

    #[test]
    fn provision_file_rejects_bad_mode() {
        // 9 is not an octal digit.
        let err = parse_provision_file("source=./a,dest=/etc/x,mode=0999").unwrap_err();
        assert!(err.to_string().contains("octal"));
    }

    #[test]
    fn provision_file_rejects_bad_owner() {
        let err = parse_provision_file("source=./a,dest=/etc/x,owner=agent:agent").unwrap_err();
        assert!(err.to_string().contains("numeric"));
    }

    #[test]
    fn provision_file_rejects_unknown_key() {
        let err = parse_provision_file("source=./a,dest=/etc/x,perm=0600").unwrap_err();
        assert!(err.to_string().contains("unknown key"));
    }

    #[test]
    fn publish_minimal_defaults_tcp_no_bind() {
        let spec = parse_publish("9119:9119").unwrap();
        assert_eq!(
            spec,
            PublishSpec {
                host_port: 9119,
                guest_port: 9119,
                protocol: "tcp".to_string(),
                bind: None,
            }
        );
    }

    #[test]
    fn publish_with_proto_and_bind() {
        let spec = parse_publish("53:53/udp@127.0.0.1").unwrap();
        assert_eq!(
            spec,
            PublishSpec {
                host_port: 53,
                guest_port: 53,
                protocol: "udp".to_string(),
                bind: Some("127.0.0.1".to_string()),
            }
        );
    }

    #[test]
    fn publish_bind_without_proto() {
        let spec = parse_publish("8080:80@10.0.0.1").unwrap();
        assert_eq!(spec.protocol, "tcp");
        assert_eq!(spec.bind.as_deref(), Some("10.0.0.1"));
    }

    #[test]
    fn publish_rejects_missing_colon() {
        let err = parse_publish("9119").unwrap_err();
        assert!(err.to_string().contains("host:guest"));
    }

    #[test]
    fn publish_rejects_zero_and_out_of_range_ports() {
        assert!(parse_publish("0:80").unwrap_err().to_string().contains("host"));
        assert!(parse_publish("80:0").unwrap_err().to_string().contains("guest"));
        // 70000 overflows u16.
        assert!(parse_publish("70000:80").is_err());
    }

    #[test]
    fn publish_rejects_bad_protocol_and_bind() {
        assert!(parse_publish("80:80/sctp")
            .unwrap_err()
            .to_string()
            .contains("protocol"));
        assert!(parse_publish("80:80@not-an-ip")
            .unwrap_err()
            .to_string()
            .contains("bind"));
    }

    #[test]
    fn create_args_minimal_omits_optional_fields() {
        let args = create_args(&CreateInputs {
            name: "dev",
            image: "exeuntu",
            hostname: "dev",
            cpu: None,
            memory_mib: None,
            disk_gib: None,
            provision: &[],
            publish: &[],
            reset_ssh_hostkeys: false,
        });
        assert_eq!(args.get("name"), Some(&json!("dev")));
        assert_eq!(args.get("image"), Some(&json!("exeuntu")));
        // hostname always defaults to the service name.
        assert_eq!(args.get("hostname"), Some(&json!("dev")));
        assert!(!args.contains_key("cpu"));
        assert!(!args.contains_key("memory_mib"));
        assert!(!args.contains_key("disk_gib"));
        assert!(!args.contains_key("provision"));
        assert!(!args.contains_key("publish"));
    }

    #[test]
    fn create_args_matches_daemon_provision_and_publish_shapes() {
        let provision = vec![(
            ProvisionFileSpec {
                source: Utf8PathBuf::from("./a.env"),
                dest: "/home/agent/.hermes/.env".to_string(),
                mode: "0600".to_string(),
                owner: "1000:1000".to_string(),
            },
            "TOKEN=secret".to_string(),
        )];
        let publish = vec![PublishSpec {
            host_port: 9119,
            guest_port: 9119,
            protocol: "tcp".to_string(),
            bind: Some("127.0.0.1".to_string()),
        }];
        let args = create_args(&CreateInputs {
            name: "hermes-a",
            image: "hermes-vm",
            hostname: "hermes-a",
            cpu: Some(4),
            memory_mib: Some(4096),
            disk_gib: Some(32),
            provision: &provision,
            publish: &publish,
            reset_ssh_hostkeys: false,
        });
        assert_eq!(args.get("cpu"), Some(&json!(4)));
        assert_eq!(args.get("memory_mib"), Some(&json!(4096)));
        assert_eq!(args.get("disk_gib"), Some(&json!(32)));
        // Provision block: exactly the [provision].files shape create() parses,
        // carrying the literal content (not the client-side path).
        assert_eq!(
            args.get("provision"),
            Some(&json!({
                "files": [{
                    "from_literal": "TOKEN=secret",
                    "dest": "/home/agent/.hermes/.env",
                    "mode": "0600",
                    "owner": "1000:1000",
                }]
            }))
        );
        // Publish array: the [[publish]] shape, protocol always explicit.
        assert_eq!(
            args.get("publish"),
            Some(&json!([{
                "host_port": 9119,
                "guest_port": 9119,
                "protocol": "tcp",
                "bind": "127.0.0.1",
            }]))
        );
    }

    #[test]
    fn create_args_publish_omits_bind_when_absent() {
        let publish = vec![PublishSpec {
            host_port: 22,
            guest_port: 22,
            protocol: "tcp".to_string(),
            bind: None,
        }];
        let args = create_args(&CreateInputs {
            name: "dev",
            image: "exeuntu",
            hostname: "dev",
            cpu: None,
            memory_mib: None,
            disk_gib: None,
            provision: &[],
            publish: &publish,
            reset_ssh_hostkeys: false,
        });
        let entry = &args.get("publish").unwrap().as_array().unwrap()[0];
        assert!(entry.get("bind").is_none());
    }

    #[test]
    fn create_args_reset_ssh_hostkeys_emits_provision_block_without_files() {
        // --reset-ssh-hostkeys with no --provision-file must still send a
        // provision block so the daemon deletes the baked host keys; the flag
        // rides in it and reset_machine_id keeps its daemon-side default.
        let args = create_args(&CreateInputs {
            name: "dev",
            image: "exeuntu",
            hostname: "dev",
            cpu: None,
            memory_mib: None,
            disk_gib: None,
            provision: &[],
            publish: &[],
            reset_ssh_hostkeys: true,
        });
        assert_eq!(
            args.get("provision"),
            Some(&json!({
                "files": [],
                "reset_ssh_hostkeys": true,
            }))
        );
    }
}
