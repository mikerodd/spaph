//! Shared types, errors, identifiers and command definitions for spaph.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SpaphError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("plugin error: {0}")]
    Plugin(String),
    #[error("audio error: {0}")]
    Audio(String),
    #[error("config error: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, SpaphError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PluginFormat {
    Vst2,
    Vst3,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginId {
    pub format: PluginFormat,
    pub path: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MidiEvent {
    pub timestamp: u64,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    Help,
    ListPlugins,
    LoadPlugin { path: String },
    UnloadPlugin { id: u32 },
    ShowPlugin { id: u32 },
    Quit,
    DumpAppConfig { path: String },
    DumpPluginsConfig { path: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub audio: AudioConfig,
    pub osc: OscConfig,
    pub plugins: Vec<PluginConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    pub backend: String,
    pub sample_rate: u32,
    pub buffer_size: u32,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OscConfig {
    pub bind_addr: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginConfig {
    pub path: String,
    pub format: Option<PluginFormat>,
    pub bypass: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_format_serde_roundtrip() {
        for f in [PluginFormat::Vst2, PluginFormat::Vst3] {
            let json = serde_json::to_string(&f).unwrap();
            let back: PluginFormat = serde_json::from_str(&json).unwrap();
            assert_eq!(f, back);
        }
    }

    #[test]
    fn app_config_default_serializes() {
        let cfg = AppConfig {
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
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.audio.sample_rate, 48000);
        assert_eq!(back.osc.port, 9000);
    }

    #[test]
    fn command_serde_roundtrip() {
        let cmd = Command::LoadPlugin { path: "/tmp/x.vst3".into() };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: Command = serde_json::from_str(&json).unwrap();
        match back {
            Command::LoadPlugin { path } => assert_eq!(path, "/tmp/x.vst3"),
            _ => panic!("wrong variant"),
        }
    }
}
