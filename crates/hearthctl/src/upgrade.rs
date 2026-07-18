//! `hearthctl upgrade [vm]`: copy the packaged hearth-guestd into existing,
//! running guestd VMs over the operator's ordinary SSH agent connection.

use crate::client;
use anyhow::{anyhow, bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use hearth_proto::{empty_args, Verb};
use serde::Serialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    time::{sleep, Instant},
};

const REPORT_TIMEOUT: Duration = Duration::from_secs(30);

const PREFLIGHT_SCRIPT: &str = r#"
set -eu
dest=/usr/local/bin/hearth-guestd

test -x "$dest"
current=$(sha256sum "$dest")
current=${current%% *}

active=no
for meta in /var/lib/hearth-guestd/tasks/*/meta.toml; do
    test -f "$meta" || continue
    if grep -Eq '^[[:space:]]*state[[:space:]]*=[[:space:]]*"(queued|running)"[[:space:]]*$' "$meta"; then
        active=yes
        break
    fi
done

echo "HEARTH_UPGRADE_SHA256=$current"
echo "HEARTH_UPGRADE_ACTIVE=$active"
"#;

const INSTALL_SCRIPT: &str = r#"
set -eu
umask 077
dest=/usr/local/bin/hearth-guestd
backup=/usr/local/bin/hearth-guestd.previous
tmp="${dest}.upgrade.$$"
backup_tmp="${backup}.tmp.$$"
cleanup() { rm -f "$tmp" "$backup_tmp"; }
trap cleanup 0 1 2 3 15

test -x "$dest"
cat > "$tmp"
chmod 0755 "$tmp"
actual=$(sha256sum "$tmp")
actual=${actual%% *}
if test "$actual" != "$1"; then
    echo "uploaded hearth-guestd checksum mismatch: expected $1, got $actual" >&2
    exit 1
fi
"$tmp" --version >/dev/null

current=$(sha256sum "$dest")
current=${current%% *}
if test "$current" = "$1"; then
    echo HEARTH_UPGRADE_UNCHANGED
    exit 0
fi

cp -p "$dest" "$backup_tmp"
mv -f "$backup_tmp" "$backup"
mv -f "$tmp" "$dest"
if systemctl restart hearth-guestd.service && systemctl is-active --quiet hearth-guestd.service; then
    echo HEARTH_UPGRADE_UPGRADED
    exit 0
fi

echo "new hearth-guestd failed to start; restoring previous binary" >&2
cp -p "$backup" "$tmp"
mv -f "$tmp" "$dest"
systemctl restart hearth-guestd.service || true
exit 1
"#;

const ROLLBACK_SCRIPT: &str = r#"
set -eu
umask 077
dest=/usr/local/bin/hearth-guestd
backup=/usr/local/bin/hearth-guestd.previous
tmp="${dest}.rollback.$$"
cleanup() { rm -f "$tmp"; }
trap cleanup 0 1 2 3 15

test -x "$backup"
cp -p "$backup" "$tmp"
mv -f "$tmp" "$dest"
systemctl restart hearth-guestd.service
systemctl is-active --quiet hearth-guestd.service
echo HEARTH_UPGRADE_ROLLED_BACK
"#;

#[derive(Debug)]
struct Payload {
    path: Utf8PathBuf,
    bytes: Vec<u8>,
    sha256: String,
    version: String,
}

#[derive(Debug, Clone)]
struct Service {
    name: String,
    running: bool,
}

#[derive(Debug)]
struct Target {
    name: String,
    address: String,
    previous_version: String,
    previous_last_seen: String,
}

#[derive(Debug, PartialEq, Eq)]
struct RemotePreflight {
    sha256: String,
    active: bool,
}

#[derive(Debug, Serialize)]
struct Outcome {
    name: String,
    status: &'static str,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallResult {
    Upgraded,
    Unchanged,
}

pub async fn run(
    socket: &Utf8PathBuf,
    requested_name: Option<&str>,
    source: Option<&Utf8Path>,
    force: bool,
    json_output: bool,
) -> Result<()> {
    let payload = load_payload(source).await?;
    let services = load_services(socket).await?;
    let specific = requested_name.is_some();
    let selected = select_services(services, requested_name)?;
    let mut outcomes = Vec::new();

    for service in selected {
        if !service.running {
            let message = "VM is stopped";
            if specific {
                bail!("cannot upgrade {}: {message}", service.name);
            }
            outcomes.push(skipped(&service.name, message));
            continue;
        }

        let status = match status(socket, &service.name).await {
            Ok(status) => status,
            Err(err) if !specific => {
                outcomes.push(skipped(
                    &service.name,
                    format!("could not inspect VM: {err:#}"),
                ));
                continue;
            }
            Err(err) => return Err(err).with_context(|| format!("inspect {}", service.name)),
        };
        let target = match target_from_status(&service, &status) {
            Ok(target) => target,
            Err(reason) if !specific => {
                outcomes.push(skipped(&service.name, reason));
                continue;
            }
            Err(reason) => bail!("cannot upgrade {}: {reason}", service.name),
        };

        let preflight = match remote_preflight(Utf8Path::new("ssh"), &target.address).await {
            Ok(preflight) => preflight,
            Err(err) => {
                outcomes.push(failed(
                    &target.name,
                    format!("SSH preflight failed: {err:#}"),
                ));
                continue;
            }
        };
        if preflight.sha256 == payload.sha256 {
            outcomes.push(Outcome {
                name: target.name,
                status: "unchanged",
                message: format!(
                    "already at {} ({})",
                    payload.version,
                    short_hash(&payload.sha256)
                ),
            });
            continue;
        }

        if preflight.active && !force {
            let reason = "VM has a running or queued agent task; use --force to override";
            if specific {
                bail!("cannot upgrade {}: {reason}", target.name);
            }
            outcomes.push(skipped(&target.name, reason));
            continue;
        }

        match install(Utf8Path::new("ssh"), &target.address, &payload).await {
            Ok(InstallResult::Unchanged) => outcomes.push(Outcome {
                name: target.name,
                status: "unchanged",
                message: format!(
                    "already at {} ({})",
                    payload.version,
                    short_hash(&payload.sha256)
                ),
            }),
            Ok(InstallResult::Upgraded) => {
                match wait_for_report(
                    socket,
                    &target.name,
                    &payload.version,
                    &target.previous_last_seen,
                )
                .await
                {
                    Ok(()) => outcomes.push(Outcome {
                        name: target.name,
                        status: "upgraded",
                        message: format!(
                            "{} -> {} ({})",
                            target.previous_version,
                            payload.version,
                            short_hash(&payload.sha256)
                        ),
                    }),
                    Err(verify_err) => {
                        let message = match rollback(Utf8Path::new("ssh"), &target.address).await {
                            Ok(()) => format!(
                                "new guestd did not report healthy: {verify_err:#}; restored previous binary"
                            ),
                            Err(rollback_err) => format!(
                                "new guestd did not report healthy: {verify_err:#}; rollback also failed: {rollback_err:#}"
                            ),
                        };
                        outcomes.push(failed(&target.name, message));
                    }
                }
            }
            Err(err) => outcomes.push(failed(
                &target.name,
                format!("upgrade over SSH failed: {err:#}"),
            )),
        }
    }

    render(&payload, &outcomes, json_output)?;
    let failures = outcomes
        .iter()
        .filter(|outcome| outcome.status == "failed")
        .count();
    if failures != 0 {
        bail!("{failures} VM upgrade(s) failed");
    }
    Ok(())
}

async fn load_payload(source: Option<&Utf8Path>) -> Result<Payload> {
    let path = match source {
        Some(path) => path.to_owned(),
        None => {
            let executable = std::env::current_exe().context("resolve hearthctl executable")?;
            let executable = Utf8PathBuf::from_path_buf(executable).map_err(|path| {
                anyhow!("hearthctl executable path is not UTF-8: {}", path.display())
            })?;
            payload_path_for_executable(&executable)?
        }
    };
    let bytes = tokio::fs::read(&path)
        .await
        .with_context(|| format!("read guest payload {path}; pass --from PATH to override"))?;
    if bytes.is_empty() {
        bail!("guest payload {path} is empty");
    }
    let output = Command::new(path.as_str())
        .arg("--version")
        .output()
        .await
        .with_context(|| format!("run {path} --version"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if !stderr.trim().is_empty() {
            stderr.trim()
        } else {
            stdout.trim()
        };
        bail!(
            "{} --version failed with {}{}",
            path,
            output.status,
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        );
    }
    let banner = String::from_utf8(output.stdout).context("guest payload version is not UTF-8")?;
    let version = parse_payload_version(&banner)?;
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    Ok(Payload {
        path,
        bytes,
        sha256,
        version,
    })
}

fn parse_payload_version(banner: &str) -> Result<String> {
    Ok(banner
        .trim()
        .strip_prefix("hearth-guestd ")
        .filter(|version| !version.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "unexpected guest payload version banner: {:?}",
                banner.trim()
            )
        })?
        .to_string())
}

fn payload_path_for_executable(executable: &Utf8Path) -> Result<Utf8PathBuf> {
    let bindir = executable
        .parent()
        .ok_or_else(|| anyhow!("hearthctl executable has no parent directory: {executable}"))?;
    let prefix = bindir
        .parent()
        .ok_or_else(|| anyhow!("hearthctl executable has no PREFIX directory: {executable}"))?;
    Ok(prefix.join("lib/hearth/guest/hearth-guestd"))
}

async fn load_services(socket: &Utf8Path) -> Result<Vec<Service>> {
    let value = client::hearth_request(socket, Verb::Ls, empty_args()).await?;
    let services = value
        .get("services")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("malformed ls response: missing services"))?;
    services
        .iter()
        .map(|value| {
            Ok(Service {
                name: required_field(value, "hostname")?.to_string(),
                running: value
                    .get("running")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

fn select_services(services: Vec<Service>, requested_name: Option<&str>) -> Result<Vec<Service>> {
    let Some(name) = requested_name else {
        return Ok(services);
    };
    services
        .into_iter()
        .find(|service| service.name == name)
        .map(|service| vec![service])
        .ok_or_else(|| anyhow!("service.not_found: no such service {name}"))
}

async fn status(socket: &Utf8Path, name: &str) -> Result<Value> {
    let mut args = Map::new();
    args.insert("name".to_string(), json!(name));
    client::hearth_request(socket, Verb::Status, args).await
}

fn target_from_status(service: &Service, status: &Value) -> std::result::Result<Target, String> {
    let address = status
        .get("address")
        .and_then(Value::as_str)
        .filter(|address| !address.is_empty())
        .ok_or_else(|| "VM has no resolved address".to_string())?;
    let guestd = status
        .get("guestd")
        .ok_or_else(|| "VM has no reported guestd; retrofit is not supported".to_string())?;
    if guestd.get("connected").and_then(Value::as_bool) != Some(true) {
        return Err("guestd is not connected".to_string());
    }
    let previous_version = guestd
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| "guestd report has no version".to_string())?;
    let previous_last_seen = guestd
        .get("last_seen")
        .and_then(Value::as_str)
        .ok_or_else(|| "guestd report has no last_seen timestamp".to_string())?;
    Ok(Target {
        name: service.name.clone(),
        address: address.to_string(),
        previous_version: previous_version.to_string(),
        previous_last_seen: previous_last_seen.to_string(),
    })
}

async fn remote_preflight(ssh: &Utf8Path, address: &str) -> Result<RemotePreflight> {
    let command = sudo_script(PREFLIGHT_SCRIPT, &[]);
    let output = ssh_output(ssh, address, &command).await?;
    parse_preflight(&output.stdout)
}

fn parse_preflight(stdout: &[u8]) -> Result<RemotePreflight> {
    let stdout = std::str::from_utf8(stdout).context("remote preflight output is not UTF-8")?;
    let hash = stdout
        .lines()
        .find_map(|line| line.strip_prefix("HEARTH_UPGRADE_SHA256="))
        .ok_or_else(|| anyhow!("remote preflight returned no checksum marker"))?
        .to_string();
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("remote preflight returned invalid checksum {hash:?}");
    }
    let active = match stdout
        .lines()
        .find_map(|line| line.strip_prefix("HEARTH_UPGRADE_ACTIVE="))
    {
        Some("yes") => true,
        Some("no") => false,
        Some(value) => bail!("remote preflight returned invalid active marker {value:?}"),
        None => bail!("remote preflight returned no active-task marker"),
    };
    Ok(RemotePreflight {
        sha256: hash,
        active,
    })
}

async fn install(ssh: &Utf8Path, address: &str, payload: &Payload) -> Result<InstallResult> {
    let remote = sudo_script(INSTALL_SCRIPT, &[&payload.sha256]);
    let mut command = ssh_command(ssh, address, &remote);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("start ssh")?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("ssh stdin was not piped"))?;
    let write_result = async {
        stdin.write_all(&payload.bytes).await?;
        stdin.shutdown().await
    }
    .await;
    drop(stdin);
    let output = child.wait_with_output().await.context("wait for ssh")?;
    if !output.status.success() {
        bail!("{}", process_failure(&output));
    }
    write_result.context("stream guest payload to ssh")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.lines().any(|line| line == "HEARTH_UPGRADE_UPGRADED") {
        Ok(InstallResult::Upgraded)
    } else if stdout
        .lines()
        .any(|line| line == "HEARTH_UPGRADE_UNCHANGED")
    {
        Ok(InstallResult::Unchanged)
    } else {
        bail!("remote installer returned no completion marker")
    }
}

async fn rollback(ssh: &Utf8Path, address: &str) -> Result<()> {
    let remote = sudo_script(ROLLBACK_SCRIPT, &[]);
    let output = ssh_output(ssh, address, &remote).await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout
        .lines()
        .any(|line| line == "HEARTH_UPGRADE_ROLLED_BACK")
    {
        bail!("remote rollback returned no completion marker");
    }
    Ok(())
}

async fn wait_for_report(
    socket: &Utf8Path,
    name: &str,
    expected_version: &str,
    previous_last_seen: &str,
) -> Result<()> {
    let started = Instant::now();
    let mut last_observation = "no status received".to_string();
    while started.elapsed() < REPORT_TIMEOUT {
        sleep(Duration::from_millis(500)).await;
        match status(socket, name).await {
            Ok(value) => {
                let guestd = value.get("guestd").unwrap_or(&Value::Null);
                let connected = guestd
                    .get("connected")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let version = guestd
                    .get("version")
                    .and_then(Value::as_str)
                    .unwrap_or("<missing>");
                let last_seen = guestd
                    .get("last_seen")
                    .and_then(Value::as_str)
                    .unwrap_or("<missing>");
                if connected && version == expected_version && last_seen != previous_last_seen {
                    return Ok(());
                }
                last_observation =
                    format!("connected={connected}, version={version}, last_seen={last_seen}");
            }
            Err(err) => last_observation = format!("status failed: {err:#}"),
        }
    }
    bail!(
        "timed out after {}s waiting for version {expected_version}; last observation: {last_observation}",
        REPORT_TIMEOUT.as_secs()
    )
}

async fn ssh_output(
    ssh: &Utf8Path,
    address: &str,
    remote_command: &str,
) -> Result<std::process::Output> {
    let output = ssh_command(ssh, address, remote_command)
        .stdin(Stdio::null())
        .output()
        .await
        .context("run ssh")?;
    if !output.status.success() {
        bail!("{}", process_failure(&output));
    }
    Ok(output)
}

fn ssh_command(ssh: &Utf8Path, address: &str, remote_command: &str) -> Command {
    let mut command = Command::new(ssh.as_str());
    command
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-o")
        .arg("ServerAliveInterval=5")
        .arg("-o")
        .arg("ServerAliveCountMax=2")
        .arg(format!("agent@{address}"))
        .arg(remote_command)
        .kill_on_drop(true);
    command
}

fn sudo_script(script: &str, args: &[&str]) -> String {
    let mut command = format!("sudo -n sh -c {} sh", shell_quote(script));
    for arg in args {
        command.push(' ');
        command.push_str(&shell_quote(arg));
    }
    command
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn process_failure(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if !stderr.is_empty() { stderr } else { stdout };
    if detail.is_empty() {
        format!("ssh exited with {}", output.status)
    } else {
        format!("ssh exited with {}: {detail}", output.status)
    }
}

fn required_field<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("malformed service: missing {field}"))
}

fn skipped(name: &str, message: impl Into<String>) -> Outcome {
    Outcome {
        name: name.to_string(),
        status: "skipped",
        message: message.into(),
    }
}

fn failed(name: &str, message: impl Into<String>) -> Outcome {
    Outcome {
        name: name.to_string(),
        status: "failed",
        message: message.into(),
    }
}

fn short_hash(hash: &str) -> String {
    format!("sha256:{}", &hash[..12])
}

fn render(payload: &Payload, outcomes: &[Outcome], json_output: bool) -> Result<()> {
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "source": payload.path,
                "version": payload.version,
                "sha256": payload.sha256,
                "results": outcomes,
            }))?
        );
        return Ok(());
    }
    println!(
        "guest payload: {} {} ({})",
        payload.path,
        payload.version,
        short_hash(&payload.sha256)
    );
    for outcome in outcomes {
        println!(
            "{:<9} {} — {}",
            outcome.status, outcome.name, outcome.message
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_payload_from_installed_prefix() {
        let path = payload_path_for_executable(Utf8Path::new("/opt/hearth/bin/hearthctl")).unwrap();
        assert_eq!(
            path,
            Utf8PathBuf::from("/opt/hearth/lib/hearth/guest/hearth-guestd")
        );
    }

    #[test]
    fn shell_quote_handles_apostrophes() {
        assert_eq!(shell_quote("a'b"), "'a'\"'\"'b'");
    }

    #[test]
    fn parses_remote_preflight_markers() {
        let hash = "1".repeat(64);
        let output = format!("noise\nHEARTH_UPGRADE_SHA256={hash}\nHEARTH_UPGRADE_ACTIVE=yes\n");
        assert_eq!(
            parse_preflight(output.as_bytes()).unwrap(),
            RemotePreflight {
                sha256: hash,
                active: true,
            }
        );
    }

    #[test]
    fn parses_git_qualified_guest_version() {
        assert_eq!(
            parse_payload_version("hearth-guestd 0.1.0+8c14e42\n").unwrap(),
            "0.1.0+8c14e42"
        );
    }

    #[test]
    fn status_requires_existing_connected_guestd() {
        let service = Service {
            name: "dev".into(),
            running: true,
        };
        let status = json!({
            "ssh_access": "configured",
            "address": "10.26.8.16",
            "guestd": {
                "connected": true,
                "version": "0.1.0",
                "last_seen": "now"
            }
        });
        let target = target_from_status(&service, &status).unwrap();
        assert_eq!(target.address, "10.26.8.16");

        let unmanaged = json!({
            "ssh_access": "not_configured",
            "address": "10.26.8.16",
            "guestd": {
                "connected": true,
                "version": "0.1.0",
                "last_seen": "now"
            }
        });
        assert!(target_from_status(&service, &unmanaged).is_ok());

        let missing = json!({"ssh_access": "configured", "address": "10.26.8.16"});
        assert!(target_from_status(&service, &missing)
            .unwrap_err()
            .contains("retrofit is not supported"));
    }

    #[test]
    fn scripts_only_replace_binary_and_restart_existing_unit() {
        assert!(INSTALL_SCRIPT.contains("/usr/local/bin/hearth-guestd"));
        assert!(INSTALL_SCRIPT.contains("/usr/local/bin/hearth-guestd.previous"));
        assert!(INSTALL_SCRIPT.contains("systemctl restart hearth-guestd.service"));
        assert!(PREFLIGHT_SCRIPT.contains("/var/lib/hearth-guestd/tasks/*/meta.toml"));
        assert!(PREFLIGHT_SCRIPT.contains("(queued|running)"));
        assert!(!INSTALL_SCRIPT.contains("daemon-reload"));
        assert!(!INSTALL_SCRIPT.contains("systemctl enable"));
    }
}
