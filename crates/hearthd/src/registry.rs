use crate::{config::Config, error::coded};
use anyhow::{anyhow, bail, Context, Result};
use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use rand::Rng;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use tokio::fs;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub name: String,
    pub enabled: bool,
    pub image: String,
    pub cpu: u32,
    pub memory_mib: u64,
    pub disk_gib: u64,
    pub vsock_cid: u32,
    pub mac: String,
    #[serde(default)]
    pub is_agent_in_charge: bool,
    /// Agent-plane participation (docs/agent-plane.md §2.5): only services
    /// with `agent = true` are visible to `agent-endpoints`/agentd, and
    /// setting it requires a guestd-declaring image at create time.
    #[serde(default)]
    pub agent: bool,
    // Recorded per-VM disk filename (e.g. `web.raw` or `mail.qcow2`). Older
    // services predate this field; `Config::disk_path` falls back to the legacy
    // `{name}.qcow2` when it is absent. Must stay a scalar (before the tables
    // below) so `toml::to_string_pretty` serializes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<String>,
    // Managed host->guest port forwards (REFACTOR_PROPOSAL.md §4.3). An array of
    // tables `[[publish]]`; declared among the other tables (after every scalar)
    // so `toml::to_string_pretty` serializes it. Empty is skipped so a service
    // with no publishes stays scalar-clean.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub publish: Vec<Publish>,
    #[serde(default)]
    pub provision: Provision,
    #[serde(default)]
    pub restart: RestartPolicy,
}

/// A managed host->guest port forward (REFACTOR_PROPOSAL.md §4.3). This is VM
/// port-forwarding owned by the registry, not Docker `-p` emulation: hearthd
/// renders every service's publishes into the `hearth_nat` nftables table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Publish {
    // Management handle so `publish rm` can target one forward by name. Optional
    // on disk: publishes created via `spawn --publish` (or before names existed)
    // have none and fall back to a deterministic name via `effective_name`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub host_port: u16,
    pub guest_port: u16,
    #[serde(default = "default_protocol")]
    pub protocol: String,
    // Optional host address to restrict the forward to; default is all host
    // addresses. Stored as a string (validated as an IpAddr) so the TOML stays
    // human-legible and round-trips.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
}

impl Publish {
    pub fn validate(&self) -> Result<()> {
        // u16 already caps at 65535; reject port 0, which is not a real port.
        if self.host_port == 0 || self.guest_port == 0 {
            bail!("publish ports must be in 1-65535");
        }
        if self.protocol != "tcp" && self.protocol != "udp" {
            bail!(
                "publish protocol must be \"tcp\" or \"udp\", got {:?}",
                self.protocol
            );
        }
        if let Some(bind) = &self.bind {
            bind.parse::<std::net::IpAddr>()
                .map_err(|_| anyhow!("publish bind must be an IP address, got {bind:?}"))?;
        }
        Ok(())
    }

    /// The name `publish rm` matches on. Named forwards use their name; unnamed
    /// ones (from `spawn --publish` or pre-names TOML) get a deterministic
    /// `{host_port}-{protocol}` handle so they are still addressable.
    pub fn effective_name(&self) -> String {
        if self.name.is_empty() {
            format!("{}-{}", self.host_port, self.protocol)
        } else {
            self.name.clone()
        }
    }
}

fn default_protocol() -> String {
    "tcp".to_string()
}

/// Per-service offline customization applied to a VM's disk at
/// create time (see REFACTOR_PROPOSAL.md §3). The whole section is optional.
/// Scalar fields are declared before `files` so `toml::to_string_pretty` (which
/// rejects a scalar after an array-of-tables) can serialize it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provision {
    // systemd regenerates a truncated /etc/machine-id on boot; cloned
    // machine-ids collide in dnsmasq DUIDs, so default to resetting.
    #[serde(default = "default_true")]
    pub reset_machine_id: bool,
    // Removing host keys only helps if the image regenerates them on boot
    // (ssh-keygen -A via sshd's unit); off by default.
    #[serde(default)]
    pub reset_ssh_hostkeys: bool,
    #[serde(default)]
    pub hostname: String,
    /// Canonical bare OpenSSH public-key lines installed for the `agent` user.
    /// Host recovery keys are merged into this list at create time so the
    /// service record describes what was actually written to its disk.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authorized_keys: Vec<String>,
    /// Explicit escape hatch for a keyless VM. This is only persisted as true
    /// when the effective key list is empty.
    #[serde(default)]
    pub allow_no_ssh: bool,
    #[serde(default)]
    pub files: Vec<ProvisionFile>,
}

