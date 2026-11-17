//! Common plugin abstraction used by all format-specific host crates.

use spaph_types::{MidiEvent, PluginFormat, Result, SpaphError};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct ParamInfo {
    pub index: u32,
    pub name: String,
    pub default_value: f64,
    pub min: f64,
    pub max: f64,
}

#[derive(Debug, Clone)]
pub struct PluginInfo {
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub format: PluginFormat,
    pub num_inputs: u32,
    pub num_outputs: u32,
    pub params: Vec<ParamInfo>,
    pub presets: Vec<String>,
}

/// A loaded plugin instance.
pub trait PluginInstance: Send {
    fn info(&self) -> &PluginInfo;
    fn set_sample_rate(&mut self, sr: f64);
    fn set_block_size(&mut self, _n: usize) {}
    fn init(&mut self) {}
    fn resume(&mut self) {}
    fn activate(&mut self) -> Result<()> {
        self.init();
        self.resume();
        Ok(())
    }
    fn deactivate(&mut self) {}
    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], num_frames: usize);
    fn handle_midi(&mut self, _event: &MidiEvent) {}
    fn handle_midi_events(&mut self, events: &[MidiEvent]) {
        for e in events {
            self.handle_midi(e);
        }
    }
    fn num_presets(&self) -> usize {
        self.info().presets.len()
    }
    fn preset_name(&self, index: usize) -> Option<String> {
        self.info().presets.get(index).cloned()
    }
    fn select_preset(&mut self, _index: usize) -> Result<()> {
        Ok(())
    }
    fn load_preset_file(&mut self, _path: &Path) -> Result<()> {
        Err(SpaphError::Plugin("preset file loading not supported".into()))
    }
    fn get_param(&self, index: u32) -> f64;
    fn set_param(&mut self, index: u32, value: f64);
    fn save_state(&self) -> Result<Vec<u8>>;
    fn load_state(&mut self, data: &[u8]) -> Result<()>;
}

pub trait PluginFactory: Send + Sync {
    fn format(&self) -> PluginFormat;
    fn can_load(&self, path: &Path) -> bool;
    fn load(&self, path: &Path) -> Result<Box<dyn PluginInstance>>;
}

pub fn format_from_path(path: &Path) -> Option<PluginFormat> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    match ext.as_str() {
        "dll" | "so" | "vst" => Some(PluginFormat::Vst2),
        "vst3" => Some(PluginFormat::Vst3),
        _ => None,
    }
}

#[allow(unused)]
pub fn not_loaded() -> SpaphError {
    SpaphError::Plugin("plugin not loaded".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn format_from_path_vst2() {
        assert_eq!(format_from_path(Path::new("foo.dll")), Some(PluginFormat::Vst2));
        assert_eq!(format_from_path(Path::new("foo.so")), Some(PluginFormat::Vst2));
        assert_eq!(format_from_path(Path::new("foo.vst")), Some(PluginFormat::Vst2));
    }

    #[test]
    fn format_from_path_vst3() {
        assert_eq!(format_from_path(Path::new("foo.vst3")), Some(PluginFormat::Vst3));
    }

    #[test]
    fn format_from_path_unknown() {
        assert_eq!(format_from_path(Path::new("foo.txt")), None);
        assert_eq!(format_from_path(Path::new("noext")), None);
    }
}
