use anyhow::{anyhow, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use hearth_proto::OciProcess;
use serde::Deserialize;
use std::{env, fs, io, path::PathBuf, process::Stdio};
use tokio::process::Command;

const DEFAULT_DATA_DIR: &str = ".local/share/hearth";

#[derive(Debug, Deserialize)]
struct OciConfig {
    process: OciProcess,
}

pub fn data_dir() -> Result<Utf8PathBuf> {
    if let Some(value) = env::var_os("HEARTH_DATA_DIR") {
        return Utf8PathBuf::from_path_buf(PathBuf::from(value))
            .map_err(|path| anyhow!("HEARTH_DATA_DIR is not valid UTF-8: {}", path.display()));
    }
    let home = env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Utf8PathBuf::from_path_buf(PathBuf::from(home).join(DEFAULT_DATA_DIR))
        .map_err(|path| anyhow!("HOME is not valid UTF-8: {}", path.display()))
}

pub fn buildah_bud_args(name: &str, dockerfile: &Utf8Path, context: &Utf8Path) -> Vec<String> {
    vec![
        "bud".to_string(),
        "-t".to_string(),
        name.to_string(),
        "-f".to_string(),
        dockerfile.to_string(),
        context.to_string(),
    ]
}

pub fn buildah_push_args(name: &str, image_layout: &Utf8Path) -> Vec<String> {
    vec![
        "push".to_string(),
        name.to_string(),
        format!("oci:{image_layout}:latest"),
    ]
}

pub fn umoci_unpack_args(image_layout: &Utf8Path, bundle: &Utf8Path) -> Vec<String> {
    umoci_unpack_args_with_rootless(image_layout, bundle, true)
}

pub fn umoci_unpack_args_with_rootless(
    image_layout: &Utf8Path,
    bundle: &Utf8Path,
    rootless: bool,
) -> Vec<String> {
    let mut args = vec!["unpack".to_string()];
    if rootless {
        args.push("--rootless".to_string());
    }
    args.extend([
        "--image".to_string(),
        format!("{image_layout}:latest"),
        bundle.to_string(),
    ]);
    args
}

pub fn read_oci_process(bundle: &Utf8Path) -> Result<OciProcess> {
    let config_path = bundle.join("config.json");
    let text = fs::read_to_string(&config_path).with_context(|| format!("read {config_path}"))?;
    let config: OciConfig =
        serde_json::from_str(&text).with_context(|| format!("parse {config_path}"))?;
    validate_oci_process(config.process)
}

pub fn validate_oci_process(mut process: OciProcess) -> Result<OciProcess> {
    process
        .validate_common()
        .map_err(|message| anyhow!(message))?;
    Ok(process)
}

pub fn command(program: &str, args: Vec<String>) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    cmd
}

pub async fn run_status(mut cmd: Command, label: &str) -> Result<()> {
    eprintln!("hearthctl: {label}");
    let status = cmd
        .status()
        .await
        .with_context(|| format!("spawn {label}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("{label} exited with {status}"))
    }
}

pub fn parent(path: &Utf8Path) -> Result<&Utf8Path> {
    path.parent()
        .ok_or_else(|| anyhow!("path has no parent: {path}"))
}

pub fn remove_dir_if_exists(path: &Utf8Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

pub fn remove_file_if_exists(path: &Utf8Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}
