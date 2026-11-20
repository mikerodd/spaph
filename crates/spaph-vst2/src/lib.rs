//! VST2 host support for spaph, built on the `vst` crate (VST 2.4 ABI).

#![allow(deprecated)]

use spaph_plugin::{EditorSession, ParamInfo, PluginFactory, PluginInfo as SpaphPluginInfo, PluginInstance as SpaphPluginInstance};
use spaph_types::{MidiEvent, PluginFormat, Result, SpaphError};
use std::path::Path;
use std::sync::{Arc, Mutex};

mod editor;
mod fxp;

use fxp::{Body, Fxp};
use vst::host::{Host, PluginLoader};
use vst::editor::Editor;
use vst::plugin::{Info as VstInfo, Plugin, PluginParameters};

pub struct Vst2Host {
    pub block_size: std::sync::Mutex<isize>,
}
impl Host for Vst2Host {
    fn get_block_size(&self) -> isize {
        *self.block_size.lock().unwrap()
    }
}

pub struct Vst2Factory;
impl PluginFactory for Vst2Factory {
    fn format(&self) -> PluginFormat {
        PluginFormat::Vst2
    }
    fn can_load(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| matches!(e.to_lowercase().as_str(), "dll" | "so" | "vst"))
            .unwrap_or(false)
    }
    fn load(&self, path: &Path) -> Result<Box<dyn SpaphPluginInstance>> {
        let host = Arc::new(Mutex::new(Vst2Host { block_size: std::sync::Mutex::new(512) }));
        let mut loader = PluginLoader::load(path, host.clone())
            .map_err(|e| SpaphError::Plugin(e.to_string()))?;
        let mut inst = loader.instance().map_err(|e| SpaphError::Plugin(e.to_string()))?;
        // The VST2 SDK has no "has editor" query on the host side; a plugin without a GUI
        // reports an empty editor rect. get_editor() can only be called once per instance,
        // so keep the editor for the lifetime of the plugin.
        let editor = inst.get_editor().filter(|e| e.size() != (0, 0));
        let has_gui = editor.is_some();
        let vst_info: VstInfo = inst.get_info();
        let params = inst.get_parameter_object();
        let num = vst_info.parameters.max(0) as u32;
        let mut param_infos = Vec::with_capacity(num as usize);
        for i in 0..num {
            param_infos.push(ParamInfo {
                index: i,
                name: params.get_parameter_name(i as i32),
                default_value: params.get_parameter(i as i32) as f64,
                min: 0.0,
                max: 1.0,
            });
        }
        let num_presets = vst_info.presets.max(0) as usize;
        let mut preset_names = Vec::with_capacity(num_presets);
        let orig = params.get_preset_num();
        for i in 0..num_presets {
            params.change_preset(i as i32);
            let name = params.get_current_preset_name();
            preset_names.push(if name.is_empty() { format!("Preset {}", i + 1) } else { name });
        }
        params.change_preset(orig);
        let info = SpaphPluginInfo {
            name: vst_info.name.clone(),
            vendor: vst_info.vendor.clone(),
            version: vst_info.version.to_string(),
            format: PluginFormat::Vst2,
            num_inputs: vst_info.inputs as u32,
            num_outputs: vst_info.outputs as u32,
            params: param_infos,
            presets: preset_names,
        };
        Ok(Box::new(Vst2Instance {
            info,
            params,
            fx_id: vst_info.unique_id,
            fx_version: vst_info.version,
            has_gui,
            // Declared before `_instance` so the editor is dropped first.
            editor_slot: Arc::new(Mutex::new(editor.map(SendEditor))),
            _instance: inst,
            sample_rate: 48000.0,
        }))
    }
}

/// The plugin's `Editor` object. It is only used on the console thread, which owns the
/// plugin; it is moved in and out of the instance's slot, never shared.
struct SendEditor(Box<dyn Editor>);

// SAFETY: the editor is only touched on the console thread that loaded the plugin.
unsafe impl Send for SendEditor {}

/// An open VST2 editor window. Dropping or closing it returns the editor to the instance.
struct Vst2Session {
    editor: Option<SendEditor>,
    window: Box<dyn editor::HostWindow>,
    slot: Arc<Mutex<Option<SendEditor>>>,
}