impl Default for Provision {
    fn default() -> Self {
        Self {
            reset_machine_id: true,
            reset_ssh_hostkeys: false,
            hostname: String::new(),
            authorized_keys: Vec::new(),
            allow_no_ssh: false,
            files: Vec::new(),
        }
    }
}

impl Provision {
    /// A secret-free summary for `status`: literal file contents are redacted to
    /// `<literal>`; only dest/mode/owner and the reset/hostname flags are shown.
    pub fn redacted_summary(&self) -> serde_json::Value {
        let files: Vec<serde_json::Value> = self
            .files
            .iter()
            .map(|f| {
                let source = match (&f.source, &f.from_literal) {
                    (Some(src), _) => json!(src),
                    _ => json!("<literal>"),
                };
                json!({
                    "dest": f.dest,
                    "mode": f.mode,
                    "owner": f.owner,
                    "source": source,
                })
            })
            .collect();
        json!({
            "files": files,
            "reset_machine_id": self.reset_machine_id,
            "reset_ssh_hostkeys": self.reset_ssh_hostkeys,
            "hostname": self.hostname,
            "ssh_access": self.ssh_access_state(),
            "ssh_user": "agent",
            "ssh_key_fingerprints": self.ssh_key_fingerprints(),
        })
    }

    pub fn ssh_access_state(&self) -> &'static str {
        if !self.authorized_keys.is_empty()
            && self.ssh_key_fingerprints().len() == self.authorized_keys.len()
        {
            "configured"
        } else if self.allow_no_ssh {
            "intentionally-disabled"
        } else {
            "legacy-unknown"
        }
    }

    pub fn ssh_key_fingerprints(&self) -> Vec<String> {
        self.authorized_keys
            .iter()
            .filter_map(|line| crate::ssh::parse_authorized_key(line).ok())
            .map(|key| key.fingerprint)
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvisionFile {
    // Exactly one of `source` (an absolute path read by the daemon at provision
    // time) or `from_literal` (inline content) must be set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Utf8PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_literal: Option<String>,
    pub dest: Utf8PathBuf,
    pub mode: String,
    pub owner: String,
}

impl ProvisionFile {
    pub fn validate(&self) -> Result<()> {
        match (&self.source, &self.from_literal) {
            (Some(_), Some(_)) => bail!(
                "provision file {}: set exactly one of `source` or `from_literal`, not both",
                self.dest
            ),
            (None, None) => bail!(
                "provision file {}: set exactly one of `source` or `from_literal`",
                self.dest
            ),
            _ => {}
        }
        if !self.dest.is_absolute() {
            bail!(
                "provision file dest must be an absolute path: {}",
                self.dest
            );
        }
        if self
            .dest
            .components()
            .any(|c| matches!(c, Utf8Component::ParentDir))
        {
            bail!("provision file dest must not contain `..`: {}", self.dest);
        }
        if let Some(src) = &self.source {
            if !src.is_absolute() {
                bail!("provision file source must be an absolute path: {src}");
            }
        }
        parse_mode(&self.mode).with_context(|| format!("provision file {}", self.dest))?;
        parse_owner(&self.owner).with_context(|| format!("provision file {}", self.dest))?;
        Ok(())
    }
}

/// Parse an octal mode string (e.g. `"0600"`) into permission bits.
pub fn parse_mode(mode: &str) -> Result<u32> {
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
    Ok(bits)
}

/// Parse a numeric `uid:gid` owner string. No passwd resolution: names are
/// rejected because the unbooted rootfs cannot be consulted.
pub fn parse_owner(owner: &str) -> Result<(u32, u32)> {
    let (uid, gid) = owner
        .split_once(':')
        .ok_or_else(|| anyhow!("owner must be numeric uid:gid, got {owner:?}"))?;
    let uid = uid
        .trim()
        .parse::<u32>()
        .map_err(|_| anyhow!("owner uid must be numeric, got {uid:?}"))?;
    let gid = gid
        .trim()
        .parse::<u32>()
        .map_err(|_| anyhow!("owner gid must be numeric, got {gid:?}"))?;
    Ok((uid, gid))
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestartPolicy {
    #[serde(default = "default_restart_policy")]
    pub policy: String,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_backoff_sec")]
    pub backoff_sec: u64,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            policy: default_restart_policy(),
            max_retries: default_max_retries(),
            backoff_sec: default_backoff_sec(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Allocations {
    #[serde(default)]
    pub vsock_cids: BTreeMap<String, u32>,
    #[serde(default)]
    pub macs: BTreeMap<String, String>,
    // Static-lease IPs (REFACTOR_PROPOSAL.md §4.2), allocated from the config's
    // static slice, sitting next to CID and MAC where they belong. Absent for a
    // service means no static reservation (dynamic DHCP only).
    #[serde(default)]
    pub ips: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct Registry {
    pub services: BTreeMap<String, Service>,
    pub allocations: Allocations,
}

pub fn validate_name(name: &str) -> Result<()> {
    let re = Regex::new(r"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$").unwrap();
    if re.is_match(name) {
        Ok(())
    } else {
        bail!("service names must be kebab-case and start with a letter")
    }
}

impl Registry {
    pub async fn load(cfg: &Config) -> Result<Self> {
        fs::create_dir_all(&cfg.services_dir).await?;
        let mut services = BTreeMap::new();
        let mut entries = fs::read_dir(&cfg.services_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = Utf8PathBuf::from_path_buf(entry.path())
                .map_err(|p| anyhow!("non-utf8 service path: {}", p.display()))?;
            if path.extension() != Some("toml") {
                continue;
            }
            let text = fs::read_to_string(&path)
                .await
                .with_context(|| format!("read {path}"))?;
            let svc: Service = toml::from_str(&text).with_context(|| format!("parse {path}"))?;
            validate_name(&svc.name)?;
            if services.insert(svc.name.clone(), svc).is_some() {
                bail!("duplicate service name in registry");
            }
        }
        let allocations = match fs::read_to_string(&cfg.allocations).await {
            Ok(text) => toml::from_str(&text).context("parse allocations")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Allocations::default(),
            Err(e) => return Err(e).context("read allocations"),
        };
        Self::validate_agent_in_charge(&services)?;
        Ok(Self {
            services,
            allocations,
        })
    }

    pub fn get(&self, name: &str) -> Result<&Service> {
        self.services
            .get(name)
            .ok_or_else(|| coded("service.not_found", format!("no service named {name}")))
    }

    pub async fn write_service(cfg: &Config, svc: &Service) -> Result<()> {
        fs::create_dir_all(&cfg.services_dir).await?;
        let path = service_path(&cfg.services_dir, &svc.name);
        atomic_write_toml(&path, svc).await
    }

    pub async fn remove_service(cfg: &Config, name: &str) -> Result<()> {
        let path = service_path(&cfg.services_dir, name);
        match fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn write_allocations(cfg: &Config, allocations: &Allocations) -> Result<()> {
        if let Some(parent) = cfg.allocations.parent() {
            fs::create_dir_all(parent).await?;
        }
        atomic_write_toml(&cfg.allocations, allocations).await
    }

    /// Allocate a CID, MAC, and (from the config's static slice) a static-lease
    /// IP for a new service. The IP is `None` when the whole slice is taken —
    /// the service still boots with dynamic DHCP, it just gets no reservation.
    pub fn allocate(
        &mut self,
        name: &str,
        static_start: std::net::Ipv4Addr,
        static_count: u32,
    ) -> (u32, String, Option<String>) {
        let mut used_cids: BTreeSet<u32> = self.allocations.vsock_cids.values().copied().collect();
        used_cids.extend(self.services.values().map(|svc| svc.vsock_cid));
        let mut cid = 100;
        while used_cids.contains(&cid) {
            cid += 1;
        }
        let mut used_macs: BTreeSet<String> = self.allocations.macs.values().cloned().collect();
        used_macs.extend(self.services.values().map(|svc| svc.mac.clone()));
        let mut rng = rand::thread_rng();
        let mac = loop {
            let mac = format!(
                "52:54:00:{:02x}:{:02x}:{:02x}",
                rng.gen::<u8>(),
                rng.gen::<u8>(),
                rng.gen::<u8>()
            );
            if !used_macs.contains(&mac) {
                break mac;
            }
        };
        let used_ips: BTreeSet<std::net::Ipv4Addr> = self
            .allocations
            .ips
            .values()
            .filter_map(|s| s.parse().ok())
            .collect();
        let ip =
            crate::net::allocate_ip(static_start, static_count, &used_ips).map(|ip| ip.to_string());
        self.allocations.vsock_cids.insert(name.to_string(), cid);
        self.allocations.macs.insert(name.to_string(), mac.clone());
        if let Some(ip) = &ip {
            self.allocations.ips.insert(name.to_string(), ip.clone());
        }
        (cid, mac, ip)
    }

    pub fn free(&mut self, name: &str) {
        self.allocations.vsock_cids.remove(name);
        self.allocations.macs.remove(name);
        self.allocations.ips.remove(name);
    }

    fn validate_agent_in_charge(services: &BTreeMap<String, Service>) -> Result<()> {
        let count = services.values().filter(|s| s.is_agent_in_charge).count();
        if count > 1 {
            bail!("at most one service may set is_agent_in_charge = true");
        }
        Ok(())
    }
}

pub fn service_path(dir: &Utf8Path, name: &str) -> Utf8PathBuf {
    dir.join(format!("{name}.toml"))
}

async fn atomic_write_toml<T: Serialize>(path: &Utf8Path, value: &T) -> Result<()> {
    let text = toml::to_string_pretty(value)?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {path}"))?;
    fs::create_dir_all(parent).await?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or("hearth"),
        std::process::id()
    ));
    fs::write(&tmp, text).await?;
    // Service TOMLs may carry provisioning literals (secrets). Lock the file
    // down before it becomes visible under its final name.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
    }
    fs::rename(&tmp, path).await?;
    Ok(())
}

fn default_restart_policy() -> String {
    "on-failure".to_string()
}

fn default_max_retries() -> u32 {
    5
}

fn default_backoff_sec() -> u64 {
    10
}

#[cfg(test)]
mod tests {
    use super::*;

    fn literal_file(dest: &str, mode: &str, owner: &str) -> ProvisionFile {
        ProvisionFile {
            source: None,
            from_literal: Some("secret".to_string()),
            dest: Utf8PathBuf::from(dest),
            mode: mode.to_string(),
            owner: owner.to_string(),
        }
    }

    #[test]
    fn parse_mode_accepts_octal_strings() {
        assert_eq!(parse_mode("0600").unwrap(), 0o600);
        assert_eq!(parse_mode("644").unwrap(), 0o644);
        assert_eq!(parse_mode("0o755").unwrap(), 0o755);
    }

    #[test]
    fn parse_mode_rejects_non_octal_and_empty() {
        assert!(parse_mode("0678").is_err()); // 8 is not an octal digit
        assert!(parse_mode("rwxr-xr-x").is_err());
        assert!(parse_mode("").is_err());
        assert!(parse_mode("10000").is_err()); // out of range
    }

    #[test]
    fn parse_owner_requires_numeric_uid_gid() {
        assert_eq!(parse_owner("1000:1000").unwrap(), (1000, 1000));
        assert_eq!(parse_owner("0:0").unwrap(), (0, 0));
        assert!(parse_owner("agent:agent").is_err());
        assert!(parse_owner("1000").is_err());
        assert!(parse_owner("1000:agent").is_err());
    }

    #[test]
    fn provision_file_validate_accepts_a_well_formed_entry() {
        assert!(literal_file("/home/agent/.env", "0600", "1000:1000")
            .validate()
            .is_ok());
        let src = ProvisionFile {
            source: Some(Utf8PathBuf::from("/etc/hearth/secrets/a.env")),
            from_literal: None,
            dest: Utf8PathBuf::from("/home/agent/.env"),
            mode: "0640".to_string(),
            owner: "1000:1000".to_string(),
        };
        assert!(src.validate().is_ok());
    }

    #[test]
    fn provision_file_validate_rejects_both_and_neither_source() {
        let both = ProvisionFile {
            source: Some(Utf8PathBuf::from("/a")),
            from_literal: Some("x".to_string()),
            dest: Utf8PathBuf::from("/dest"),
            mode: "0600".to_string(),
            owner: "0:0".to_string(),
        };
        assert!(both.validate().is_err());
        let neither = ProvisionFile {
            source: None,
            from_literal: None,
            dest: Utf8PathBuf::from("/dest"),
            mode: "0600".to_string(),
            owner: "0:0".to_string(),
        };
        assert!(neither.validate().is_err());
    }

    #[test]
    fn provision_file_validate_rejects_relative_dest_and_parent_dir() {
        assert!(literal_file("home/agent/.env", "0600", "0:0")
            .validate()
            .is_err());
        assert!(literal_file("/home/../etc/shadow", "0600", "0:0")
            .validate()
            .is_err());
    }

    #[test]
    fn provision_file_validate_rejects_bad_mode_and_owner() {
        assert!(literal_file("/dest", "999", "0:0").validate().is_err());
        assert!(literal_file("/dest", "0600", "root:root")
            .validate()
            .is_err());
    }

    #[test]
    fn provision_file_validate_rejects_relative_source() {
        let rel = ProvisionFile {
            source: Some(Utf8PathBuf::from("relative/a.env")),
            from_literal: None,
            dest: Utf8PathBuf::from("/dest"),
            mode: "0600".to_string(),
            owner: "0:0".to_string(),
        };
        assert!(rel.validate().is_err());
    }

    #[test]
    fn provision_defaults_reset_machine_id_true() {
        let p = Provision::default();
        assert!(p.reset_machine_id);
        assert!(!p.reset_ssh_hostkeys);
        assert!(p.files.is_empty());
        // An empty `[provision]` table also defaults reset_machine_id on.
        let parsed: Provision = toml::from_str("").unwrap();
        assert!(parsed.reset_machine_id);
    }

    #[test]
    fn redacted_summary_never_echoes_literal_content() {
        let mut p = Provision::default();
        p.files
            .push(literal_file("/home/agent/.env", "0600", "1000:1000"));
        let summary = p.redacted_summary().to_string();
        assert!(summary.contains("<literal>"));
        assert!(!summary.contains("secret"));
        assert!(summary.contains("/home/agent/.env"));
    }

    #[test]
    fn publish_validate_accepts_well_formed_entries() {
        assert!(Publish {
            name: String::new(),
            host_port: 9119,
            guest_port: 9119,
            protocol: "tcp".to_string(),
            bind: None,
        }
        .validate()
        .is_ok());
        assert!(Publish {
            name: String::new(),
            host_port: 53,
            guest_port: 53,
            protocol: "udp".to_string(),
            bind: Some("100.121.19.41".to_string()),
        }
        .validate()
        .is_ok());
    }

    #[test]
    fn publish_validate_rejects_bad_port_protocol_and_bind() {
        assert!(Publish {
            name: String::new(),
            host_port: 0,
            guest_port: 80,
            protocol: "tcp".to_string(),
            bind: None,
        }
        .validate()
        .is_err());
        assert!(Publish {
            name: String::new(),
            host_port: 80,
            guest_port: 80,
            protocol: "sctp".to_string(),
            bind: None,
        }
        .validate()
        .is_err());
        assert!(Publish {
            name: String::new(),
            host_port: 80,
            guest_port: 80,
            protocol: "tcp".to_string(),
            bind: Some("not-an-ip".to_string()),
        }
        .validate()
        .is_err());
    }

    #[test]
    fn publish_protocol_defaults_to_tcp() {
        let p: Publish = toml::from_str("host_port = 80\nguest_port = 80\n").unwrap();
        assert_eq!(p.protocol, "tcp");
        assert!(p.bind.is_none());
    }

    #[test]
    fn publish_effective_name_falls_back_to_port_and_proto() {
        // A nameless publish (from `spawn --publish` or pre-names TOML) is still
        // addressable by a deterministic handle.
        let p: Publish = toml::from_str("host_port = 9119\nguest_port = 9119\n").unwrap();
        assert!(p.name.is_empty());
        assert_eq!(p.effective_name(), "9119-tcp");
        // A named publish keeps its name.
        let named: Publish =
            toml::from_str("name = \"dashboard\"\nhost_port = 80\nguest_port = 80\n").unwrap();
        assert_eq!(named.effective_name(), "dashboard");
        // The name round-trips (and is only serialized when set).
        let text = toml::to_string_pretty(&named).unwrap();
        assert!(text.contains("name = \"dashboard\""));
        assert!(!toml::to_string_pretty(&p).unwrap().contains("name ="));
    }

    #[test]
    fn service_with_publish_round_trips_through_toml() {
        let mut svc: Service = toml::from_str(
            r#"
name = "hermes"
enabled = false
image = "hermes-vm"
cpu = 4
memory_mib = 4096
disk_gib = 32
vsock_cid = 100
mac = "52:54:00:12:34:56"
"#,
        )
        .unwrap();
        svc.publish.push(Publish {
            name: String::new(),
            host_port: 9119,
            guest_port: 9119,
            protocol: "tcp".to_string(),
            bind: Some("100.121.19.41".to_string()),
        });
        let text = toml::to_string_pretty(&svc).unwrap();
        let parsed: Service = toml::from_str(&text).unwrap();
        assert_eq!(parsed.publish.len(), 1);
        assert_eq!(parsed.publish[0].host_port, 9119);
        assert_eq!(parsed.publish[0].bind.as_deref(), Some("100.121.19.41"));
    }

    #[test]
    fn service_without_provision_section_still_parses() {
        let text = r#"
name = "mail"
enabled = true
image = "base"
cpu = 2
memory_mib = 2048
disk_gib = 20
vsock_cid = 100
mac = "52:54:00:12:34:56"

"#;
        let svc: Service = toml::from_str(text).unwrap();
        assert!(svc.disk.is_none());
        assert!(svc.provision.reset_machine_id);
        assert!(svc.provision.files.is_empty());
    }

    #[test]
    fn service_with_provision_round_trips_through_toml() {
        let mut svc: Service = toml::from_str(
            r#"
name = "hermes"
enabled = false
image = "hermes-vm"
cpu = 4
memory_mib = 4096
disk_gib = 32
vsock_cid = 100
mac = "52:54:00:12:34:56"
"#,
        )
        .unwrap();
        svc.disk = Some("hermes.raw".to_string());
        svc.provision.hostname = "hermes".to_string();
        svc.provision.reset_ssh_hostkeys = true;
        svc.provision.files.push(literal_file(
            "/home/agent/.hermes/.env",
            "0600",
            "1000:1000",
        ));
        let text = toml::to_string_pretty(&svc).unwrap();
        let parsed: Service = toml::from_str(&text).unwrap();
        assert_eq!(parsed.disk.as_deref(), Some("hermes.raw"));
        assert_eq!(parsed.provision.hostname, "hermes");
        assert!(parsed.provision.reset_ssh_hostkeys);
        assert_eq!(parsed.provision.files.len(), 1);
        assert_eq!(
            parsed.provision.files[0].dest,
            Utf8PathBuf::from("/home/agent/.hermes/.env")
        );
    }
}
