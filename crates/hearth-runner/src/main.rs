use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::{
    env, fs,
    process::{Command, ExitCode},
};

#[derive(Debug, Deserialize)]
struct RunManifest {
    args: Vec<String>,
    #[serde(default)]
    env: Vec<String>,
    #[serde(default = "default_cwd")]
    cwd: String,
}

fn main() -> ExitCode {
    match run() {
        Ok(status) => ExitCode::from(status),
        Err(err) => {
            eprintln!("hearth-runner: {err:#}");
            ExitCode::from(126)
        }
    }
}

fn run() -> Result<u8> {
    let mut args = env::args();
    let _program = args.next();
    let root = args.next().unwrap_or_else(|| "/newroot".to_string());
    let manifest_path = args
        .next()
        .unwrap_or_else(|| "/newroot/.hearth/run.json".to_string());

    let manifest = read_manifest(&manifest_path)?;
    chroot(&root)?;
    env::set_current_dir(&manifest.cwd).with_context(|| format!("chdir {}", manifest.cwd))?;

    let mut command = Command::new(&manifest.args[0]);
    command.args(&manifest.args[1..]);
    command.env_clear();
    for value in &manifest.env {
        let (key, value) = value
            .split_once('=')
            .ok_or_else(|| anyhow!("invalid OCI env entry without '=': {value}"))?;
        command.env(key, value);
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("exec {}", manifest.args[0]))?;
    let status = child.wait().context("wait for OCI process")?;
    let code = if let Some(code) = status.code() {
        u8::try_from(code).unwrap_or(125)
    } else {
        128 + signal_of(&status).unwrap_or(0).min(127) as u8
    };
    write_exit_status(code)?;
    Ok(code)
}

fn read_manifest(path: &str) -> Result<RunManifest> {
    let text = fs::read_to_string(path).with_context(|| format!("read {path}"))?;
    let manifest: RunManifest =
        serde_json::from_str(&text).with_context(|| format!("parse {path}"))?;
    validate_manifest(manifest)
}

fn validate_manifest(mut manifest: RunManifest) -> Result<RunManifest> {
    if manifest.args.is_empty() || manifest.args[0].is_empty() {
        bail!("run manifest args must contain an executable");
    }
    if manifest.cwd.is_empty() {
        manifest.cwd = default_cwd();
    }
    if !manifest.cwd.starts_with('/') {
        bail!("run manifest cwd must be absolute: {}", manifest.cwd);
    }
    for value in manifest.args.iter().chain(manifest.env.iter()) {
        if value.as_bytes().contains(&0) {
            bail!("run manifest contains a NUL byte");
        }
    }
    Ok(manifest)
}

fn default_cwd() -> String {
    "/".to_string()
}

fn chroot(root: &str) -> Result<()> {
    let root = std::ffi::CString::new(root).context("chroot path contains NUL")?;
    let rc = unsafe { libc::chroot(root.as_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("chroot");
    }
    Ok(())
}

#[cfg(unix)]
fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn signal_of(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

fn write_exit_status(status: u8) -> Result<()> {
    fs::write("/.hearth/exit-status", format!("{status}\n")).context("write /.hearth/exit-status")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_manifest() {
        let manifest = validate_manifest(RunManifest {
            args: vec!["python3".to_string(), "-m".to_string()],
            env: vec!["PATH=/usr/bin:/bin".to_string()],
            cwd: "/srv".to_string(),
        })
        .unwrap();
        assert_eq!(manifest.args[0], "python3");
    }

    #[test]
    fn rejects_empty_args() {
        let err = validate_manifest(RunManifest {
            args: Vec::new(),
            env: Vec::new(),
            cwd: "/".to_string(),
        })
        .unwrap_err();
        assert!(err.to_string().contains("args"));
    }

    #[test]
    fn rejects_relative_cwd() {
        let err = validate_manifest(RunManifest {
            args: vec!["python3".to_string()],
            env: Vec::new(),
            cwd: "srv".to_string(),
        })
        .unwrap_err();
        assert!(err.to_string().contains("cwd"));
    }
}
