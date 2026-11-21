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

#[test]
fn plugin_hide_unknown_id_reports_error_without_panic() {
    let dir = scratch("phide");
    std::fs::write(dir.join("s.spaph"), "plugin hide 99\n").unwrap();
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
fn plugin_unload_all_with_no_plugins_succeeds() {
    let dir = scratch("unloadall");
    std::fs::write(dir.join("s.spaph"), "plugin unload all\n").unwrap();
    std::fs::write(dir.join("c.json"), r#"{ "script": "s.spaph" }"#).unwrap();

    let out = spaph()
        .args(["--config", dir.join("c.json").to_str().unwrap(), "--osc-port", "0"])
        .output()
        .unwrap();

    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("panicked"), "must not panic: {err}");
    assert_eq!(out.status.code(), Some(0), "{err}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("no plugins to unload"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn plugin_unload_without_id_prints_usage() {
    let dir = scratch("unloadu");
    std::fs::write(dir.join("s.spaph"), "plugin unload\n").unwrap();
    std::fs::write(dir.join("c.json"), r#"{ "script": "s.spaph" }"#).unwrap();

    let out = spaph()
        .args(["--config", dir.join("c.json").to_str().unwrap(), "--osc-port", "0"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("usage: plugin unload <plugin_id|all>"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_runs_nested_script() {
    let dir = scratch("load");
    std::fs::write(dir.join("child.spaph"), "master 0.5\n").unwrap();
    std::fs::write(dir.join("s.spaph"), "load child.spaph\n").unwrap();
    std::fs::write(dir.join("c.json"), r#"{ "script": "s.spaph" }"#).unwrap();

    let out = spaph()
        .args(["--config", dir.join("c.json").to_str().unwrap(), "--osc-port", "0"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("master volume set to 0.5"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_resolves_path_from_config_dir_not_working_dir() {
    let dir = scratch("loadcwd");
    let elsewhere = dir.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(dir.join("child.spaph"), "master 0.6\n").unwrap();
    std::fs::write(dir.join("s.spaph"), "load child.spaph\n").unwrap();
    std::fs::write(dir.join("c.json"), r#"{ "script": "s.spaph" }"#).unwrap();

    let out = spaph()
        .current_dir(&elsewhere)
        .args(["--config", dir.join("c.json").to_str().unwrap(), "--osc-port", "0"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("master volume set to 0.6"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_failing_line_stops_parent_script() {
    let dir = scratch("loadfail");
    std::fs::write(dir.join("child.spaph"), "frobnicate now\n").unwrap();
    std::fs::write(dir.join("s.spaph"), "load child.spaph\nmaster 0.9\n").unwrap();
    std::fs::write(dir.join("c.json"), r#"{ "script": "s.spaph" }"#).unwrap();

    let out = spaph()
        .args(["--config", dir.join("c.json").to_str().unwrap(), "--osc-port", "0"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unknown command: frobnicate"), "{err}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("master volume set to 0.9"), "parent must stop: {text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_missing_file_reports_error_without_panic() {
    let dir = scratch("loadmissing");
    std::fs::write(dir.join("s.spaph"), "load nope.spaph\n").unwrap();
    std::fs::write(dir.join("c.json"), r#"{ "script": "s.spaph" }"#).unwrap();

    let out = spaph()
        .args(["--config", dir.join("c.json").to_str().unwrap(), "--osc-port", "0"])
        .output()
        .unwrap();

    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("panicked"), "must not panic: {err}");
    assert_eq!(out.status.code(), Some(1));
    assert!(err.contains("cannot read script"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_without_path_prints_usage() {
    let dir = scratch("loadu");
    std::fs::write(dir.join("s.spaph"), "load\n").unwrap();
    std::fs::write(dir.join("c.json"), r#"{ "script": "s.spaph" }"#).unwrap();

    let out = spaph()
        .args(["--config", dir.join("c.json").to_str().unwrap(), "--osc-port", "0"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage: load <script>"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_recursion_is_limited_to_a_fixed_depth() {
    // A script that loads itself must stop with an error, not overflow the stack.
    let dir = scratch("loadrec");
    std::fs::write(dir.join("self.spaph"), "load self.spaph\n").unwrap();
    std::fs::write(dir.join("c.json"), r#"{ "script": "self.spaph" }"#).unwrap();

    let out = spaph()
        .args(["--config", dir.join("c.json").to_str().unwrap(), "--osc-port", "0"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(1), "must exit cleanly, not crash");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("nested too deep"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn quit_in_loaded_script_ends_only_that_script() {
    let dir = scratch("loadquit");
    std::fs::write(dir.join("child.spaph"), "quit\nmaster 0.3\n").unwrap();
    std::fs::write(dir.join("s.spaph"), "load child.spaph\nmaster 0.2\n").unwrap();
    std::fs::write(dir.join("c.json"), r#"{ "script": "s.spaph" }"#).unwrap();

    let out = spaph()
        .args(["--config", dir.join("c.json").to_str().unwrap(), "--osc-port", "0"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("0.3"), "child must stop at quit: {text}");
    assert!(text.contains("master volume set to 0.2"), "parent must continue: {text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn script_flag_is_restored_after_load() {
    // After a nested script returns, the outer script is still non-interactive,
    // so `midi listen` must still be skipped rather than blocking.
    let dir = scratch("loadflag");
    std::fs::write(dir.join("child.spaph"), "master 0.2\n").unwrap();
    std::fs::write(dir.join("s.spaph"), "load child.spaph\nmidi listen 0\n").unwrap();
    std::fs::write(dir.join("c.json"), r#"{ "script": "s.spaph" }"#).unwrap();

    let out = spaph()
        .args(["--config", dir.join("c.json").to_str().unwrap(), "--osc-port", "0"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("interactive only"));
    let _ = std::fs::remove_dir_all(&dir);
}

fn run_script_text(name: &str, text: &str) -> (Option<i32>, String, String) {
    let dir = scratch(name);
    std::fs::write(dir.join("s.spaph"), text).unwrap();
    std::fs::write(dir.join("c.json"), r#"{ "script": "s.spaph" }"#).unwrap();
    let out = spaph()
        .args(["--config", dir.join("c.json").to_str().unwrap(), "--osc-port", "0"])
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn plugin_load_alias_with_digit_start_is_rejected_before_loading() {
    let (code, _, err) = run_script_text("alias1", "plugin load /no/such.so as 1synth\n");
    assert_eq!(code, Some(1));
    assert!(!err.contains("panicked"), "{err}");
    assert!(err.contains("invalid alias '1synth'"), "{err}");
}

#[test]
fn plugin_load_alias_all_is_reserved() {
    let (code, _, err) = run_script_text("alias2", "plugin load /no/such.so as all\n");
    assert_eq!(code, Some(1));
    assert!(err.contains("alias 'all' is reserved"), "{err}");
}

#[test]
fn plugin_load_as_without_name_prints_usage() {
    let (code, _, err) = run_script_text("alias3", "plugin load /no/such.so as\n");
    assert_eq!(code, Some(1));
    assert!(err.contains("usage: plugin load <path> [as <name>]"), "{err}");
}

#[test]
fn plugin_show_unknown_alias_reports_error_without_panic() {
    let (code, _, err) = run_script_text("alias4", "plugin show nosuch\n");
    assert_eq!(code, Some(1));
    assert!(!err.contains("panicked"), "{err}");
    assert!(err.contains("invalid plugin id or alias"), "{err}");
}

#[test]
fn midi_connect_unknown_alias_reports_error() {
    let (code, _, err) = run_script_text("alias5", "midi connect 0 nosuch\n");
    assert_eq!(code, Some(1));
    assert!(err.contains("invalid plugin id or alias"), "{err}");
}

#[test]
fn numeric_id_still_reports_not_found_not_alias_error() {
    // Existing scripts use numeric ids; those keep their original error text.
    let (code, _, err) = run_script_text("alias6", "plugin hide 99\n");
    assert_eq!(code, Some(1));
    assert!(err.contains("plugin 99 not found"), "{err}");
}

#[test]
fn plugin_connect_without_both_ids_prints_usage() {
    let (code, _, err) = run_script_text("connu", "plugin connect 1\n");
    assert_eq!(code, Some(1));
    assert!(err.contains("usage: plugin connect <source> <target>"), "{err}");
}

#[test]
fn plugin_connect_unknown_numeric_ids_report_not_found() {
    let (code, _, err) = run_script_text("connnf", "plugin connect 1 2\n");
    assert_eq!(code, Some(1));
    assert!(!err.contains("panicked"), "{err}");
    assert!(err.contains("no plugin with id 1"), "{err}");
}

#[test]
fn plugin_connect_unknown_alias_reports_invalid() {
    let (code, _, err) = run_script_text("connal", "plugin connect nosuch other\n");
    assert_eq!(code, Some(1));
    assert!(err.contains("invalid plugin id or alias"), "{err}");
}

#[test]
fn plugin_disconnect_unknown_reports_not_found() {
    let (code, _, err) = run_script_text("discnf", "plugin disconnect 99\n");
    assert_eq!(code, Some(1));
    assert!(!err.contains("panicked"), "{err}");
    assert!(err.contains("no plugin with id 99"), "{err}");
}

#[test]
fn plugin_disconnect_without_id_prints_usage() {
    let (code, _, err) = run_script_text("discu", "plugin disconnect\n");
    assert_eq!(code, Some(1));
    assert!(err.contains("usage: plugin disconnect <target>"), "{err}");
}
