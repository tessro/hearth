use crate::{config::Config, error::coded};
use anyhow::{anyhow, bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use rand::Rng;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use tokio::fs;

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    #[serde(default)]
    pub cloud_init: CloudInit,
    #[serde(default)]
    pub restart: RestartPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudInit {
    pub hostname: String,
    #[serde(default)]
    pub ssh_keys: Vec<String>,
    #[serde(default = "default_user")]
    pub user: String,
}

impl Default for CloudInit {
    fn default() -> Self {
        Self {
            hostname: String::new(),
            ssh_keys: Vec::new(),
            user: default_user(),
        }
    }
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
            let mut svc: Service =
                toml::from_str(&text).with_context(|| format!("parse {path}"))?;
            if svc.cloud_init.hostname.is_empty() {
                svc.cloud_init.hostname = svc.name.clone();
            }
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

    pub fn allocate(&mut self, name: &str) -> (u32, String) {
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
        self.allocations.vsock_cids.insert(name.to_string(), cid);
        self.allocations.macs.insert(name.to_string(), mac.clone());
        (cid, mac)
    }

    pub fn free(&mut self, name: &str) {
        self.allocations.vsock_cids.remove(name);
        self.allocations.macs.remove(name);
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
    fs::rename(&tmp, path).await?;
    Ok(())
}

fn default_user() -> String {
    "agent".to_string()
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
