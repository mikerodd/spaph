//! Configuration file (`spaph.json`) and startup script handling for spaph.
//!
//! Precedence for every setting: CLI flag > config file > built-in default.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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

/// A parameter declared by a script with a `# param` header.
#[derive(Debug, Clone, PartialEq)]
pub struct ParamDecl {
    pub name: String,
    /// `None` for a required parameter; `Some(value)` for an optional one (empty when `name=`).
    pub default: Option<String>,
}

/// Parameter values, by name. Used both for the values a caller passes and the bound values.
pub type ParamValues = BTreeMap<String, String>;

fn is_param_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Read the `# param` headers of a script:
/// `# param name` (required), `# param name=` (optional, empty when not given) or
/// `# param name=value` (optional, `value` when not given).
pub fn parse_param_headers(text: &str) -> Result<Vec<ParamDecl>, String> {
    let mut decls: Vec<ParamDecl> = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let Some(rest) = raw.trim().strip_prefix('#') else { continue };
        let spec = match rest.trim_start().strip_prefix("param") {
            Some(s) if s.is_empty() || s.starts_with(char::is_whitespace) => s.trim(),
            _ => continue,
        };
        let err = |msg: String| format!("line {}: {}", i + 1, msg);
        if spec.is_empty() {
            return Err(err("'# param' needs a name".into()));
        }
        let (name, default) = match spec.split_once('=') {
            Some((name, default)) => (name.trim(), Some(default.trim().to_string())),
            None => (spec, None),
        };
        if !is_param_name(name) {
            return Err(err(format!("invalid parameter name '{}'", name)));
        }
        if decls.iter().any(|d| d.name == name) {
            return Err(err(format!("parameter '{}' declared twice", name)));
        }
        decls.push(ParamDecl { name: name.to_string(), default });
    }
    Ok(decls)
}

/// Read the `name=value` arguments of a `load` call (already split into tokens).
pub fn parse_param_args(tokens: &[String]) -> Result<ParamValues, String> {
    let mut out = ParamValues::new();
    for token in tokens {
        let (name, value) = token
            .split_once('=')
            .ok_or_else(|| format!("expected name=value, got '{}'", token))?;
        if !is_param_name(name) {
            return Err(format!("invalid parameter name '{}'", name));
        }
        if out.insert(name.to_string(), value.to_string()).is_some() {
            return Err(format!("parameter '{}' given twice", name));
        }
    }
    Ok(out)
}

/// Check the caller's values against the declarations and fill in defaults.
pub fn bind_params(decls: &[ParamDecl], given: &ParamValues) -> Result<ParamValues, String> {
    if decls.is_empty() && !given.is_empty() {
        return Err("this script takes no parameters".into());
    }
    if let Some(name) = given.keys().find(|k| !decls.iter().any(|d| &d.name == *k)) {
        return Err(format!("unknown parameter '{}'", name));
    }
    let mut bound = ParamValues::new();
    for decl in decls {
        let value = match (given.get(&decl.name), &decl.default) {
            (Some(v), _) => v.clone(),
            (None, Some(default)) => default.clone(),
            (None, None) => return Err(format!("missing required parameter '{}'", decl.name)),
        };
        bound.insert(decl.name.clone(), value);
    }
    Ok(bound)
}

/// Quote a value so it stays one argument after `split_args`. Values without spaces or
/// quotes are left bare; an empty value becomes nothing at all.
fn quote_value(value: &str) -> Result<String, String> {
    if !value.chars().any(|c| c.is_whitespace() || c == '\'' || c == '"') {
        Ok(value.to_string())
    } else if !value.contains('\'') {
        Ok(format!("'{}'", value))
    } else if !value.contains('"') {
        Ok(format!("\"{}\"", value))
    } else {
        Err(format!("value '{}' contains both ' and \" quotes", value))
    }
}

