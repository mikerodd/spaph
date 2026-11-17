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
