use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::fmt;

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verb {
    Ping,
    Version,
    Ls,
    Status,
    Create,
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
    ImagePull,
    ImageRm,
    NetSetup,
    NetTeardown,
    HostCheck,
}

impl Verb {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::Version => "version",
            Self::Ls => "ls",
            Self::Status => "status",
            Self::Create => "create",
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
            Self::ImagePull => "image-pull",
            Self::ImageRm => "image-rm",
            Self::NetSetup => "net-setup",
            Self::NetTeardown => "net-teardown",
            Self::HostCheck => "host-check",
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
    })
}
