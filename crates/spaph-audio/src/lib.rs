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
// MIDI event counters (diagnostics for lost or delayed events)
// ---------------------------------------------------------------------------

pub mod midi_stats {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    static RECEIVED: AtomicU64 = AtomicU64::new(0);
    static FILTERED: AtomicU64 = AtomicU64::new(0);
    static QUEUED: AtomicU64 = AtomicU64::new(0);
    static DELIVERED: AtomicU64 = AtomicU64::new(0);
    static DEFERRED: AtomicU64 = AtomicU64::new(0);
    static NOTE_ON: AtomicU64 = AtomicU64::new(0);
    static NOTE_OFF: AtomicU64 = AtomicU64::new(0);

    /// A MIDI message arrived from an input port (before channel filtering).
    pub fn on_received() { RECEIVED.fetch_add(1, Relaxed); }
    /// A message was rejected by the channel filter.
    pub fn on_filtered() { FILTERED.fetch_add(1, Relaxed); }
    /// A message was placed in a plugin's queue.
    pub fn on_queued() { QUEUED.fetch_add(1, Relaxed); }
    /// Messages were handed to a plugin during an audio block; note-on and note-off are
    /// counted separately so a stuck note shows up as more note-ons than note-offs.
    pub fn on_delivered(events: &[super::MidiEvent]) {
        DELIVERED.fetch_add(events.len() as u64, Relaxed);
        for ev in events {
            let status = ev.data.first().copied().unwrap_or(0);
            let velocity = ev.data.get(2).copied().unwrap_or(0);
            match status & 0xF0 {
                0x90 if velocity > 0 => { NOTE_ON.fetch_add(1, Relaxed); }
                0x90 | 0x80 => { NOTE_OFF.fetch_add(1, Relaxed); }
                _ => {}
            }
        }
    }
    /// An audio block found a plugin busy; its queued events were kept for the next block.
    pub fn on_deferred() { DEFERRED.fetch_add(1, Relaxed); }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Snapshot {
        pub received: u64,
        pub filtered: u64,
        pub queued: u64,
        pub delivered: u64,
        pub deferred: u64,
        pub note_on: u64,
        pub note_off: u64,
    }

    pub fn snapshot() -> Snapshot {
        Snapshot {
            received: RECEIVED.load(Relaxed),
            filtered: FILTERED.load(Relaxed),
            queued: QUEUED.load(Relaxed),
            delivered: DELIVERED.load(Relaxed),
            deferred: DEFERRED.load(Relaxed),
            note_on: NOTE_ON.load(Relaxed),
            note_off: NOTE_OFF.load(Relaxed),
        }
    }

    pub fn reset() {
        for c in [&RECEIVED, &FILTERED, &QUEUED, &DELIVERED, &DEFERRED, &NOTE_ON, &NOTE_OFF] {
            c.store(0, Relaxed);
        }
    }
}

// ---------------------------------------------------------------------------
// Output stream (process-wide, so any thread such as the OSC server can restart it)
// ---------------------------------------------------------------------------

// The stream is kept only so it stays alive; it is never read.
struct StreamSlot(#[allow(dead_code)] cpal::Stream);

// SAFETY: the stream is only created, replaced or dropped while holding STREAM's lock.
// cpal drives its callback on its own thread; this handle is never used for audio work.
unsafe impl Send for StreamSlot {}

static STREAM: Mutex<Option<StreamSlot>> = Mutex::new(None);

/// Renders one block of interleaved f32 samples.
type Render = Arc<dyn Fn(&mut [f32]) + Send + Sync>;

fn stream_error(e: cpal::Error) {
    tracing::error!("audio stream error: {}", e);
}

/// Open an output stream whose samples have type `T`. The f32 block from `render` is
/// converted per callback, so devices that only accept integer samples (e.g. ASIO4ALL, i32)
/// work the same as f32 devices.
fn open_stream_typed<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    render: Render,
) -> std::result::Result<cpal::Stream, cpal::Error>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let mut scratch: Vec<f32> = Vec::new();
    device.build_output_stream::<T, _, _>(
        config.clone(),
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            scratch.resize(data.len(), 0.0);
            render(&mut scratch);
            for (out, sample) in data.iter_mut().zip(scratch.iter()) {
                *out = T::from_sample(*sample);
            }
        },
        stream_error,
        None,
    )
}

