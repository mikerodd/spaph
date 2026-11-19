//! Configuration file (`spaph.json`) and startup script handling for spaph.
//!
//! Precedence for every setting: CLI flag > config file > built-in default.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Name of the configuration file loaded automatically from the working directory.
pub const DEFAULT_CONFIG_NAME: &str = "spaph.json";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config file {}: {}", .path.display(), .source)]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid config file {}: {}", .path.display(), .message)]
    Parse { path: PathBuf, message: String },
    #[error("invalid log_level '{0}' (expected one of: none, error, warn, info, debug, trace)")]
    InvalidLogLevel(String),
}

/// Audio settings. `None` means "not set": the built-in default applies.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AudioSettings {
    pub sample_rate: Option<u32>,
    pub buffer_size: Option<u32>,
    pub device_index: Option<usize>,
    /// Audio host backend, e.g. "wasapi" or "asio" (case-insensitive). `None` = system default.
    pub backend: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OscSettings {
    pub port: Option<u16>,
}

/// Parsed `spaph.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub audio: AudioSettings,
    pub osc: OscSettings,
    pub log_level: Option<String>,
    /// Path to a script of CLI commands run at startup (relative to the config file).
    pub script: Option<PathBuf>,
}

/// Values given on the command line; `Some` overrides the config file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overrides {
    pub sample_rate: Option<u32>,
    pub buffer_size: Option<u32>,
    pub device_index: Option<usize>,
    pub backend: Option<String>,
    pub log_level: Option<String>,
    pub osc_port: Option<u16>,
}

const LOG_LEVELS: [&str; 6] = ["none", "error", "warn", "info", "debug", "trace"];

fn validate_log_level(level: &str) -> Result<(), ConfigError> {
    if LOG_LEVELS.contains(&level) {
        Ok(())
    } else {
        Err(ConfigError::InvalidLogLevel(level.to_string()))
    }
}

/// Parse configuration text. `origin` is only used in error messages.
pub fn parse_config(text: &str, origin: &Path) -> Result<Config, ConfigError> {
    let cfg: Config = serde_json::from_str(text).map_err(|e| ConfigError::Parse {
        path: origin.to_path_buf(),
        message: e.to_string(),
    })?;
    if let Some(level) = &cfg.log_level {
        validate_log_level(level)?;
    }
    Ok(cfg)
}

/// Read and parse a configuration file.
pub fn load_config(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let text = if text.trim().is_empty() { "{}" } else { text.as_str() };
    parse_config(text, path)
}

/// Return `<dir>/spaph.json` if that file exists.
pub fn find_default_config(dir: &Path) -> Option<PathBuf> {
    let candidate = dir.join(DEFAULT_CONFIG_NAME);
    candidate.is_file().then_some(candidate)
}

/// Search order: executable's own directory first (so the binary behaves the same
/// regardless of the current working directory), then the working directory.
pub fn find_default_config_any() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if let Some(p) = find_default_config(dir) {
                return Some(p);
            }
        }
    }
    std::env::current_dir().ok().and_then(|d| find_default_config(&d))
}

/// Apply command-line overrides on top of the configuration.
pub fn apply_overrides(cfg: &mut Config, o: &Overrides) -> Result<(), ConfigError> {
    if let Some(v) = o.sample_rate {
        cfg.audio.sample_rate = Some(v);
    }
    if let Some(v) = o.buffer_size {
        cfg.audio.buffer_size = Some(v);
    }
    if let Some(v) = o.device_index {
        cfg.audio.device_index = Some(v);
    }
    if let Some(v) = &o.backend {
        cfg.audio.backend = Some(v.clone());
    }
    if let Some(v) = o.osc_port {
        cfg.osc.port = Some(v);
    }
    if let Some(level) = &o.log_level {
        validate_log_level(level)?;
        cfg.log_level = Some(level.clone());
    }
    Ok(())
}

/// Resolve the script path: relative paths are taken from the config file's folder.
pub fn resolve_script(cfg: &Config, config_dir: Option<&Path>) -> Option<PathBuf> {
    let script = cfg.script.as_ref()?;
    if script.is_absolute() {
        return Some(script.clone());
    }
    Some(match config_dir {
        Some(dir) => dir.join(script),
        None => script.clone(),
    })
}

/// One command read from a script, with its 1-based line number.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptLine {
    pub line: usize,
    pub command: String,
}

