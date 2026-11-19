//! Audio I/O abstraction for spaph (cpal backend).

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use spaph_plugin::PluginInstance;
use spaph_types::{MidiEvent, Result, SpaphError};
use std::sync::{Arc, Mutex, OnceLock};

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

static SAMPLE_RATE: Mutex<u32> = Mutex::new(48000);
static BUFFER_SIZE: Mutex<usize> = Mutex::new(512);
static DEVICE_INDEX: Mutex<Option<usize>> = Mutex::new(None);

pub fn set_sample_rate(v: u32) { *SAMPLE_RATE.lock().unwrap() = v; }
pub fn get_sample_rate() -> u32 { *SAMPLE_RATE.lock().unwrap() }
pub fn set_buffer_size(v: usize) { *BUFFER_SIZE.lock().unwrap() = if v == 0 { 512 } else { v }; }
pub fn get_buffer_size() -> usize { *BUFFER_SIZE.lock().unwrap() }
pub fn set_device_index(v: Option<usize>) { *DEVICE_INDEX.lock().unwrap() = v; }
pub fn get_device_index() -> Option<usize> { *DEVICE_INDEX.lock().unwrap() }

pub struct ActivePlugin {
    pub id: u32,
    pub instance: Arc<Mutex<Box<dyn PluginInstance>>>,
    pub gain: Arc<Mutex<f32>>,
    pub midi_queue: Arc<Mutex<Vec<MidiEvent>>>,
}

static ACTIVE: OnceLock<Arc<Mutex<Vec<ActivePlugin>>>> = OnceLock::new();

fn active_list() -> Arc<Mutex<Vec<ActivePlugin>>> {
    ACTIVE.get_or_init(|| Arc::new(Mutex::new(Vec::new()))).clone()
}

pub fn active_ids() -> Vec<u32> {
    active_list().lock().unwrap().iter().map(|p| p.id).collect()
}

pub fn arm(id: u32, instance: Arc<Mutex<Box<dyn PluginInstance>>>, gain: Arc<Mutex<f32>>, midi_queue: Arc<Mutex<Vec<MidiEvent>>>) -> Result<()> {
    let active = active_list();
    let mut guard = active.lock().unwrap();
    if guard.iter().any(|p| p.id == id) {
        return Ok(());
    }
    {
        let mut inst = instance.lock().unwrap();
        inst.set_sample_rate(get_sample_rate() as f64);
        inst.set_block_size(get_buffer_size());
        let _ = inst.activate();
    }
    guard.push(ActivePlugin { id, instance, gain, midi_queue });
    Ok(())
}

pub fn disarm(id: u32) -> Result<()> {
    let active = active_list();
    let mut guard = active.lock().unwrap();
    if let Some(pos) = guard.iter().position(|p| p.id == id) {
        let ap = guard.remove(pos);
        drop(guard);
        if let Ok(mut inst) = ap.instance.lock() {
            inst.deactivate();
        }
    }
    Ok(())
}

pub fn disarm_all() {
    let active = active_list();
    let all: Vec<ActivePlugin> = {
        let mut guard = active.lock().unwrap();
        guard.drain(..).collect()
    };
    for ap in all {
        if let Ok(mut inst) = ap.instance.lock() {
            inst.deactivate();
        }
    }
}

// ---------------------------------------------------------------------------
// Thread-local stream
// ---------------------------------------------------------------------------

thread_local! {
    static STREAM: std::cell::RefCell<Option<cpal::Stream>> = std::cell::RefCell::new(None);
}