impl Vst2Session {
    fn release(&mut self) {
        if let Some(mut editor) = self.editor.take() {
            editor::detach(editor.0.as_mut());
            *self.slot.lock().unwrap() = Some(editor);
        }
    }
}

impl EditorSession for Vst2Session {
    fn tick(&mut self) -> bool {
        match self.editor.as_mut() {
            Some(editor) => editor::tick(editor.0.as_mut(), self.window.as_mut()),
            None => false,
        }
    }

    fn close(mut self: Box<Self>) {
        self.release();
    }
}

impl Drop for Vst2Session {
    fn drop(&mut self) {
        self.release();
    }
}

pub struct Vst2Instance {
    info: SpaphPluginInfo,
    params: Arc<dyn PluginParameters>,
    fx_id: i32,
    fx_version: i32,
    has_gui: bool,
    /// The editor while no window is open; taken by `open_editor_session` and returned on close.
    editor_slot: Arc<Mutex<Option<SendEditor>>>,
    _instance: vst::host::PluginInstance,
    sample_rate: f64,
}

unsafe impl Send for Vst2Instance {}

impl SpaphPluginInstance for Vst2Instance {
    fn info(&self) -> &SpaphPluginInfo {
        &self.info
    }
    fn set_sample_rate(&mut self, sr: f64) {
        self.sample_rate = sr;
        self._instance.set_sample_rate(sr as f32);
    }
    fn set_block_size(&mut self, n: usize) {
        self._instance.set_block_size(n as i64);
    }
    fn init(&mut self) {
        self._instance.init();
    }
    fn resume(&mut self) {
        self._instance.resume();
    }
    fn deactivate(&mut self) {}
    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], num_frames: usize) {
        let input_ptrs: Vec<*const f32> = inputs.iter().map(|b| b.as_ptr()).collect();
        let mut output_ptrs: Vec<*mut f32> = outputs.iter_mut().map(|b| b.as_mut_ptr()).collect();
        let mut audio = unsafe {
            vst::buffer::AudioBuffer::from_raw(
                input_ptrs.len(),
                output_ptrs.len(),
                input_ptrs.as_ptr(),
                output_ptrs.as_mut_ptr(),
                num_frames,
            )
        };
        self._instance.process(&mut audio);
    }

    fn handle_midi(&mut self, event: &MidiEvent) {
        self.handle_midi_events(std::slice::from_ref(event));
    }

    fn handle_midi_events(&mut self, events: &[MidiEvent]) {
        let mut midi_events: Vec<vst::api::MidiEvent> = Vec::new();
        for ev in events {
            if ev.data.is_empty() {
                continue;
            }
            let me = vst::api::MidiEvent {
                event_type: vst::api::EventType::Midi,
                byte_size: std::mem::size_of::<vst::api::MidiEvent>() as i32,
                delta_frames: 0,
                flags: 0,
                note_length: 0,
                note_offset: 0,
                midi_data: [ev.data[0], *ev.data.get(1).unwrap_or(&0), *ev.data.get(2).unwrap_or(&0)],
                _midi_reserved: 0,
                detune: 0,
                note_off_velocity: 0,
                _reserved1: 0,
                _reserved2: 0,
            };
            midi_events.push(me);
        }
        if midi_events.is_empty() {
            return;
        }
        for ev in events {
            tracing::debug!("vst2 midi -> effProcessEvents: {:02x?}", ev.data);
        }
        let n = midi_events.len();
        let size = std::mem::size_of::<vst::api::Events>()
            + (n.saturating_sub(2)) * std::mem::size_of::<*mut vst::api::Event>();
        // Back the block with u64 words so it is 8-byte aligned, as `Events` requires.
        let mut buf = vec![0u64; size.div_ceil(8)];
        let events_ptr = buf.as_mut_ptr() as *mut vst::api::Events;
        let mut boxes: Vec<Box<vst::api::MidiEvent>> =
            midi_events.into_iter().map(Box::new).collect();
        unsafe {
            (*events_ptr).num_events = n as i32;
            (*events_ptr)._reserved = 0;
            let slot = &mut (*events_ptr).events as *mut [*mut vst::api::Event; 2]
                as *mut *mut vst::api::Event;
            for (i, b) in boxes.iter_mut().enumerate() {
                *slot.add(i) = b.as_mut() as *mut vst::api::MidiEvent as *mut vst::api::Event;
            }
            self._instance.process_events(&*events_ptr);
        }
    }

    fn preset_name(&self, index: usize) -> Option<String> {
        self.info.presets.get(index).cloned()
    }
    fn select_preset(&mut self, index: usize) -> Result<()> {
        if index >= self.info.presets.len() {
            return Err(SpaphError::Plugin("preset index out of range".into()));
        }
        self.params.change_preset(index as i32);
        Ok(())
    }

    fn load_preset_file(&mut self, path: &std::path::Path) -> Result<()> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
        if ext == "fxb" {
            return Err(SpaphError::Plugin("VST2 bank files (.fxb) are not supported yet; use .fxp".into()));
        }
        let data = std::fs::read(path).map_err(|e| SpaphError::Plugin(e.to_string()))?;
        self.load_state(&data)
    }

    fn has_editor(&self) -> bool {
        self.has_gui
    }

    fn open_editor_session(&mut self) -> Result<Box<dyn EditorSession>> {
        if !self.has_gui {
            return Err(SpaphError::Plugin("plugin has no editor".into()));
        }
        let mut editor = self
            .editor_slot
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| SpaphError::Plugin("editor is already open".into()))?;
        let (width, height) = editor.0.size();
        let attached = editor::open_window(&self.info.name, width, height).and_then(|mut window| {
            editor::attach(editor.0.as_mut(), window.as_mut())?;
            Ok(window)
        });
        match attached {
            Ok(window) => Ok(Box::new(Vst2Session {
                editor: Some(editor),
                window,
                slot: self.editor_slot.clone(),
            })),
            Err(e) => {
                // Nothing was opened: give the editor back so it can be tried again later.
                *self.editor_slot.lock().unwrap() = Some(editor);
                Err(e)
            }
        }
    }

    fn save_preset_file(&self, path: &std::path::Path) -> Result<()> {
        let data = self.save_state()?;
        std::fs::write(path, data).map_err(|e| SpaphError::Plugin(e.to_string()))?;
        Ok(())
    }

    fn get_param(&self, index: u32) -> f64 {
        self.params.get_parameter(index as i32) as f64
    }
    fn set_param(&mut self, index: u32, value: f64) {
        self.params.set_parameter(index as i32, value as f32);
    }
    /// Current program as `.fxp` bytes: the opaque chunk when the plugin provides one,
    /// otherwise its parameter values.
    fn save_state(&self) -> Result<Vec<u8>> {
        let chunk = self.params.get_preset_data();
        let body = if chunk.is_empty() {
            let values = (0..self.info.params.len() as i32)
                .map(|i| self.params.get_parameter(i))
                .collect();
            Body::Params(values)
        } else {
            Body::Chunk(chunk)
        };
        let program = Fxp {
            fx_id: self.fx_id,
            fx_version: self.fx_version,
            name: self.params.get_current_preset_name(),
            body,
        };
        Ok(fxp::encode(&program))
    }

    fn load_state(&mut self, data: &[u8]) -> Result<()> {
        let program = fxp::decode(data).map_err(SpaphError::Plugin)?;
        if program.fx_id != self.fx_id {
            return Err(SpaphError::Plugin(format!(
                "preset was saved for plugin id {:#x}, this plugin is {:#x}",
                program.fx_id, self.fx_id
            )));
        }
        match program.body {
            Body::Chunk(chunk) => self.params.load_preset_data(&chunk),
            Body::Params(values) => {
                if values.len() != self.info.params.len() {
                    return Err(SpaphError::Plugin(format!(
                        "preset has {} parameters, plugin has {}",
                        values.len(),
                        self.info.params.len()
                    )));
                }
                for (i, v) in values.into_iter().enumerate() {
                    self.params.set_parameter(i as i32, v);
                }
            }
        }
        Ok(())
    }
}
