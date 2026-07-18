use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::fmt;

/// Version shown by every shipped Hearth binary and protocol endpoint.
pub const VERSION: &str = env!("HEARTH_BUILD_VERSION");

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verb {
    Ping,
    Version,
    Ls,
    Status,
    Create,
    Rename,
    Destroy,
    Start,
    Stop,
    Restart,
    Reboot,
    Snapshot,
    Restore,
    Resize,
    Logs,
    ImageLs,
    ImageImport,
    ImageRm,
    NetSetup,
    NetTeardown,
    HostCheck,
    Publish,
    Unpublish,
    /// Block until a guestd boot report says the service is ready (§2.1 of
    /// docs/agent-plane.md). Only meaningful for images that declare guestd.
    Wait,
    /// Agent-plane discovery: services with `agent = true` plus their guestd
    /// telemetry.
    AgentEndpoints,
    /// Socket-broker verb: bind `<vm>.sock_<port>` and pass the listening fd
    /// (SCM_RIGHTS) to the caller.
    GuestListener,
    /// Socket-broker verb: connect `<vm>.sock` and pass the connected fd.
    GuestConnect,
}

impl Verb {
    /// Every protocol verb, in declaration order. `version_result` publishes the
    /// wire names from this list so a client can tell whether a daemon speaks a
    /// given verb; the exhaustiveness test keeps it from drifting out of sync
    /// with the enum.
    pub const ALL: &'static [Verb] = &[
        Verb::Ping,
        Verb::Version,
        Verb::Ls,
        Verb::Status,
        Verb::Create,
        Verb::Rename,
        Verb::Destroy,
        Verb::Start,
        Verb::Stop,
        Verb::Restart,
        Verb::Reboot,
        Verb::Snapshot,
        Verb::Restore,
        Verb::Resize,
        Verb::Logs,
        Verb::ImageLs,
        Verb::ImageImport,
        Verb::ImageRm,
        Verb::NetSetup,
        Verb::NetTeardown,
        Verb::HostCheck,
        Verb::Publish,
        Verb::Unpublish,
        Verb::Wait,
        Verb::AgentEndpoints,
        Verb::GuestListener,
        Verb::GuestConnect,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::Version => "version",
            Self::Ls => "ls",
            Self::Status => "status",
            Self::Create => "create",
            Self::Rename => "rename",
            Self::Destroy => "destroy",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Reboot => "reboot",
            Self::Snapshot => "snapshot",
            Self::Restore => "restore",
            Self::Resize => "resize",
            Self::Logs => "logs",
            Self::ImageLs => "image-ls",
            Self::ImageImport => "image-import",
            Self::ImageRm => "image-rm",
            Self::NetSetup => "net-setup",
            Self::NetTeardown => "net-teardown",
            Self::HostCheck => "host-check",
            Self::Publish => "publish",
            Self::Unpublish => "unpublish",
            Self::Wait => "wait",
            Self::AgentEndpoints => "agent-endpoints",
            Self::GuestListener => "guest-listener",
            Self::GuestConnect => "guest-connect",
        }
    }
}

impl fmt::Display for Verb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub verb: Verb,
    #[serde(default)]
    pub args: Map<String, Value>,
}

