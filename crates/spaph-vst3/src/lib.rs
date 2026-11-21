//! VST3 host support for spaph, built on the `vst3-host` crate.

use spaph_plugin::{EditorSession, ParamInfo, PluginFactory, PluginInfo as SpaphPluginInfo, PluginInstance as SpaphPluginInstance};
use spaph_types::{MidiEvent, PluginFormat, Result, SpaphError};
use std::path::Path;
use vst3_host::plugin::Plugin as Vst3Plugin;
use vst3_host::simple;

/// Channel count of the main bus: the first active bus with channels. `None` if there is none.
pub fn main_bus_channels(buses: &[vst3_host::audio::AudioBusConfig]) -> Option<u32> {
    buses
        .iter()
        .find(|b| b.active && b.channel_count > 0)
        .map(|b| b.channel_count as u32)
}

/// Smallest input value handed to a VST3 plugin: about -180 dBFS, inaudible.
pub const SILENCE_FLOOR: f32 = 1e-9;

/// Replace an exact zero with `SILENCE_FLOOR`. The host flags an all-zero bus as silent, and
/// some effects then stop producing their tail (reverb, delay), so exact silence is avoided.
pub fn keep_active(s: f32) -> f32 {
    if s == 0.0 { SILENCE_FLOOR } else { s }
}

pub struct Vst3Factory;

impl PluginFactory for Vst3Factory {
    fn format(&self) -> PluginFormat {
        PluginFormat::Vst3
    }
    fn can_load(&self, path: &Path) -> bool {
        if path.is_dir() {
            path.to_str().map(|s| s.ends_with(".vst3")).unwrap_or(false)
        } else {
            path.extension().map(|e| e == "vst3").unwrap_or(false)
        }
    }
    fn load(&self, path: &Path) -> Result<Box<dyn SpaphPluginInstance>> {
        let plugin = simple::load_plugin(path).map_err(|e| SpaphError::Plugin(e.to_string()))?;
        let vi = plugin.info().clone();
        // `vi.audio_inputs` / `audio_outputs` count buses, not channels. Use the channel count
        // of the main (first active) bus instead, falling back to the bus counts.
        let (num_inputs, num_outputs) = match plugin.audio_bus_layout() {
            Ok(layout) => (
                main_bus_channels(&layout.inputs).unwrap_or(0),
                main_bus_channels(&layout.outputs).unwrap_or(0),
            ),
            Err(_) => (vi.audio_inputs, vi.audio_outputs),
        };
        let params = plugin.get_parameters().map_err(|e| SpaphError::Plugin(e.to_string()))?;
        let param_infos: Vec<ParamInfo> = params
            .iter()
            .map(|p| ParamInfo {
                index: p.id,
                name: p.name.clone(),
                default_value: p.default,
                min: p.min,
                max: p.max,
            })
            .collect();
        // Build preset list from units
        let mut preset_names = Vec::new();
        let mut presets_map = Vec::new();
        if let Ok(units) = plugin.get_units() {
            tracing::debug!("VST3 units: {} -> {:?}", units.len(), units.iter().map(|u| u.programs.len()).collect::<Vec<_>>());
            for unit in &units {
                tracing::debug!("  unit {} programs: {}", unit.id, unit.programs.len());
            }
            for unit in units {
                for (idx, name) in unit.programs.into_iter().enumerate() {
                    preset_names.push(if name.is_empty() { format!("Preset {}", preset_names.len() + 1) } else { name });
                    presets_map.push((unit.id, idx as i32));
                }
            }
        } else {
            tracing::debug!("VST3 get_units failed");
        }
        let info = SpaphPluginInfo {
            name: vi.name.clone(),
            vendor: vi.vendor.clone(),
            version: vi.version.clone(),
            format: PluginFormat::Vst3,
            num_inputs,
            num_outputs,
            params: param_infos,
            presets: preset_names,
        };
        let plugin = std::sync::Arc::new(std::sync::Mutex::new(plugin));
        Ok(Box::new(Vst3Instance {
            info,
            plugin,
            presets_map,
            sample_rate: 48000.0,
        }))
    }
}

pub struct Vst3Instance {
    info: SpaphPluginInfo,
    plugin: std::sync::Arc<std::sync::Mutex<Vst3Plugin>>,
    presets_map: Vec<(i32, i32)>,
    sample_rate: f64,
}

unsafe impl Send for Vst3Instance {}

/// An open VST3 editor window, serviced from the console thread that loaded the plugin.
struct Vst3Session {
    window: vst3_host::window::PluginWindow,
    plugin: std::sync::Arc<std::sync::Mutex<Vst3Plugin>>,
}

