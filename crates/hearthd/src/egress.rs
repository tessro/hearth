//! Host-managed outbound proxy settings installed into each new VM.
//!
//! This config contains no provider secret. The guest gets a public CA,
//! proxy addresses, and optional public placeholder values. Stalin keeps and
//! injects the real provider credentials on the host.

use crate::registry::{Provision, ProvisionFile};
use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::IpAddr,
};
use tokio::{
    fs,
    net::TcpStream,
    time::{timeout, Duration},
};

pub const GUEST_CA_CERT: &str = "/usr/local/share/ca-certificates/hearth-egress.crt";
const GUEST_CA_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";
const GUEST_ENV_FILE: &str = "/etc/hearth/egress.env";
const GUEST_PROFILE: &str = "/etc/profile.d/hearth-egress.sh";
const GUEST_SYSTEM_ENV: &str = "/etc/systemd/system.conf.d/20-hearth-egress.conf";
const GUEST_USER_ENV: &str = "/etc/environment.d/20-hearth-egress.conf";
const RESERVED_DESTINATIONS: &[&str] = &[
    GUEST_CA_CERT,
    GUEST_ENV_FILE,
    GUEST_PROFILE,
    GUEST_SYSTEM_ENV,
    GUEST_USER_ENV,
];
const RESERVED_ENVIRONMENT: &[&str] = &[
    "http_proxy",
    "https_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "no_proxy",
    "NO_PROXY",
    "SSL_CERT_FILE",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
    "GIT_SSL_CAINFO",
    "HEARTH_EGRESS_PASSTHROUGH",
];
const FORBIDDEN_PLACEHOLDER_ENVIRONMENT: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "PATH",
    "SHELL",
    "XDG_RUNTIME_DIR",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "PYTHONHOME",
    "PYTHONPATH",
    "NODE_OPTIONS",
    "BASH_ENV",
    "ENV",
];

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EgressConfig {
    pub proxy_url: String,
    pub ca_cert: Utf8PathBuf,
    #[serde(default = "default_no_proxy")]
    pub no_proxy: String,
    /// Public values used only to satisfy clients that refuse to start without
    /// a provider variable. Stalin replaces matching request credentials.
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyEndpoint {
    pub authority: String,
}

impl EgressConfig {
    pub fn validate(&self) -> Result<ProxyEndpoint> {
        let endpoint = parse_proxy_url(&self.proxy_url)?;
        if !self.ca_cert.is_absolute() {
            bail!("ca_cert must be an absolute host path");
        }
        validate_value("no_proxy", &self.no_proxy)?;
        let reserved: BTreeSet<&str> = RESERVED_ENVIRONMENT.iter().copied().collect();
        let forbidden: BTreeSet<&str> = FORBIDDEN_PLACEHOLDER_ENVIRONMENT.iter().copied().collect();
        for (name, value) in &self.environment {
            validate_environment_name(name)?;
            if reserved.contains(name.as_str()) || forbidden.contains(name.as_str()) {
                bail!("environment key {name} is reserved");
            }
            validate_value(name, value)?;
        }
        Ok(endpoint)
    }

    fn guest_environment(&self) -> BTreeMap<String, String> {
        let mut environment = BTreeMap::from([
            ("http_proxy".to_string(), self.proxy_url.clone()),
            ("https_proxy".to_string(), self.proxy_url.clone()),
            ("HTTP_PROXY".to_string(), self.proxy_url.clone()),
            ("HTTPS_PROXY".to_string(), self.proxy_url.clone()),
            ("no_proxy".to_string(), self.no_proxy.clone()),
            ("NO_PROXY".to_string(), self.no_proxy.clone()),
            ("SSL_CERT_FILE".to_string(), GUEST_CA_BUNDLE.to_string()),
            (
                "REQUESTS_CA_BUNDLE".to_string(),
                GUEST_CA_BUNDLE.to_string(),
            ),
            ("CURL_CA_BUNDLE".to_string(), GUEST_CA_BUNDLE.to_string()),
            ("NODE_EXTRA_CA_CERTS".to_string(), GUEST_CA_CERT.to_string()),
            ("GIT_SSL_CAINFO".to_string(), GUEST_CA_BUNDLE.to_string()),
        ]);
        environment.extend(self.environment.clone());
        environment.insert(
            "HEARTH_EGRESS_PASSTHROUGH".to_string(),
            self.environment
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(","),
        );
        environment
    }
}

pub async fn load(path: &Utf8Path) -> Result<EgressConfig> {
    let text = fs::read_to_string(path)
        .await
        .with_context(|| format!("read egress config {path}"))?;
    let config: EgressConfig =
        toml::from_str(&text).with_context(|| format!("parse egress config {path}"))?;
    config
        .validate()
        .with_context(|| format!("validate egress config {path}"))?;
    Ok(config)
}

pub async fn validate_startup(path: Option<&Utf8Path>) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    let config = load(path).await?;
    read_ca_certificate(&config).await?;
    Ok(())
}

