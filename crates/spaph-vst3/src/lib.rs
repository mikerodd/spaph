//! VST3 host support for spaph, built on the `vst3-host` crate.

use spaph_plugin::{EditorSession, ParamInfo, PluginFactory, PluginInfo as SpaphPluginInfo, PluginInstance as SpaphPluginInstance};
use spaph_types::{MidiEvent, PluginFormat, Result, SpaphError};
use std::path::Path;
use vst3_host::plugin::Plugin as Vst3Plugin;
use vst3_host::simple;

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
            num_inputs: vi.audio_inputs,
            num_outputs: vi.audio_outputs,
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
        for (i, ch) in inputs.iter().enumerate() {
            if i < bufs.inputs.len() {
                for (j, s) in ch.iter().enumerate().take(num_frames) {
                    bufs.inputs[i][j] = *s;
                }
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
