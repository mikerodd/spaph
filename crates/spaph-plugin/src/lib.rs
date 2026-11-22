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

/// An open plugin editor window.
///
/// Plugins (VST3 in particular) must be driven from the thread that loaded them, so a session
/// is created and serviced only on that thread. `tick` is called regularly from the console
/// loop, which keeps the prompt responsive without a separate UI thread.
pub trait EditorSession {
    /// Service the window once. Returns `false` once the window has been closed.
    fn tick(&mut self) -> bool;
    /// Close the window and hand the editor back to its plugin instance.
    fn close(self: Box<Self>);
}

/// A loaded plugin instance.
pub trait PluginInstance: Send {
    fn info(&self) -> &PluginInfo;
    /// Name of parameter `index`. Read on demand, so the plugin is not queried for every
    /// parameter at load time (some plugins crash on that). The default uses `info()`.
    fn param_name(&self, index: u32) -> String {
        self.info().params.iter().find(|p| p.index == index).map(|p| p.name.clone()).unwrap_or_default()
    }
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
    /// Whether the plugin has a graphical editor.
    fn has_editor(&self) -> bool {
        false
    }
    /// Open the editor window. Returns at once; the caller services it with `tick` until the
    /// window is closed or `close` is called. Audio and MIDI keep running meanwhile.
    fn open_editor_session(&mut self) -> Result<Box<dyn EditorSession>> {
        Err(SpaphError::Plugin("editor not supported".into()))
    }
    fn load_preset_file(&mut self, _path: &Path) -> Result<()> {
        Err(SpaphError::Plugin("preset file loading not supported".into()))
    }
    fn save_preset_file(&self, _path: &Path) -> Result<()> {
        Err(SpaphError::Plugin("preset file saving not supported".into()))
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

    struct Named(PluginInfo);
    impl PluginInstance for Named {
        fn info(&self) -> &PluginInfo { &self.0 }
        fn set_sample_rate(&mut self, _sr: f64) {}
        fn process(&mut self, _i: &[&[f32]], _o: &mut [&mut [f32]], _n: usize) {}
        fn get_param(&self, _index: u32) -> f64 { 0.0 }
        fn set_param(&mut self, _index: u32, _value: f64) {}
        fn save_state(&self) -> spaph_types::Result<Vec<u8>> { Ok(vec![]) }
        fn load_state(&mut self, _data: &[u8]) -> spaph_types::Result<()> { Ok(()) }
    }

    #[test]
    fn default_param_name_comes_from_info() {
        let info = PluginInfo {
            name: "t".into(),
            vendor: "v".into(),
            version: "1".into(),
            format: PluginFormat::Vst2,
            num_inputs: 2,
            num_outputs: 2,
            params: vec![ParamInfo { index: 0, name: "Gain".into(), default_value: 0.0, min: 0.0, max: 1.0 }],
            presets: vec![],
        };
        let p = Named(info);
        assert_eq!(p.param_name(0), "Gain");
        assert_eq!(p.param_name(9), "");
    }

    #[test]
    fn format_from_path_unknown() {
        assert_eq!(format_from_path(Path::new("foo.txt")), None);
        assert_eq!(format_from_path(Path::new("noext")), None);
    }
}