impl EditorSession for Vst3Session {
    fn tick(&mut self) -> bool {
        let _ = self.window.service_platform_events();
        if let Ok(mut p) = self.plugin.lock() {
            p.service_run_loop();
        }
        // Pump the Windows message queue so the plugin's editor (and any child
        // windows such as login dialogs) can receive input and paint.
        #[cfg(target_os = "windows")]
        unsafe {
            use winapi::um::winuser::{PeekMessageW, TranslateMessage, DispatchMessageW, PM_REMOVE};
            use winapi::um::winuser::MSG;
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        !self.window.closed_by_user()
    }

    fn close(mut self: Box<Self>) {
        self.window.close();
    }
}

impl SpaphPluginInstance for Vst3Instance {
    fn info(&self) -> &SpaphPluginInfo {
        &self.info
    }
    fn set_sample_rate(&mut self, sr: f64) {
        self.sample_rate = sr;
    }
    fn init(&mut self) {
        let _ = self.plugin.lock().unwrap().start_processing();
    }
    fn activate(&mut self) -> Result<()> {
        self.plugin.lock().unwrap().start_processing().map_err(|e| SpaphError::Plugin(e.to_string()))
    }
    fn deactivate(&mut self) {
        let _ = self.plugin.lock().unwrap().stop_processing();
    }
    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], num_frames: usize) {
        let mut bufs = vst3_host::audio::AudioBuffers::new(inputs.len(), outputs.len(), num_frames, self.sample_rate);
        for (i, ch) in bufs.inputs.iter_mut().enumerate() {
            match inputs.get(i) {
                Some(src) => {
                    for (j, s) in src.iter().enumerate().take(num_frames) {
                        ch[j] = keep_active(*s);
                    }
                }
                // Unconnected input channels are fed the floor too, so no bus is flagged silent.
                None => ch.fill(SILENCE_FLOOR),
            }
        }
        let _ = self.plugin.lock().unwrap().process_audio(&mut bufs);
        for (i, ch) in outputs.iter_mut().enumerate() {
            if i < bufs.outputs.len() {
                for (j, s) in ch.iter_mut().enumerate().take(num_frames) {
                    *s = bufs.outputs[i][j];
                }
            }
        }
    }    fn handle_midi(&mut self, event: &MidiEvent) {
        if let Some(me) = vst3_host::midi::MidiEvent::from_midi_bytes(&event.data) {
            let _ = self.plugin.lock().unwrap().send_live_midi_event(me);
        }
    }    fn preset_name(&self, index: usize) -> Option<String> {
        self.info.presets.get(index).cloned()
    }
    fn select_preset(&mut self, index: usize) -> Result<()> {
        if index >= self.presets_map.len() {
            return Err(SpaphError::Plugin("preset index out of range".into()));
        }
        let (unit_id, prog_idx) = self.presets_map[index];
        self.plugin.lock().unwrap().select_program(unit_id, prog_idx).map_err(|e| SpaphError::Plugin(e.to_string()))
    }
    fn load_preset_file(&mut self, path: &Path) -> Result<()> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
        if ext == "vstpreset" {
            self.plugin.lock().unwrap().load_vstpreset(path).map_err(|e| SpaphError::Plugin(e.to_string()))
        } else if ext == "fxp" || ext == "fxb" {
            Err(SpaphError::Plugin(".fxp/.fxb is VST2 format and cannot be loaded into a VST3 plugin via host state API".into()))
        } else {
            let data = std::fs::read(path).map_err(|e| SpaphError::Plugin(e.to_string()))?;
            self.plugin.lock().unwrap().load_state(&data).map_err(|e| SpaphError::Plugin(e.to_string()))
        }
    }

    fn has_editor(&self) -> bool {
        self.plugin.lock().unwrap().has_editor()
    }

    fn open_editor_session(&mut self) -> Result<Box<dyn EditorSession>> {
        use vst3_host::window::PluginWindow;
        let mut window = PluginWindow::new(self.plugin.clone());
        window.open().map_err(|e| SpaphError::Plugin(e.to_string()))?;
        Ok(Box::new(Vst3Session { window, plugin: self.plugin.clone() }))
    }

    fn save_preset_file(&self, path: &Path) -> Result<()> {
        self.plugin.lock().unwrap().save_vstpreset(path).map_err(|e| SpaphError::Plugin(e.to_string()))
    }

    fn get_param(&self, index: u32) -> f64 {
        self.plugin.lock().unwrap().get_parameter(index).unwrap_or(0.0)
    }
    fn set_param(&mut self, index: u32, value: f64) {
        let _ = self.plugin.lock().unwrap().set_parameter(index, value);
    }
    fn save_state(&self) -> Result<Vec<u8>> {
        self.plugin.lock().unwrap().save_state().map_err(|e| SpaphError::Plugin(e.to_string()))
    }
    fn load_state(&mut self, data: &[u8]) -> Result<()> {
        self.plugin.lock().unwrap().load_state(data).map_err(|e| SpaphError::Plugin(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::{main_bus_channels, keep_active, SILENCE_FLOOR};

    #[test]
    fn zero_input_sample_becomes_inaudible_floor() {
        // A bus of exact zeros is flagged silent by the VST3 host, and effects such as Calf
        // reverb then stop their tail. The floor is -180 dBFS: inaudible, but not zero.
        assert_ne!(keep_active(0.0), 0.0);
        assert_eq!(keep_active(0.0), SILENCE_FLOOR);
        assert!(SILENCE_FLOOR < 1e-8 && SILENCE_FLOOR > 1e-12);
    }

    #[test]
    fn non_zero_input_sample_is_unchanged() {
        assert_eq!(keep_active(0.25), 0.25);
        assert_eq!(keep_active(-0.5), -0.5);
    }

    use vst3_host::audio::AudioBusConfig;

    fn bus(channel_count: usize, active: bool) -> AudioBusConfig {
        AudioBusConfig { channel_count, active }
    }

    #[test]
    fn main_bus_channels_counts_channels_not_buses() {
        assert_eq!(main_bus_channels(&[bus(2, true), bus(2, true)]), Some(2));
    }

    #[test]
    fn main_bus_channels_skips_inactive_buses() {
        assert_eq!(main_bus_channels(&[bus(2, false), bus(1, true)]), Some(1));
    }

    #[test]
    fn main_bus_channels_none_without_active_bus() {
        assert_eq!(main_bus_channels(&[]), None);
        assert_eq!(main_bus_channels(&[bus(2, false)]), None);
        assert_eq!(main_bus_channels(&[bus(0, true)]), None);
    }
}