/// Split a script into commands. Blank lines and lines starting with `#` are skipped.
pub fn parse_script(text: &str) -> Vec<ScriptLine> {
    text.lines()
        .enumerate()
        .filter_map(|(i, raw)| {
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                None
            } else {
                Some(ScriptLine {
                    line: i + 1,
                    command: trimmed.to_string(),
                })
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> PathBuf {
        PathBuf::from("spaph.json")
    }

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("spaph_cfg_{}_{}", name, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_full_config() {
        let text = r#"{
            "audio": { "sample_rate": 48000, "buffer_size": 256, "device_index": 1 },
            "osc": { "port": 9100 },
            "log_level": "debug",
            "script": "startup.spaph"
        }"#;
        let cfg = parse_config(text, &origin()).unwrap();
        assert_eq!(cfg.audio.sample_rate, Some(48000));
        assert_eq!(cfg.audio.buffer_size, Some(256));
        assert_eq!(cfg.audio.device_index, Some(1));
        assert_eq!(cfg.osc.port, Some(9100));
        assert_eq!(cfg.log_level.as_deref(), Some("debug"));
        assert_eq!(cfg.script, Some(PathBuf::from("startup.spaph")));
    }

    #[test]
    fn partial_config_leaves_other_fields_unset() {
        let cfg = parse_config(r#"{ "osc": { "port": 9001 } }"#, &origin()).unwrap();
        assert_eq!(cfg.osc.port, Some(9001));
        assert_eq!(cfg.audio, AudioSettings::default());
        assert_eq!(cfg.log_level, None);
        assert_eq!(cfg.script, None);
    }

    #[test]
    fn empty_object_is_valid() {
        assert_eq!(parse_config("{}", &origin()).unwrap(), Config::default());
    }

    #[test]
    fn unknown_key_is_rejected() {
        let err = parse_config(r#"{ "audio": { "sample_rat": 48000 } }"#, &origin()).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn invalid_json_reports_position() {
        let err = parse_config("{\n  \"osc\": ,\n}", &origin()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("spaph.json"), "{msg}");
        assert!(msg.contains("line 2"), "{msg}");
    }

    #[test]
    fn invalid_log_level_is_rejected() {
        let err = parse_config(r#"{ "log_level": "verbose" }"#, &origin()).unwrap_err();
        assert!(err.to_string().contains("verbose"), "{err}");
    }

    #[test]
    fn missing_file_error_names_the_path() {
        let err = load_config(Path::new("/definitely/not/here/spaph.json")).unwrap_err();
        assert!(err.to_string().contains("/definitely/not/here/spaph.json"), "{err}");
    }

    #[test]
    fn audio_backend_is_read_from_file() {
        let cfg = parse_config(r#"{ "audio": { "backend": "asio" } }"#, &origin()).unwrap();
        assert_eq!(cfg.audio.backend.as_deref(), Some("asio"));
    }

    #[test]
    fn audio_backend_cli_overrides_file() {
        let mut cfg = parse_config(r#"{ "audio": { "backend": "wasapi" } }"#, &origin()).unwrap();
        let o = Overrides { backend: Some("asio".into()), ..Default::default() };
        apply_overrides(&mut cfg, &o).unwrap();
        assert_eq!(cfg.audio.backend.as_deref(), Some("asio"));
    }

    #[test]
    fn audio_backend_unset_stays_none() {
        let cfg = parse_config("{}", &origin()).unwrap();
        assert_eq!(cfg.audio.backend, None);
    }

    #[test]
    fn cli_overrides_win_over_file() {
        let mut cfg = parse_config(
            r#"{ "audio": { "sample_rate": 44100, "buffer_size": 512 }, "osc": { "port": 9000 }, "log_level": "info" }"#,
            &origin(),
        )
        .unwrap();
        let o = Overrides {
            sample_rate: Some(48000),
            osc_port: Some(9500),
            log_level: Some("trace".into()),
            ..Default::default()
        };
        apply_overrides(&mut cfg, &o).unwrap();
        assert_eq!(cfg.audio.sample_rate, Some(48000)); // overridden
        assert_eq!(cfg.audio.buffer_size, Some(512)); // kept from file
        assert_eq!(cfg.osc.port, Some(9500));
        assert_eq!(cfg.log_level.as_deref(), Some("trace"));
    }

    #[test]
    fn no_overrides_keeps_file_values() {
        let mut cfg = parse_config(r#"{ "osc": { "port": 9100 } }"#, &origin()).unwrap();
        let before = cfg.clone();
        apply_overrides(&mut cfg, &Overrides::default()).unwrap();
        assert_eq!(cfg, before);
    }

    #[test]
    fn invalid_override_log_level_is_rejected() {
        let mut cfg = Config::default();
        let o = Overrides { log_level: Some("loud".into()), ..Default::default() };
        assert!(apply_overrides(&mut cfg, &o).is_err());
    }

    #[test]
    fn script_parser_skips_blanks_and_comments_and_keeps_order() {
        let text = "# setup\n\nload '/a b.so'\n   \n  # note\nlist plugins\n";
        let lines = parse_script(text);
        assert_eq!(
            lines,
            vec![
                ScriptLine { line: 3, command: "load '/a b.so'".into() },
                ScriptLine { line: 6, command: "list plugins".into() },
            ]
        );
    }

    #[test]
    fn script_lines_are_trimmed() {
        let lines = parse_script("   volume 1 0.5   \n");
        assert_eq!(lines[0].command, "volume 1 0.5");
    }

    #[test]
    fn script_path_relative_to_config_dir() {
        let cfg = Config { script: Some(PathBuf::from("startup.spaph")), ..Default::default() };
        let dir = PathBuf::from("/home/u/rig");
        assert_eq!(
            resolve_script(&cfg, Some(&dir)),
            Some(PathBuf::from("/home/u/rig/startup.spaph"))
        );
    }

    #[test]
    fn script_absolute_path_unchanged() {
        let cfg = Config { script: Some(PathBuf::from("/etc/spaph/s.spaph")), ..Default::default() };
        assert_eq!(
            resolve_script(&cfg, Some(Path::new("/home/u/rig"))),
            Some(PathBuf::from("/etc/spaph/s.spaph"))
        );
    }

    #[test]
    fn no_script_configured() {
        assert_eq!(resolve_script(&Config::default(), None), None);
    }

    #[test]
    fn default_config_found_only_when_present() {
        let dir = tmp_dir("found");
        let _ = std::fs::remove_file(dir.join(DEFAULT_CONFIG_NAME));
        assert_eq!(find_default_config(&dir), None);
        std::fs::write(dir.join(DEFAULT_CONFIG_NAME), "{}").unwrap();
        assert_eq!(find_default_config(&dir), Some(dir.join(DEFAULT_CONFIG_NAME)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_config_reads_file() {
        let dir = tmp_dir("load");
        let path = dir.join("c.json");
        std::fs::write(&path, r#"{ "osc": { "port": 9200 } }"#).unwrap();
        assert_eq!(load_config(&path).unwrap().osc.port, Some(9200));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