pub async fn augment_provision(path: &Utf8Path, provision: &mut Provision) -> Result<()> {
    let config = load(path).await?;
    let ca = read_ca_certificate(&config).await?;
    let reserved: BTreeSet<&str> = RESERVED_DESTINATIONS.iter().copied().collect();
    if let Some(conflict) = provision
        .files
        .iter()
        .find(|file| reserved.contains(file.dest.as_str()))
    {
        bail!(
            "provision destination {} is reserved for host egress settings",
            conflict.dest
        );
    }

    let environment = config.guest_environment();
    let env_file = render_environment_file(&environment);
    let profile = render_profile(&environment);
    let system = render_system_environment(&environment);
    provision.files.extend([
        managed_file(GUEST_CA_CERT, ca),
        managed_file(GUEST_ENV_FILE, env_file.clone()),
        managed_file(GUEST_PROFILE, profile),
        managed_file(GUEST_SYSTEM_ENV, system),
        managed_file(GUEST_USER_ENV, env_file),
    ]);
    Ok(())
}

pub async fn checks(path: &Utf8Path) -> Vec<Value> {
    let config = match load(path).await {
        Ok(config) => config,
        Err(_) => {
            return vec![json!({
                "name": "egress_config",
                "path": path,
                "ok": false,
                "error": "missing or invalid",
            })]
        }
    };
    let ca_ok = read_ca_certificate(&config).await.is_ok();
    let endpoint = config
        .validate()
        .expect("load already validated the egress config");
    let proxy_ok = timeout(
        Duration::from_secs(2),
        TcpStream::connect(&endpoint.authority),
    )
    .await
    .is_ok_and(|result| result.is_ok());
    vec![
        json!({ "name": "egress_config", "path": path, "ok": true }),
        json!({
            "name": "egress_ca_certificate",
            "path": config.ca_cert,
            "ok": ca_ok,
        }),
        json!({
            "name": "egress_proxy",
            "address": endpoint.authority,
            "ok": proxy_ok,
        }),
    ]
}

fn managed_file(dest: &str, from_literal: String) -> ProvisionFile {
    ProvisionFile {
        from_literal,
        dest: Utf8PathBuf::from(dest),
        mode: "0644".to_string(),
        owner: "0:0".to_string(),
    }
}

async fn read_ca_certificate(config: &EgressConfig) -> Result<String> {
    let ca = fs::read_to_string(&config.ca_cert)
        .await
        .with_context(|| format!("read egress CA certificate {}", config.ca_cert))?;
    if !ca.contains("-----BEGIN CERTIFICATE-----") || !ca.contains("-----END CERTIFICATE-----") {
        bail!("egress CA file does not contain a PEM certificate");
    }
    Ok(ca)
}

fn parse_proxy_url(value: &str) -> Result<ProxyEndpoint> {
    let authority = value
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("proxy_url must use http://"))?;
    if authority.is_empty()
        || authority
            .chars()
            .any(|character| matches!(character, '/' | '?' | '#' | '@'))
        || authority.chars().any(char::is_whitespace)
    {
        bail!("proxy_url must contain only a host and port");
    }
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("proxy_url must include a port"))?;
    let bracketed = host.starts_with('[') && host.ends_with(']');
    let bare_host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let valid_host = if bracketed {
        matches!(bare_host.parse::<IpAddr>(), Ok(IpAddr::V6(_)))
    } else if host.contains(':') {
        false
    } else {
        bare_host.parse::<IpAddr>().is_ok()
            || (!bare_host.is_empty()
                && bare_host.len() <= 253
                && bare_host.split('.').all(|label| {
                    !label.is_empty()
                        && label.len() <= 63
                        && !label.starts_with('-')
                        && !label.ends_with('-')
                        && label
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                }))
    };
    let parsed_port = port.parse::<u16>();
    if !valid_host || parsed_port.is_err() || parsed_port == Ok(0) {
        bail!("proxy_url must contain a valid host and port");
    }
    Ok(ProxyEndpoint {
        authority: authority.to_string(),
    })
}

fn validate_environment_name(name: &str) -> Result<()> {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        bail!("environment key must not be empty");
    };
    if !(first.is_ascii_alphabetic() || first == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        bail!("environment key {name:?} is not a valid variable name");
    }
    Ok(())
}

fn validate_value(name: &str, value: &str) -> Result<()> {
    if value.chars().any(char::is_control) {
        bail!("{name} contains a forbidden control character");
    }
    Ok(())
}

fn render_environment_file(environment: &BTreeMap<String, String>) -> String {
    environment
        .iter()
        .map(|(name, value)| format!("{name}=\"{}\"\n", escape_double_quoted(value)))
        .collect()
}

