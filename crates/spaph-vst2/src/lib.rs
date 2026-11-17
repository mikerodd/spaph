//! VST2 host support for spaph, built on the `vst` crate (VST 2.4 ABI).

#![allow(deprecated)]

use spaph_plugin::{ParamInfo, PluginFactory, PluginInfo as SpaphPluginInfo, PluginInstance as SpaphPluginInstance};
use spaph_types::{MidiEvent, PluginFormat, Result, SpaphError};
use std::path::Path;
use std::sync::{Arc, Mutex};
use vst::host::{Host, PluginLoader};
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
            _instance: inst,
            sample_rate: 48000.0,
        }))
    }
}

pub struct Vst2Instance {
    info: SpaphPluginInfo,
    params: Arc<dyn PluginParameters>,
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
        let n = midi_events.len();
        let size = std::mem::size_of::<vst::api::Events>()
            + (n.saturating_sub(2)) * std::mem::size_of::<*mut vst::api::Event>();
        let mut buf = vec![0u8; size];
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
        if ext == "fxp" || ext == "fxb" {
            return Err(SpaphError::Plugin(".fxp/.fxb loading requires parsing the VST2 preset container; not implemented yet".into()));
        }
        let data = std::fs::read(path).map_err(|e| SpaphError::Plugin(e.to_string()))?;
        self.load_state(&data)
    }

    fn get_param(&self, index: u32) -> f64 {
        self.params.get_parameter(index as i32) as f64
    }
    fn set_param(&mut self, index: u32, value: f64) {
        self.params.set_parameter(index as i32, value as f32);
    }
    fn save_state(&self) -> Result<Vec<u8>> {
        Err(SpaphError::Plugin("state save not implemented for VST2".into()))
    }
    fn load_state(&mut self, _data: &[u8]) -> Result<()> {
        Err(SpaphError::Plugin("state load not implemented for VST2".into()))
    }
}