/// Replace each `$name` in a script line with its bound value. A name ends at the first
/// character that is not a letter, digit or `_`. A `$name` that is not declared is an error.
pub fn substitute_params(line: &str, values: &ParamValues) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = line;
    while let Some(pos) = rest.find('$') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 1..];
        let len = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        let name = &after[..len];
        if !is_param_name(name) {
            out.push('$');
            rest = after;
            continue;
        }
        let value = values
            .get(name)
            .ok_or_else(|| format!("unknown parameter '${}'", name))?;
        out.push_str(&quote_value(value)?);
        rest = &after[len..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Commands of a script with its parameters bound. A script without `# param` headers is
/// returned unchanged, and takes no parameters. All `$name` references are checked before
/// any command is returned, so a bad call fails before the script runs.
pub fn bind_script(text: &str, given: &ParamValues) -> Result<Vec<ScriptLine>, String> {
    let decls = parse_param_headers(text)?;
    let mut lines = parse_script(text);
    if decls.is_empty() {
        bind_params(&decls, given)?;
        return Ok(lines);
    }
    let values = bind_params(&decls, given)?;
    for line in lines.iter_mut() {
        line.command = substitute_params(&line.command, &values)
            .map_err(|e| format!("line {}: {}", line.line, e))?;
    }
    Ok(lines)
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

    fn params(pairs: &[(&str, &str)]) -> ParamValues {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn param_headers_read_required_and_optional() {
        let text = "# param alias\n# param device=\n# param midi=3\nplugin load x as $alias\n";
        let decls = parse_param_headers(text).unwrap();
        assert_eq!(
            decls,
            vec![
                ParamDecl { name: "alias".into(), default: None },
                ParamDecl { name: "device".into(), default: Some(String::new()) },
                ParamDecl { name: "midi".into(), default: Some("3".into()) },
            ]
        );
    }

    #[test]
    fn param_header_ignores_spaces_around_equals() {
        let decls = parse_param_headers("# param plugin_name= amsynth\n# param midi =  2\n").unwrap();
        assert_eq!(
            decls,
            vec![
                ParamDecl { name: "plugin_name".into(), default: Some("amsynth".into()) },
                ParamDecl { name: "midi".into(), default: Some("2".into()) },
            ]
        );
    }

    #[test]
    fn param_header_needs_a_name() {
        assert!(parse_param_headers("# param\n").unwrap_err().contains("needs a name"));
        assert!(parse_param_headers("# param 1x\n").unwrap_err().contains("invalid parameter name"));
        assert!(parse_param_headers("# param a b\n").unwrap_err().contains("invalid parameter name"));
    }

    #[test]
    fn param_header_declared_twice_is_rejected() {
        let err = parse_param_headers("# param a\n# param a=\n").unwrap_err();
        assert!(err.contains("declared twice"), "{err}");
    }

    #[test]
    fn plain_comments_are_not_param_headers() {
        assert_eq!(parse_param_headers("# parameters are cool\n# note\n").unwrap(), vec![]);
    }

    #[test]
    fn param_args_split_on_first_equals() {
        let tokens = vec!["alias=strum2".to_string(), "device=Scarlett Solo".to_string(), "x=a=b".to_string()];
        assert_eq!(
            parse_param_args(&tokens).unwrap(),
            params(&[("alias", "strum2"), ("device", "Scarlett Solo"), ("x", "a=b")])
        );
    }

    #[test]
    fn param_args_must_be_named() {
        let err = parse_param_args(&["strum2".to_string()]).unwrap_err();
        assert!(err.contains("expected name=value"), "{err}");
        let err = parse_param_args(&["1x=2".to_string()]).unwrap_err();
        assert!(err.contains("invalid parameter name"), "{err}");
    }

    #[test]
    fn param_args_given_twice_is_rejected() {
        let tokens = vec!["a=1".to_string(), "a=2".to_string()];
        assert!(parse_param_args(&tokens).unwrap_err().contains("given twice"));
    }

    #[test]
    fn bind_requires_required_params() {
        let decls = parse_param_headers("# param alias\n").unwrap();
        let err = bind_params(&decls, &ParamValues::new()).unwrap_err();
        assert!(err.contains("missing required parameter 'alias'"), "{err}");
    }

    #[test]
    fn bind_fills_defaults() {
        let decls = parse_param_headers("# param a\n# param b=\n# param c=7\n").unwrap();
        let bound = bind_params(&decls, &params(&[("a", "1")])).unwrap();
        assert_eq!(bound, params(&[("a", "1"), ("b", ""), ("c", "7")]));
    }

    #[test]
    fn bind_rejects_undeclared_param() {
        let decls = parse_param_headers("# param a\n").unwrap();
        let err = bind_params(&decls, &params(&[("a", "1"), ("typo", "2")])).unwrap_err();
        assert!(err.contains("unknown parameter 'typo'"), "{err}");
    }

    #[test]
    fn bind_rejects_params_for_script_without_headers() {
        let err = bind_params(&[], &params(&[("a", "1")])).unwrap_err();
        assert!(err.contains("takes no parameters"), "{err}");
    }

    #[test]
    fn substitute_replaces_names() {
        let values = params(&[("alias", "strum2"), ("midi", "2")]);
        assert_eq!(
            substitute_params("midi connect 0 $alias $midi", &values).unwrap(),
            "midi connect 0 strum2 2"
        );
    }

    #[test]
    fn substitute_empty_value_disappears() {
        let values = params(&[("midi", "")]);
        assert_eq!(substitute_params("midi connect 0 strum $midi", &values).unwrap(), "midi connect 0 strum ");
        let tokens = split_args_for_test(&substitute_params("midi connect 0 strum $midi", &values).unwrap());
        assert_eq!(tokens, vec!["midi", "connect", "0", "strum"]);
    }

    #[test]
    fn substitute_quotes_values_with_spaces() {
        let values = params(&[("device", "Scarlett Solo USB")]);
        let line = substitute_params("audio connect strum $device", &values).unwrap();
        assert_eq!(line, "audio connect strum 'Scarlett Solo USB'");
    }

    #[test]
    fn substitute_uses_double_quotes_when_value_has_single_quote() {
        let values = params(&[("name", "it's a synth")]);
        let line = substitute_params("plugin load x as $name", &values).unwrap();
        assert_eq!(line, "plugin load x as \"it's a synth\"");
    }

    #[test]
    fn substitute_rejects_value_with_both_quote_kinds() {
        let values = params(&[("name", "it's \"x\"")]);
        assert!(substitute_params("plugin load x as $name", &values).is_err());
    }

    #[test]
    fn substitute_rejects_undeclared_name() {
        let err = substitute_params("midi connect 0 $nope", &ParamValues::new()).unwrap_err();
        assert!(err.contains("unknown parameter '$nope'"), "{err}");
    }

    #[test]
    fn substitute_leaves_lone_dollar_alone() {
        assert_eq!(substitute_params("cost $ 5 $1", &ParamValues::new()).unwrap(), "cost $ 5 $1");
    }

    #[test]
    fn bind_script_without_headers_is_unchanged() {
        let lines = bind_script("master $x\n", &ParamValues::new()).unwrap();
        assert_eq!(lines, vec![ScriptLine { line: 1, command: "master $x".into() }]);
    }

    #[test]
    fn bind_script_substitutes_each_line() {
        let text = "# param level\n\nmaster $level\n";
        let lines = bind_script(text, &params(&[("level", "0.5")])).unwrap();
        assert_eq!(lines, vec![ScriptLine { line: 3, command: "master 0.5".into() }]);
    }

    #[test]
    fn bind_script_reports_undeclared_reference_with_line() {
        let text = "# param a\nmaster $a\nmaster $b\n";
        let err = bind_script(text, &params(&[("a", "1")])).unwrap_err();
        assert!(err.starts_with("line 3:"), "{err}");
        assert!(err.contains("unknown parameter '$b'"), "{err}");
    }

    /// Whitespace split, enough for lines without quotes.
    fn split_args_for_test(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
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
