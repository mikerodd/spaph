//! Audio I/O abstraction for spaph (cpal backend).

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use spaph_plugin::PluginInstance;
use spaph_types::{MidiEvent, Result, SpaphError};

static MASTER_GAIN: std::sync::Mutex<f32> = std::sync::Mutex::new(1.0);
use std::sync::{Arc, Mutex};

pub fn list_devices() -> Result<Vec<String>> {
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

pub fn set_master_gain(value: f32) {
    *MASTER_GAIN.lock().unwrap() = value;
}

pub fn get_master_gain() -> f32 {
    *MASTER_GAIN.lock().unwrap()
}

pub struct AudioHandle {
    pub _stream: cpal::Stream,
}

// SAFETY: cpal::Stream is !Send on some backends, but we only move the handle
// between threads for control purposes; the stream is created and dropped on the
// main thread. Marking Send+Sync allows global storage for OSC command handling.
unsafe impl Send for AudioHandle {}
unsafe impl Sync for AudioHandle {}

pub fn start_audio(
    plugin: Arc<Mutex<Box<dyn PluginInstance>>>,
    midi_queue: Arc<Mutex<Vec<MidiEvent>>>,
    device_index: Option<usize>,
    sample_rate: Option<u32>,
    buffer_size: Option<u32>,
    gain: std::sync::Arc<std::sync::Mutex<f32>>,
) -> Result<AudioHandle> {
    let host = cpal::default_host();
    let device = match device_index {
        Some(i) => host
            .output_devices()
            .map_err(|e| SpaphError::Audio(e.to_string()))?
            .nth(i)
            .ok_or_else(|| SpaphError::Audio(format!("no audio device at index {}", i)))?,
        None => host
            .default_output_device()
            .ok_or_else(|| SpaphError::Audio("no default output device".into()))?,
    };
    let supported = if let Some(sr) = sample_rate {
        let mut chosen = None;
        if let Ok(ranges) = device.supported_output_configs() {
            for r in ranges {
                if sr >= r.min_sample_rate() && sr <= r.max_sample_rate() {
                    chosen = Some(r.with_sample_rate(sr));
                    break;
                }
            }
        }
        chosen.unwrap_or_else(|| device.default_output_config().unwrap())
    } else {
        device.default_output_config().map_err(|e| SpaphError::Audio(e.to_string()))?
    };
    let sample_rate = supported.sample_rate();
    let channels = supported.channels() as usize;
    let mut stream_config: cpal::StreamConfig = supported.into();
    if let Some(bs) = buffer_size {
        stream_config.buffer_size = cpal::BufferSize::Fixed(bs);
    }

    let plugin_block_size = buffer_size.map(|b| (b as usize).min(512)).unwrap_or(512);
    let plugin_outputs = {
        let mut p = plugin.lock().unwrap();
        p.init();
        p.set_sample_rate(sample_rate as f64);
        p.set_block_size(plugin_block_size);
        p.resume();
        let n = p.info().num_outputs as usize;
        if n == 0 { 1 } else { n }
    };

    let block_size = match stream_config.buffer_size {
        cpal::BufferSize::Fixed(n) => n as usize,
        _ => 512usize,
    };
    let mut scratch = vec![vec![0.0f32; block_size]; plugin_outputs];

    let plugin_cb = plugin.clone();
    let midi_cb = midi_queue.clone();
    let gain_cb = gain.clone();

    let data_fn = move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
        let frames = if channels > 0 {
            data.len() / channels
        } else {
            0
        };
        if frames == 0 {
            return;
        }
        // resize scratch if needed
        if scratch.len() != plugin_outputs || scratch[0].len() != frames {
            scratch = vec![vec![0.0f32; frames]; plugin_outputs];
        }
        for ch in &mut scratch {
            for s in ch.iter_mut() {
                *s = 0.0;
            }
        }
        data.fill(0.0);

        let events: Vec<MidiEvent> = {
            let mut q = midi_cb.lock().unwrap();
            std::mem::take(&mut *q)
        };
        if !events.is_empty() { tracing::debug!("AUDIO MIDI: {:?}", events); }

        if let Ok(mut p) = plugin_cb.lock() {
            let plugin = &mut *p;
            plugin.set_sample_rate(sample_rate as f64);
            plugin.handle_midi_events(&events);
            let mut out_refs: Vec<&mut [f32]> =
                scratch.iter_mut().map(|v| v.as_mut_slice()).collect();
            let inputs: &[&[f32]] = &[];
            plugin.process(inputs, &mut out_refs, frames);
        }

        let gain_val = *gain_cb.lock().unwrap();
        let master_val = *MASTER_GAIN.lock().unwrap();
        for f in 0..frames {
            for c in 0..channels {
                let src_ch = if plugin_outputs > 0 {
                    c % plugin_outputs
                } else {
                    0
                };
                if src_ch < scratch.len() {
                    data[f * channels + c] = scratch[src_ch][f] * gain_val * master_val;
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
    stream
        .play()
        .map_err(|e| SpaphError::Audio(e.to_string()))?;

    Ok(AudioHandle { _stream: stream })
}

pub fn _err(msg: &str) -> SpaphError {
    SpaphError::Audio(msg.to_string())
}
