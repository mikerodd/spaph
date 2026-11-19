//! Audio I/O abstraction for spaph (cpal backend).

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use spaph_plugin::PluginInstance;
use spaph_types::{MidiEvent, Result, SpaphError};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

static MASTER_GAIN: Mutex<f32> = Mutex::new(1.0);

pub fn set_master_gain(value: f32) {
    *MASTER_GAIN.lock().unwrap() = value;
}

pub fn get_master_gain() -> f32 {
    *MASTER_GAIN.lock().unwrap()
}

fn ensure_pulse_prop() {
    if std::env::var("PULSE_PROP").is_err() {
        unsafe { std::env::set_var("PULSE_PROP", "application.name=spaph"); }
    }
}

// ---------------------------------------------------------------------------
// Backend enumeration
// ---------------------------------------------------------------------------

pub fn backend_list() -> Vec<(String, bool, bool)> {
    // Avoid auto-starting a JACK server while probing.
    unsafe { std::env::set_var("JACK_NO_START_SERVER", "1"); }
    let current = cpal::default_host().id();
    let mut out = Vec::new();
    for id in cpal::available_hosts() {
        // Probing JACK creates a connection attempt to jackd and floods stderr
        // when no server is running; report it as available without probing.
        let available = if id.name().eq_ignore_ascii_case("jack") {
            true
        } else {
            cpal::host_from_id(id).is_ok()
        };
        out.push((id.name().to_string(), available, id == current));
    }
    out
}

pub fn list_devices() -> Result<Vec<String>> {
    ensure_pulse_prop();
    let host = cpal::default_host();
    let mut out = Vec::new();
    if let Ok(devs) = host.output_devices() {
        for d in devs {
            if let Ok(desc) = d.description() {
                out.push(desc.name().to_string());
            }
        }
    }
    Ok(out)
}

pub fn device_name(index: Option<usize>) -> Option<String> {
    let devs = list_devices().ok()?;
    match index {
        Some(i) => devs.get(i).cloned(),
        None => devs.into_iter().next(),
    }
}

// ---------------------------------------------------------------------------
// Active plugin / mixer
// ---------------------------------------------------------------------------

pub struct ActivePlugin {
    pub id: u32,
    pub instance: Arc<Mutex<Box<dyn PluginInstance>>>,
    pub gain: Arc<Mutex<f32>>,
    pub midi_queue: Arc<Mutex<Vec<MidiEvent>>>,
    pub scratch: Vec<Vec<f32>>,
}

pub struct AudioEngine {
    stream: Option<cpal::Stream>,
    active: Arc<Mutex<Vec<ActivePlugin>>>,
    sample_rate: u32,
    buffer_size: usize,
    channels: usize,
    device_index: Option<usize>,
}

unsafe impl Send for AudioEngine {}
unsafe impl Sync for AudioEngine {}

static ENGINE: Mutex<Option<AudioEngine>> = Mutex::new(None);

pub fn engine() -> &'static Mutex<Option<AudioEngine>> {
    &ENGINE
}

impl AudioEngine {
    pub fn new(sample_rate: Option<u32>, buffer_size: Option<u32>, device_index: Option<usize>) -> Result<Self> {
        ensure_pulse_prop();
        let host = cpal::default_host();
        let device = match device_index {
            Some(i) => host.output_devices().map_err(|e| SpaphError::Audio(e.to_string()))?.nth(i)
                .ok_or_else(|| SpaphError::Audio(format!("no audio device at index {}", i)))?,
            None => host.default_output_device().ok_or_else(|| SpaphError::Audio("no default output device".into()))?,
        };
        let supported = device.default_output_config().map_err(|e| SpaphError::Audio(e.to_string()))?;
        let sr = sample_rate.unwrap_or(supported.sample_rate());
        let channels = supported.channels() as usize;
        let bs = buffer_size.unwrap_or(512) as usize;
        let bs = if bs == 0 { 512 } else { bs };

        let mut engine = AudioEngine {
            stream: None,
            active: Arc::new(Mutex::new(Vec::new())),
            sample_rate: sr,
            buffer_size: bs,
            channels,
            device_index,
        };
        engine.start_stream()?;
        Ok(engine)
    }

