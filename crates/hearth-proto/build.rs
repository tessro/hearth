use std::{env, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=HEARTH_RELEASE");
    println!("cargo:rerun-if-env-changed=HEARTH_GIT_SHA");
    rerun_when_git_head_changes();

    let base = env::var("CARGO_PKG_VERSION").expect("Cargo sets CARGO_PKG_VERSION");
    let release = env::var("HEARTH_RELEASE").as_deref() == Ok("1");
    let version = if release {
        base
    } else {
        let sha = env::var("HEARTH_GIT_SHA")
            .ok()
            .or_else(git_sha)
            .unwrap_or_else(|| "unknown".to_string());
        assert!(
            sha == "unknown"
                || (!sha.is_empty() && sha.bytes().all(|byte| byte.is_ascii_hexdigit())),
            "HEARTH_GIT_SHA must be a hexadecimal commit ID"
        );
        format!("{base}+{sha}")
    };
    println!("cargo:rustc-env=HEARTH_BUILD_VERSION={version}");
}

fn git_sha() -> Option<String> {
    git_output(&["rev-parse", "--short=7", "HEAD"])
}

fn rerun_when_git_head_changes() {
    for logical_path in ["HEAD", "packed-refs"] {
        if let Some(path) = git_output(&["rev-parse", "--git-path", logical_path]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(head_ref) = git_output(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git_output(&["rev-parse", "--git-path", &head_ref]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}
