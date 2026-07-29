//! `lpctl web-password` — the only way to recover a password nobody wrote
//! down, since the configuration holds nothing but the Argon2id hash.
//!
//! The generation half is covered by `artd`'s end-to-end test; this covers
//! the recovery half, including the case that matters most in practice —
//! asking for a password on a device where it was never generated.

use std::process::Command;

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("lpctl-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn the_password_is_reprinted_from_beside_the_local_config() {
    let dir = scratch("reprint");
    std::fs::write(dir.join("web-password.txt"), "abcde-fghij-klmnp-qrstu\n").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_lpctl"))
        .arg("--config-local")
        .arg(dir.join("config.local.toml"))
        .arg("web-password")
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "abcde-fghij-klmnp-qrstu"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_password_file_says_where_it_would_have_been() {
    // Reached on a device where authentication is off, or where artd could
    // not write it — in which case the log line is the only copy, and saying
    // so is more use than "No such file or directory".
    let dir = scratch("missing");
    let out = Command::new(env!("CARGO_BIN_EXE_lpctl"))
        .arg("--config-local")
        .arg(dir.join("config.local.toml"))
        .arg("web-password")
        .output()
        .unwrap();

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("web-password.txt"), "{stderr}");
    assert!(stderr.contains("startup log"), "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reprinting_the_password_does_not_need_the_daemon() {
    // The socket does not exist here. Needing the password most likely means
    // the web interface is the thing that is not working.
    let dir = scratch("no-daemon");
    std::fs::write(dir.join("web-password.txt"), "xxxxx-xxxxx-xxxxx-xxxxx\n").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_lpctl"))
        .arg("--socket")
        .arg(dir.join("definitely-not-a-socket"))
        .arg("--config-local")
        .arg(dir.join("config.local.toml"))
        .arg("web-password")
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