    fn start_stream(&mut self) -> Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }
        ensure_pulse_prop();
        let host = cpal::default_host();
        let device = match self.device_index {
            Some(i) => host.output_devices().map_err(|e| SpaphError::Audio(e.to_string()))?.nth(i)
                .ok_or_else(|| SpaphError::Audio(format!("no audio device at index {}", i)))?,
            None => host.default_output_device().ok_or_else(|| SpaphError::Audio("no default output device".into()))?,
        };
        let stream_config = cpal::StreamConfig {
            channels: self.channels as u16,
            sample_rate: self.sample_rate,
            buffer_size: cpal::BufferSize::Fixed(self.buffer_size as u32),
        };

        let active = self.active.clone();
        let channels = self.channels;
        let sample_rate = self.sample_rate;
        let buffer_size = self.buffer_size;

        let data_fn = move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            data.fill(0.0);
            let frames = if channels > 0 { data.len() / channels } else { 0 };
            if frames == 0 {
                return;
            }
            let active = match active.try_lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            let master = get_master_gain();
            for plugin in active.iter() {
                // resize scratch if needed
                let po = if plugin.scratch.is_empty() { 1 } else { plugin.scratch.len() };
                if plugin.scratch.is_empty() || plugin.scratch[0].len() != frames {
                    // reallocate
                    let scratch = vec![vec![0.0f32; frames]; po];
                    // can't mutate through &ActivePlugin in iter(); handle below
                    let _ = scratch;
                }
                // drain midi
                let events: Vec<MidiEvent> = {
                    let mut q = plugin.midi_queue.lock().unwrap_or_else(|p| p.into_inner());
                    std::mem::take(&mut *q)
                };
                let mut inst = match plugin.instance.try_lock() {
                    Ok(g) => g,
                    Err(_) => continue,
                };
                inst.set_sample_rate(sample_rate as f64);
                inst.set_block_size(buffer_size);
                for ev in &events {
                    inst.handle_midi(ev);
                }
                // process into a temp scratch (owned by plugin, but we need mut)
                // We'll use a local scratch to avoid borrow issues.
                // (scratch resize handled by reallocating into a local vec)
                let mut scratch = vec![vec![0.0f32; frames]; po];
                let mut out_refs: Vec<&mut [f32]> = scratch.iter_mut().map(|v| v.as_mut_slice()).collect();
                let inputs: &[&[f32]] = &[];
                inst.process(inputs, &mut out_refs, frames);
                drop(inst);
                let gain = *plugin.gain.lock().unwrap_or_else(|p| p.into_inner());
                for f in 0..frames {
                    for c in 0..channels {
                        let src_ch = if po > 0 { c % po } else { 0 };
                        if src_ch < scratch.len() {
                            data[f * channels + c] += scratch[src_ch][f] * gain * master;
                        }
                    }
                }
            }
        };

        let err_fn = |e| {
            tracing::error!("audio stream error: {}", e);
        };

        let stream = device
            .build_output_stream(stream_config, data_fn, err_fn, None)
            .map_err(|e| SpaphError::Audio(e.to_string()))?;
        stream.play().map_err(|e| SpaphError::Audio(e.to_string()))?;
        self.stream = Some(stream);
        Ok(())
    }

    pub fn arm(&mut self, id: u32, instance: Arc<Mutex<Box<dyn PluginInstance>>>, gain: Arc<Mutex<f32>>, midi_queue: Arc<Mutex<Vec<MidiEvent>>>) -> Result<()> {
        let mut active = self.active.lock().unwrap();
        if active.iter().any(|p| p.id == id) {
            return Ok(());
        }
        let plugin_outputs = {
            let p = instance.lock().unwrap();
            let n = p.info().num_outputs as usize;
            if n == 0 { 1 } else { n }
        };
        active.push(ActivePlugin {
            id,
            instance,
            gain,
            midi_queue,
            scratch: vec![vec![0.0f32; self.buffer_size]; plugin_outputs],
        });
        drop(active);
        let active = self.active.lock().unwrap();
        if let Some(p) = active.iter().find(|p| p.id == id) {
            let mut inst = p.instance.lock().unwrap();
            inst.set_sample_rate(self.sample_rate as f64);
            inst.set_block_size(self.buffer_size);
            let _ = inst.activate();
        }
        drop(active);
        if self.stream.is_none() {
            self.start_stream()?;
        }
        Ok(())
    }

    pub fn disarm(&mut self, id: u32) -> Result<()> {
        let mut active = self.active.lock().unwrap();
        if let Some(pos) = active.iter().position(|p| p.id == id) {
            let ap = active.remove(pos);
            drop(active);
            if let Ok(mut inst) = ap.instance.lock() {
                inst.deactivate();
            }
        }
        Ok(())
    }

    pub fn disarm_all(&mut self) {
        let mut active = self.active.lock().unwrap();
        let all: Vec<ActivePlugin> = active.drain(..).collect();
        drop(active);
        for ap in all {
            if let Ok(mut inst) = ap.instance.lock() {
                inst.deactivate();
            }
        }
    }

    pub fn active_ids(&self) -> Vec<u32> {
        self.active.lock().unwrap().iter().map(|p| p.id).collect()
    }

}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn ensure_engine(sample_rate: Option<u32>, buffer_size: Option<u32>, device_index: Option<usize>) -> Result<()> {
    let mut e = ENGINE.lock().unwrap();
    if e.is_none() {
        *e = Some(AudioEngine::new(sample_rate, buffer_size, device_index)?);
    }
    Ok(())
}

pub fn start_stream(sample_rate: Option<u32>, buffer_size: Option<u32>, device_index: Option<usize>) -> Result<()> {
    ensure_engine(sample_rate, buffer_size, device_index)?;
    let mut e = ENGINE.lock().unwrap();
    if let Some(eng) = e.as_mut() {
        eng.start_stream()?;
    }
    Ok(())
}

pub fn arm(id: u32, instance: Arc<Mutex<Box<dyn PluginInstance>>>, gain: Arc<Mutex<f32>>, midi_queue: Arc<Mutex<Vec<MidiEvent>>>) -> Result<()> {
    ensure_engine(None, None, None)?;
    let mut e = ENGINE.lock().unwrap();
    if let Some(eng) = e.as_mut() {
        eng.arm(id, instance, gain, midi_queue)?;
    }
    Ok(())
}

pub fn disarm(id: u32) -> Result<()> {
    let mut e = ENGINE.lock().unwrap();
    if let Some(eng) = e.as_mut() {
        eng.disarm(id)?;
    }
    Ok(())
}

pub fn disarm_all() {
    let mut e = ENGINE.lock().unwrap();
    if let Some(eng) = e.as_mut() {
        eng.disarm_all();
    }
}

pub fn active_ids() -> Vec<u32> {
    ENGINE.lock().unwrap().as_ref().map(|e| e.active_ids()).unwrap_or_default()
}

pub fn _err(msg: &str) -> SpaphError {
    SpaphError::Audio(msg.to_string())
}
