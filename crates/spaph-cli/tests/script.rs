//! End-to-end tests of the startup configuration and script behaviour.
//! They run the real binary; no plugin or audio device is needed.

use std::path::PathBuf;
use std::process::{Command, Stdio};

fn spaph() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_spaph"));
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    c
}

/// Fresh scratch directory per test, so tests can run in parallel.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("spaph_it_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn failing_script_line_exits_nonzero_with_location() {
    let dir = scratch("fail");
    std::fs::write(dir.join("s.spaph"), "# comment\nload /no/such/plugin.so\nlist plugins\n").unwrap();
    std::fs::write(dir.join("c.json"), r#"{ "script": "s.spaph" }"#).unwrap();

    let out = spaph()
        .args(["--config", dir.join("c.json").to_str().unwrap(), "--osc-port", "0"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("line 2"), "{err}");
    assert!(err.contains("load /no/such/plugin.so"), "{err}");
    assert!(err.contains("stopping"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unknown_command_in_script_stops_the_program() {
    let dir = scratch("unknown");
    std::fs::write(dir.join("s.spaph"), "frobnicate now\n").unwrap();
    std::fs::write(dir.join("c.json"), r#"{ "script": "s.spaph" }"#).unwrap();

    let out = spaph()
        .args(["--config", dir.join("c.json").to_str().unwrap(), "--osc-port", "0"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(1));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn quit_in_script_ends_script_but_not_program() {
    let dir = scratch("quit");
    // The second line must never run; the program still reaches the prompt and exits
    // normally when stdin reaches end-of-file.
    std::fs::write(dir.join("s.spaph"), "quit\nfrobnicate now\n").unwrap();
    std::fs::write(dir.join("c.json"), r#"{ "script": "s.spaph" }"#).unwrap();

    let out = spaph()
        .args(["--config", dir.join("c.json").to_str().unwrap(), "--osc-port", "0"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("frobnicate"), "second script line must not run: {err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn midi_listen_in_script_is_skipped_with_warning() {
    let dir = scratch("midi");
    std::fs::write(dir.join("s.spaph"), "midi listen 0\n").unwrap();
    std::fs::write(dir.join("c.json"), r#"{ "script": "s.spaph" }"#).unwrap();

    let out = spaph()
        .args(["--config", dir.join("c.json").to_str().unwrap(), "--osc-port", "0"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("interactive only"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn invalid_config_exits_with_message() {
    let dir = scratch("badcfg");
    std::fs::write(dir.join("c.json"), r#"{ "osc": { "prot": 1 } }"#).unwrap();

    let out = spaph()
        .args(["--config", dir.join("c.json").to_str().unwrap()])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unknown field"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_explicit_config_exits_with_message() {
    let out = spaph()
        .args(["--config", "/definitely/not/here.json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("/definitely/not/here.json"));
}
