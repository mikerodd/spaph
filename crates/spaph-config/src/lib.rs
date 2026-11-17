//! JSON configuration handling for spaph.

use spaph_types::{AppConfig, AudioConfig, OscConfig, PluginConfig, Result};
use std::fs;
use std::path::Path;

pub fn default_app_config() -> AppConfig {
    AppConfig {
        audio: AudioConfig {
            backend: "pipewire".into(),
            sample_rate: 48000,
            buffer_size: 512,
            input_device: None,
            output_device: None,
        },
        osc: OscConfig {
            bind_addr: "0.0.0.0".into(),
            port: 9000,
        },
        plugins: vec![],
    }
}

pub fn dump_app_config(path: &Path) -> Result<()> {
    let cfg = default_app_config();
    let json = serde_json::to_string_pretty(&cfg)?;
    fs::write(path, json)?;
    Ok(())
}

pub fn dump_plugins_config(path: &Path) -> Result<()> {
    let cfg: Vec<PluginConfig> = vec![];
    let json = serde_json::to_string_pretty(&cfg)?;
    fs::write(path, json)?;
    Ok(())
}

pub fn load_app_config(path: &Path) -> Result<AppConfig> {
    let data = fs::read_to_string(path)?;
    let cfg: AppConfig = serde_json::from_str(&data)?;
    Ok(cfg)
}

pub fn save_app_config(path: &Path, cfg: &AppConfig) -> Result<()> {
    let json = serde_json::to_string_pretty(cfg)?;
    fs::write(path, json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn dump_and_load_app_config() {
        let dir = std::env::temp_dir().join("spaph_test_cfg");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("app.json");
        dump_app_config(&path).unwrap();
        let cfg = load_app_config(&path).unwrap();
        assert_eq!(cfg.audio.sample_rate, 48000);
        assert_eq!(cfg.osc.port, 9000);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn dump_plugins_config_valid_json() {
        let dir = std::env::temp_dir().join("spaph_test_plugs");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("plugins.json");
        dump_plugins_config(&path).unwrap();
        let data = fs::read_to_string(&path).unwrap();
        let parsed: Vec<spaph_types::PluginConfig> = serde_json::from_str(&data).unwrap();
        assert!(parsed.is_empty());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join("spaph_test_round");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("rt.json");
        let cfg = default_app_config();
        save_app_config(&path, &cfg).unwrap();
        let loaded = load_app_config(&path).unwrap();
        assert_eq!(loaded.audio.backend, cfg.audio.backend);
        let _ = fs::remove_file(&path);
    }
}