pub fn start_stream() -> Result<()> {
    if STREAM.with(|s| s.borrow().is_some()) {
        return Ok(());
    }
    ensure_pulse_prop();
    let host = cpal::default_host();
    let device_index = get_device_index();
    let device = match device_index {
        Some(i) => host.output_devices().map_err(|e| SpaphError::Audio(e.to_string()))?.nth(i)
            .ok_or_else(|| SpaphError::Audio(format!("no audio device at index {}", i)))?,
        None => host.default_output_device().ok_or_else(|| SpaphError::Audio("no default output device".into()))?,
    };
    let sample_rate = get_sample_rate();
    let buffer_size = get_buffer_size();
    let channels = device.default_output_config().map_err(|e| SpaphError::Audio(e.to_string()))?.channels() as usize;

    let active = active_list();
    let channels_c = channels;
    let buffer_size_c = buffer_size;

    let data_fn = move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
        data.fill(0.0);
        let frames = if channels_c > 0 { data.len() / channels_c } else { 0 };
        if frames == 0 {
            return;
        }
        let active = match active.try_lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let master = get_master_gain();
        for plugin in active.iter() {
            let po = {
                let p = plugin.instance.lock().unwrap_or_else(|e| e.into_inner());
                let n = p.info().num_outputs as usize;
                if n == 0 { 1 } else { n }
            };
            let events: Vec<MidiEvent> = {
                let mut q = plugin.midi_queue.lock().unwrap_or_else(|e| e.into_inner());
                std::mem::take(&mut *q)
            };
            let mut inst = match plugin.instance.try_lock() {
                Ok(g) => g,
                Err(_) => continue,
            };
            inst.set_block_size(buffer_size_c);
            for ev in &events {
                inst.handle_midi(ev);
            }
            let mut scratch = vec![vec![0.0f32; frames]; po];
            let mut out_refs: Vec<&mut [f32]> = scratch.iter_mut().map(|v| v.as_mut_slice()).collect();
            let inputs: &[&[f32]] = &[];
            inst.process(inputs, &mut out_refs, frames);
            drop(inst);
            let gain = *plugin.gain.lock().unwrap_or_else(|e| e.into_inner());
            for f in 0..frames {
                for c in 0..channels_c {
                    let src_ch = if po > 0 { c % po } else { 0 };
                    if src_ch < scratch.len() {
                        data[f * channels_c + c] += scratch[src_ch][f] * gain * master;
                    }
                }
            }
        }
    };

    let err_fn = |e| {
        tracing::error!("audio stream error: {}", e);
    };

    let stream_config = cpal::StreamConfig {
        channels: channels as u16,
        sample_rate: sample_rate,
        buffer_size: cpal::BufferSize::Default,
    };

    let stream = device
        .build_output_stream(stream_config, data_fn, err_fn, None)
        .map_err(|e| SpaphError::Audio(e.to_string()))?;
    stream.play().map_err(|e| SpaphError::Audio(e.to_string()))?;
    STREAM.with(|s| *s.borrow_mut() = Some(stream));
    Ok(())
}

pub fn stop_stream() {
    STREAM.with(|s| { *s.borrow_mut() = None; });
}

pub fn is_running() -> bool {
    STREAM.with(|s| s.borrow().is_some())
}

pub fn ensure_engine(sample_rate: Option<u32>, buffer_size: Option<u32>, device_index: Option<usize>) -> Result<()> {
    if let Some(v) = sample_rate { set_sample_rate(v); }
    if let Some(v) = buffer_size { set_buffer_size(v as usize); }
    if device_index.is_some() { set_device_index(device_index); }
    Ok(())
}

pub fn start_stream_with(sample_rate: Option<u32>, buffer_size: Option<u32>, device_index: Option<usize>) -> Result<()> {
    ensure_engine(sample_rate, buffer_size, device_index)?;
    start_stream()
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

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

pub fn backend_list() -> Vec<(String, bool, bool)> {
    ensure_pulse_prop();
    let current = cpal::default_host().id();
    let mut out = Vec::new();
    for id in cpal::available_hosts() {
        let available = if id.name().eq_ignore_ascii_case("jack") {
            true
        } else {
            cpal::host_from_id(id).is_ok()
        };
        out.push((id.name().to_string(), available, id == current));
    }
    out
}

pub fn _err(msg: &str) -> SpaphError {
    SpaphError::Audio(msg.to_string())
}