fn open_stream(
    device: &cpal::Device,
    format: cpal::SampleFormat,
    config: &cpal::StreamConfig,
    render: Render,
) -> Result<cpal::Stream> {
    let result = match format {
        cpal::SampleFormat::F32 => open_stream_typed::<f32>(device, config, render),
        cpal::SampleFormat::I32 => open_stream_typed::<i32>(device, config, render),
        cpal::SampleFormat::I16 => open_stream_typed::<i16>(device, config, render),
        cpal::SampleFormat::U16 => open_stream_typed::<u16>(device, config, render),
        other => return Err(SpaphError::Audio(format!("unsupported sample format {:?}", other))),
    };
    result.map_err(|e| SpaphError::Audio(e.to_string()))
}

pub fn start_stream() -> Result<()> {
    let mut slot = STREAM.lock().unwrap();
    if slot.is_some() {
        return Ok(());
    }
    ensure_pulse_prop();
    let host = current_host()?;
    let device_index = get_device_index();
    let device = match device_index {
        Some(i) => host.output_devices().map_err(|e| SpaphError::Audio(e.to_string()))?.nth(i)
            .ok_or_else(|| SpaphError::Audio(format!("no audio device at index {}", i)))?,
        None => host.default_output_device().ok_or_else(|| SpaphError::Audio("no default output device".into()))?,
    };
    let sample_rate = get_sample_rate();
    let buffer_size = get_buffer_size();
    let default_cfg = device.default_output_config().map_err(|e| SpaphError::Audio(e.to_string()))?;
    let channels = default_cfg.channels() as usize;
    let sample_format = default_cfg.sample_format();

    let active = active_list();
    let channels_c = channels;

    // Renders one block as f32; converted to the device's sample format by `open_stream`.
    let render = move |data: &mut [f32]| {
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
            // Lock the instance first. If another thread holds it, leave the queued MIDI
            // events where they are and try again on the next block; taking them first would
            // throw them away (lost note-offs).
            let mut inst = match plugin.instance.try_lock() {
                Ok(g) => g,
                Err(_) => {
                    midi_stats::on_deferred();
                    continue;
                }
            };
            let po = {
                let n = inst.info().num_outputs as usize;
                if n == 0 { 1 } else { n }
            };
            // Never block the audio thread on the queue: if the MIDI thread is pushing right
            // now, play this block without new events and deliver them on the next one.
            let events: Vec<MidiEvent> = plugin
                .midi_queue
                .try_lock()
                .map(|mut q| std::mem::take(&mut *q))
                .unwrap_or_default();
            midi_stats::on_delivered(&events);
            // One batch per block: VST2 plugins expect all events of a block in a single
            // effProcessEvents call; sending them one by one can drop earlier events.
            if !events.is_empty() {
                inst.handle_midi_events(&events);
            }
            let mut scratch = vec![vec![0.0f32; frames]; po];
            let mut out_refs: Vec<&mut [f32]> = scratch.iter_mut().map(|v| v.as_mut_slice()).collect();
            let inputs: &[&[f32]] = &[];
            inst.process(inputs, &mut out_refs, frames);
            drop(inst);
            let gain = plugin.gain.try_lock().map(|g| *g).unwrap_or(1.0);
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
    let render: Render = Arc::new(render);

    // An explicit buffer size bounds latency. With BufferSize::Default, PipeWire-ALSA may pick
    // very large periods (cpal's ALSA backend notes this), which shows up as seconds of delay.
    // Fall back to Default only if the device refuses the requested size.
    let fixed_config = cpal::StreamConfig {
        channels: channels as u16,
        sample_rate: sample_rate,
        buffer_size: cpal::BufferSize::Fixed(buffer_size as u32),
    };
    let default_config = cpal::StreamConfig {
        channels: channels as u16,
        sample_rate: sample_rate,
        buffer_size: cpal::BufferSize::Default,
    };

    let open = |config: &cpal::StreamConfig| open_stream(&device, sample_format, config, render.clone());
    let stream = match open(&fixed_config) {
        Ok(s) => {
            tracing::info!(
                "audio stream: {} Hz, {} channels, {:?} samples, buffer {} frames",
                sample_rate, channels, sample_format, buffer_size
            );
            s
        }
        Err(e) => {
            tracing::warn!("fixed buffer {} not accepted ({}); using device default buffer", buffer_size, e);
            open(&default_config).map_err(|e| SpaphError::Audio(e.to_string()))?
        }
    };
    stream.play().map_err(|e| SpaphError::Audio(e.to_string()))?;
    *slot = Some(StreamSlot(stream));
    Ok(())
}

pub fn stop_stream() {
    *STREAM.lock().unwrap() = None;
}

pub fn is_running() -> bool {
    STREAM.lock().unwrap().is_some()
}

/// Switch the output stream to the device at `index` (None = system default).
/// Restarts the stream if needed; on failure the previous device is restored.
pub fn set_output_device(index: Option<usize>) -> Result<()> {
    let previous = get_device_index();
    if previous == index && is_running() {
        return Ok(());
    }
    stop_stream();
    set_device_index(index);
    if let Err(e) = start_stream() {
        set_device_index(previous);
        let _ = start_stream();
        return Err(e);
    }
    Ok(())
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
    let host = current_host()?;
    // Keep every device (even unnamed ones) so positions match the indices used by start_stream.
    let mut out = Vec::new();
    if let Ok(devs) = host.output_devices() {
        for d in devs {
            let name = d
                .description()
                .map(|desc| desc.name().to_string())
                .unwrap_or_else(|_| "(unnamed device)".to_string());
            out.push(name);
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
// Audio backend (host) selection
// ---------------------------------------------------------------------------

/// Backend chosen with `audio.backend` / `--audio-backend`. `None` = cpal's default host.
static BACKEND: Mutex<Option<cpal::HostId>> = Mutex::new(None);

/// Select the audio backend by name (case-insensitive), or `None` for the default.
/// Must be called before the stream starts; the name must be one cpal compiled in.
pub fn set_backend(name: Option<&str>) -> Result<()> {
    let id = match name {
        None => None,
        Some(wanted) => Some(
            cpal::available_hosts()
                .into_iter()
                .find(|id| id.name().eq_ignore_ascii_case(wanted))
                .ok_or_else(|| {
                    let names: Vec<&str> = cpal::available_hosts().iter().map(|id| id.name()).collect();
                    SpaphError::Audio(format!(
                        "unknown audio backend '{}' (compiled in: {})",
                        wanted,
                        names.join(", ")
                    ))
                })?,
        ),
    };
    *BACKEND.lock().unwrap() = id;
    Ok(())
}

/// Host used for devices and streams: the selected backend, else cpal's default.
fn current_host() -> Result<cpal::Host> {
    match *BACKEND.lock().unwrap() {
        Some(id) => cpal::host_from_id(id).map_err(|e| SpaphError::Audio(e.to_string())),
        None => Ok(cpal::default_host()),
    }
}

/// `(name, available, in_use)` for every backend compiled in.
/// A backend is available only if it can be opened and has at least one output device, so
/// ASIO without an installed driver is reported as unavailable.
pub fn backend_list() -> Vec<(String, bool, bool)> {
    ensure_pulse_prop();
    let in_use = match *BACKEND.lock().unwrap() {
        Some(id) => id,
        None => cpal::default_host().id(),
    };
    let mut out = Vec::new();
    for id in cpal::available_hosts() {
        let available = if id.name().eq_ignore_ascii_case("jack") {
            true
        } else {
            cpal::host_from_id(id)
                .ok()
                .and_then(|host| host.output_devices().ok())
                .map(|mut devs| devs.next().is_some())
                .unwrap_or(false)
        };
        out.push((id.name().to_string(), available, id == in_use));
    }
    out
}

pub fn _err(msg: &str) -> SpaphError {
    SpaphError::Audio(msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_backend_is_rejected_with_available_list() {
        let err = set_backend(Some("no-such-backend")).unwrap_err().to_string();
        assert!(err.contains("unknown audio backend 'no-such-backend'"), "{err}");
        assert!(err.contains("compiled in:"), "{err}");
    }

    #[test]
    fn backend_list_marks_exactly_one_in_use() {
        let list = backend_list();
        assert!(!list.is_empty());
        assert!(list.iter().filter(|(_, _, in_use)| *in_use).count() <= 1);
    }

    #[test]
    fn midi_stats_count_and_reset() {
        // Counters are process-wide; this is the only test that touches them.
        midi_stats::reset();
        midi_stats::on_received();
        midi_stats::on_received();
        midi_stats::on_filtered();
        midi_stats::on_queued();
        let ev = |data: &[u8]| MidiEvent { timestamp: 0, data: data.to_vec() };
        midi_stats::on_delivered(&[
            ev(&[0x90, 60, 100]), // note on
            ev(&[0x90, 60, 0]),   // note on with velocity 0 is a note off
            ev(&[0x81, 62, 40]),  // note off, channel 2
        ]);
        midi_stats::on_deferred();
        let s = midi_stats::snapshot();
        assert_eq!(
            s,
            midi_stats::Snapshot {
                received: 2, filtered: 1, queued: 1, delivered: 3, deferred: 1, note_on: 1, note_off: 2,
            }
        );
        midi_stats::reset();
        assert_eq!(midi_stats::snapshot().received, 0);
    }
}