fn render_profile(environment: &BTreeMap<String, String>) -> String {
    let mut rendered = String::from("# Managed by Hearth. Contains no provider secrets.\n");
    for (name, value) in environment {
        rendered.push_str("export ");
        rendered.push_str(name);
        rendered.push_str("='");
        rendered.push_str(&value.replace('\'', "'\"'\"'"));
        rendered.push_str("'\n");
    }
    rendered
}

fn render_system_environment(environment: &BTreeMap<String, String>) -> String {
    let mut rendered = String::from("[Manager]\nDefaultEnvironment=");
    for (index, (name, value)) in environment.iter().enumerate() {
        if index > 0 {
            rendered.push(' ');
        }
        rendered.push('"');
        rendered.push_str(name);
        rendered.push('=');
        rendered.push_str(&escape_double_quoted(value));
        rendered.push('"');
    }
    rendered.push('\n');
    rendered
}

fn escape_double_quoted(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn default_no_proxy() -> String {
    "localhost,127.0.0.1,::1".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(root: &Utf8Path) -> EgressConfig {
        EgressConfig {
            proxy_url: "http://10.26.8.1:8080".to_string(),
            ca_cert: root.join("ca.pem"),
            no_proxy: "localhost,127.0.0.1,::1,10.26.8.0/24".to_string(),
            environment: BTreeMap::from([
                (
                    "ANTHROPIC_API_KEY".to_string(),
                    "stalin-managed".to_string(),
                ),
                ("OPENAI_API_KEY".to_string(), "stalin-managed".to_string()),
            ]),
        }
    }

    #[test]
    fn proxy_url_is_exact_http_authority() {
        assert!(parse_proxy_url("http://10.26.8.1:8080").is_ok());
        assert!(parse_proxy_url("https://10.26.8.1:8080").is_err());
        assert!(parse_proxy_url("http://10.26.8.1").is_err());
        assert!(parse_proxy_url("http://10.26.8.1:8080/path").is_err());
        assert!(parse_proxy_url("http://user@10.26.8.1:8080").is_err());
        assert!(parse_proxy_url("http://bad_host:8080").is_err());
        assert!(parse_proxy_url("http://[::1]:8080").is_ok());
    }

    #[test]
    fn placeholders_cannot_change_process_startup() {
        let root = Utf8Path::new("/tmp/test");
        let mut config = config(root);
        config
            .environment
            .insert("LD_PRELOAD".to_string(), "/tmp/inject.so".to_string());
        assert!(config.validate().is_err());
    }

    #[test]
    fn managed_environment_covers_process_types() {
        let root = Utf8Path::new("/tmp/test");
        let environment = config(root).guest_environment();
        assert_eq!(environment["http_proxy"], "http://10.26.8.1:8080");
        assert_eq!(environment["HTTPS_PROXY"], "http://10.26.8.1:8080");
        assert_eq!(environment["SSL_CERT_FILE"], GUEST_CA_BUNDLE);
        assert_eq!(environment["NODE_EXTRA_CA_CERTS"], GUEST_CA_CERT);
        assert_eq!(
            environment["HEARTH_EGRESS_PASSTHROUGH"],
            "ANTHROPIC_API_KEY,OPENAI_API_KEY"
        );
    }

    #[tokio::test]
    async fn provision_gets_public_ca_and_global_environment() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let config = config(&root);
        fs::write(
            &config.ca_cert,
            "-----BEGIN CERTIFICATE-----\ntest\n-----END CERTIFICATE-----\n",
        )
        .await
        .unwrap();
        let path = root.join("egress.toml");
        fs::write(&path, toml::to_string(&config).unwrap())
            .await
            .unwrap();
        let mut provision = Provision::default();
        augment_provision(&path, &mut provision).await.unwrap();
        assert_eq!(provision.files.len(), RESERVED_DESTINATIONS.len());
        let env = provision
            .files
            .iter()
            .find(|file| file.dest == Utf8Path::new(GUEST_ENV_FILE))
            .unwrap();
        assert!(env
            .from_literal
            .contains("http_proxy=\"http://10.26.8.1:8080\""));
        assert!(env
            .from_literal
            .contains("OPENAI_API_KEY=\"stalin-managed\""));
        assert!(!env.from_literal.contains("sk-"));
    }

    #[tokio::test]
    async fn user_cannot_replace_managed_egress_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let config = config(&root);
        fs::write(
            &config.ca_cert,
            "-----BEGIN CERTIFICATE-----\ntest\n-----END CERTIFICATE-----\n",
        )
        .await
        .unwrap();
        let path = root.join("egress.toml");
        fs::write(&path, toml::to_string(&config).unwrap())
            .await
            .unwrap();
        let mut provision = Provision::default();
        provision
            .files
            .push(managed_file(GUEST_ENV_FILE, "bad".to_string()));
        let error = augment_provision(&path, &mut provision).await.unwrap_err();
        assert!(error.to_string().contains("reserved"));
    }
}
