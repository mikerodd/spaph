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
    std::fs::write(dir.join("s.spaph"), "# comment\nplugin load /no/such/plugin.so\nplugin list\n").unwrap();
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

#[test]
fn connect_audio_with_unknown_plugin_id_reports_error_without_panic() {
    // Regression: `connect audio 0 1` (id 0 does not exist) used to panic.
    let dir = scratch("badid");
    std::fs::write(dir.join("s.spaph"), "audio connect 0 1\n").unwrap();
    std::fs::write(dir.join("c.json"), r#"{ "script": "s.spaph" }"#).unwrap();

    let out = spaph()
        .args(["--config", dir.join("c.json").to_str().unwrap(), "--osc-port", "0"])
        .output()
        .unwrap();

    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("panicked"), "must not panic: {err}");
    assert_eq!(out.status.code(), Some(1));
    assert!(err.contains("plugin 0 not found"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn midi_disconnect_targeted_unknown_reports_error() {
    let dir = scratch("mdis");
    std::fs::write(dir.join("s.spaph"), "midi disconnect 999 1\n").unwrap();
    std::fs::write(dir.join("c.json"), r#"{ "script": "s.spaph" }"#).unwrap();

    let out = spaph()
        .args(["--config", dir.join("c.json").to_str().unwrap(), "--osc-port", "0"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no matching MIDI connection"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn audio_disconnect_unknown_reports_error() {
    let dir = scratch("adis");
    std::fs::write(dir.join("s.spaph"), "audio disconnect 999\n").unwrap();
    std::fs::write(dir.join("c.json"), r#"{ "script": "s.spaph" }"#).unwrap();

    let out = spaph()
        .args(["--config", dir.join("c.json").to_str().unwrap(), "--osc-port", "0"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not connected to audio"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn midi_disconnect_all_in_script_ok() {
    let dir = scratch("mdisall");
    std::fs::write(dir.join("s.spaph"), "midi disconnect\n").unwrap();
    std::fs::write(dir.join("c.json"), r#"{ "script": "s.spaph" }"#).unwrap();

    let out = spaph()
        .args(["--config", dir.join("c.json").to_str().unwrap(), "--osc-port", "0"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn plugin_show_unknown_id_reports_error_without_panic() {
    // `plugin show` works for VST2 and VST3; an unknown id must report an error, not panic.
    let dir = scratch("pshow");
    std::fs::write(dir.join("s.spaph"), "plugin show 99\n").unwrap();
    std::fs::write(dir.join("c.json"), r#"{ "script": "s.spaph" }"#).unwrap();

    let out = spaph()
        .args(["--config", dir.join("c.json").to_str().unwrap(), "--osc-port", "0"])
        .output()
        .unwrap();

    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("panicked"), "must not panic: {err}");
    assert_eq!(out.status.code(), Some(1));
    assert!(err.contains("plugin 99 not found"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn plugin_show_without_id_prints_usage() {
    let dir = scratch("pshowu");
    std::fs::write(dir.join("s.spaph"), "plugin show\n").unwrap();
    std::fs::write(dir.join("c.json"), r#"{ "script": "s.spaph" }"#).unwrap();

    let out = spaph()
        .args(["--config", dir.join("c.json").to_str().unwrap(), "--osc-port", "0"])
        .output()
        .unwrap();

    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("panicked"), "must not panic: {err}");
    assert!(err.contains("usage: plugin show <plugin_id>"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn plugin_preset_save_unknown_id_reports_error_without_panic() {
    let dir = scratch("psave");
    std::fs::write(dir.join("s.spaph"), "plugin preset save 99 out.fxp\n").unwrap();
    std::fs::write(dir.join("c.json"), r#"{ "script": "s.spaph" }"#).unwrap();

    let out = spaph()
        .args(["--config", dir.join("c.json").to_str().unwrap(), "--osc-port", "0"])
        .output()
        .unwrap();

    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("panicked"), "must not panic: {err}");
    assert_eq!(out.status.code(), Some(1));
    assert!(err.contains("plugin 99 not found"), "{err}");
    assert!(!dir.join("out.fxp").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unknown_audio_backend_in_config_exits_with_message() {
    let dir = scratch("badbackend");
    std::fs::write(dir.join("c.json"), r#"{ "audio": { "backend": "no-such-backend" } }"#).unwrap();

    let out = spaph()
        .args(["--config", dir.join("c.json").to_str().unwrap(), "--osc-port", "0"])
        .output()
        .unwrap();

    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("panicked"), "must not panic: {err}");
    assert_eq!(out.status.code(), Some(1));
    assert!(err.contains("unknown audio backend 'no-such-backend'"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