impl Request {
    pub fn new(id: impl Into<String>, verb: Verb, args: Map<String, Value>) -> Self {
        Self {
            id: id.into(),
            verb,
            args,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<StreamKind>,
}

impl Response {
    pub fn success(id: impl Into<String>, result: impl Into<Value>) -> Self {
        Self {
            id: id.into(),
            ok: true,
            result: Some(result.into()),
            error: None,
            stream: None,
        }
    }

    pub fn stream_data(id: impl Into<String>, data: impl Into<Value>) -> Self {
        Self {
            id: id.into(),
            ok: true,
            result: Some(data.into()),
            error: None,
            stream: Some(StreamKind::Data),
        }
    }

    pub fn stream_end(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ok: true,
            result: None,
            error: None,
            stream: Some(StreamKind::End),
        }
    }

    pub fn failure(
        id: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            ok: false,
            result: None,
            error: Some(ErrorBody {
                code: code.into(),
                message: message.into(),
            }),
            stream: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StreamKind {
    Data,
    End,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

pub fn empty_args() -> Map<String, Value> {
    Map::new()
}

pub fn object_arg(key: &str, value: impl Into<Value>) -> Map<String, Value> {
    let mut args = Map::new();
    args.insert(key.to_string(), value.into());
    args
}

pub fn version_result(crate_version: &str) -> Value {
    json!({
        "protocol": PROTOCOL_VERSION,
        "version": crate_version,
        "verbs": Verb::ALL.iter().map(|verb| verb.as_str()).collect::<Vec<_>>(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OciProcess {
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default = "default_cwd")]
    pub cwd: String,
}

impl OciProcess {
    pub fn validate_common(&mut self) -> Result<(), String> {
        if self.args.is_empty() || self.args[0].is_empty() {
            return Err("OCI config process.args must contain an executable".to_string());
        }
        if self.cwd.is_empty() {
            self.cwd = default_cwd();
        }
        if !self.cwd.starts_with('/') {
            return Err(format!(
                "OCI config process.cwd must be absolute: {}",
                self.cwd
            ));
        }
        for env in &self.env {
            if env.as_bytes().contains(&0) {
                return Err("OCI config process.env contains a NUL byte".to_string());
            }
        }
        for arg in &self.args {
            if arg.as_bytes().contains(&0) {
                return Err("OCI config process.args contains a NUL byte".to_string());
            }
        }
        Ok(())
    }

    pub fn validate_init_only(&mut self) -> Result<(), String> {
        self.validate_common()?;
        if self.args.len() != 1 {
            return Err(
                "Dockerfile VM images currently require exactly one OCI process arg".to_string(),
            );
        }
        validate_kernel_cmdline_path("OCI config process.args[0]", &self.args[0])?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageManifest {
    pub version: u32,
    pub root_device: String,
    pub root_fstype: String,
    pub init: String,
    /// Minimum guest-kernel contract this image needs. hearthd refuses to boot
    /// it against a kernel whose contract is lower. Defaults to 1 (the original
    /// contract) so older manifests keep booting. Kept before `oci` so it
    /// serializes as a top-level scalar, not inside the `[oci]` table.
    #[serde(default = "default_min_kernel_contract")]
    pub min_kernel_contract: u32,
    /// Whether the image carries hearth-guestd (docs/agent-plane.md §2.5).
    /// Declared, not guessed: only guestd-declaring images may back
    /// `agent = true` services, and only they get boot-report readiness —
    /// everything else waits on a serial marker exactly as before. Kept before
    /// `oci` so it serializes as a top-level scalar.
    #[serde(default)]
    pub guestd: bool,
    pub oci: OciProcess,
}

impl ImageManifest {
    pub fn from_oci_process(process: OciProcess) -> Result<Self, String> {
        let mut process = process;
        process.validate_init_only()?;
        Ok(Self {
            version: 1,
            root_device: "/dev/vda".to_string(),
            root_fstype: "ext4".to_string(),
            init: process.args[0].clone(),
            min_kernel_contract: default_min_kernel_contract(),
            guestd: false,
            oci: process,
        })
    }

    pub fn validate(&mut self) -> Result<(), String> {
        if self.version != 1 {
            return Err(format!(
                "unsupported Hearth image manifest version {}",
                self.version
            ));
        }
        validate_kernel_cmdline_path("root_device", &self.root_device)?;
        validate_kernel_cmdline_token("root_fstype", &self.root_fstype)?;
        validate_kernel_cmdline_path("init", &self.init)?;
        self.oci.validate_init_only()?;
        if self.oci.args[0] != self.init {
            return Err("manifest init must match oci.args[0]".to_string());
        }
        Ok(())
    }
}

fn default_cwd() -> String {
    "/".to_string()
}

fn default_min_kernel_contract() -> u32 {
    1
}

fn validate_kernel_cmdline_path(label: &str, value: &str) -> Result<(), String> {
    if !value.starts_with('/') {
        return Err(format!("{label} must be an absolute path: {value}"));
    }
    validate_kernel_cmdline_token(label, value)
}

fn validate_kernel_cmdline_token(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    if value.chars().any(char::is_whitespace) {
        return Err(format!("{label} must not contain whitespace: {value}"));
    }
    if value.as_bytes().contains(&0) {
        return Err(format!("{label} must not contain a NUL byte"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verb_all_has_enum_cardinality_and_unique_wire_names() {
        // Exhaustive match: adding a `Verb` variant fails to compile here until
        // it is handled, a reminder to also extend `Verb::ALL`. The length
        // assertion then pins the count.
        fn witness(verb: &Verb) {
            match verb {
                Verb::Ping
                | Verb::Version
                | Verb::Ls
                | Verb::Status
                | Verb::Create
                | Verb::Rename
                | Verb::Destroy
                | Verb::Start
                | Verb::Stop
                | Verb::Restart
                | Verb::Reboot
                | Verb::Snapshot
                | Verb::Restore
                | Verb::Resize
                | Verb::Logs
                | Verb::ImageLs
                | Verb::ImageImport
                | Verb::ImageRm
                | Verb::NetSetup
                | Verb::NetTeardown
                | Verb::HostCheck
                | Verb::Publish
                | Verb::Unpublish
                | Verb::Wait
                | Verb::AgentEndpoints
                | Verb::GuestListener
                | Verb::GuestConnect => {}
            }
        }
        for verb in Verb::ALL {
            witness(verb);
        }
        assert_eq!(Verb::ALL.len(), 27, "Verb::ALL must list every variant");
        let mut names: Vec<&str> = Verb::ALL.iter().map(|verb| verb.as_str()).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "Verb::ALL has duplicate wire names");
    }

    #[test]
    fn version_result_lists_all_verb_wire_names() {
        let value = version_result("9.9.9");
        let verbs = value.get("verbs").and_then(Value::as_array).unwrap();
        assert_eq!(verbs.len(), Verb::ALL.len());
        assert!(verbs.iter().any(|verb| verb == "image-import"));
        assert_eq!(value.get("version").and_then(Value::as_str), Some("9.9.9"));
    }

    #[test]
    fn image_manifest_uses_absolute_init_as_pid_one() {
        let manifest = ImageManifest::from_oci_process(OciProcess {
            args: vec!["/usr/local/bin/init".to_string()],
            env: vec!["EXEUNTU=1".to_string()],
            cwd: "/home/exedev".to_string(),
        })
        .unwrap();

        assert_eq!(manifest.init, "/usr/local/bin/init");
        assert_eq!(manifest.root_device, "/dev/vda");
        assert_eq!(manifest.root_fstype, "ext4");
    }

    #[test]
    fn image_manifest_rejects_extra_args_for_now() {
        let err = ImageManifest::from_oci_process(OciProcess {
            args: vec!["/init".to_string(), "--debug".to_string()],
            env: Vec::new(),
            cwd: "/".to_string(),
        })
        .unwrap_err();

        assert!(err.contains("exactly one"));
    }

    #[test]
    fn manifest_min_kernel_contract_defaults_to_one_when_absent() {
        let manifest: ImageManifest = serde_json::from_str(
            r#"{
                "version": 1,
                "root_device": "/dev/vda",
                "root_fstype": "ext4",
                "init": "/usr/local/bin/init",
                "oci": { "args": ["/usr/local/bin/init"] }
            }"#,
        )
        .unwrap();
        assert_eq!(manifest.min_kernel_contract, 1);
    }

    #[test]
    fn manifest_min_kernel_contract_round_trips() {
        let mut manifest = ImageManifest::from_oci_process(OciProcess {
            args: vec!["/usr/local/bin/init".to_string()],
            env: Vec::new(),
            cwd: "/".to_string(),
        })
        .unwrap();
        manifest.min_kernel_contract = 3;
        let text = serde_json::to_string(&manifest).unwrap();
        let parsed: ImageManifest = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.min_kernel_contract, 3);
    }

    #[test]
    fn image_manifest_rejects_relative_init() {
        let err = ImageManifest::from_oci_process(OciProcess {
            args: vec!["init".to_string()],
            env: Vec::new(),
            cwd: "/".to_string(),
        })
        .unwrap_err();

        assert!(err.contains("absolute"));
    }
}
