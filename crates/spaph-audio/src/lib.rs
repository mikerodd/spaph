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

/// A plugin in the audio graph.
///
/// A node is processed when it is armed (`to_output`, heard on the device) or when it is part
/// of a link (`plugin connect`), even if it is not armed: it then only feeds other plugins.
pub struct ActivePlugin {
    pub id: u32,
    pub instance: Arc<Mutex<Box<dyn PluginInstance>>>,
    pub gain: Arc<Mutex<f32>>,
    pub midi_queue: Arc<Mutex<Vec<MidiEvent>>>,
    /// Channel counts, read once when the node is attached (no locking in the audio thread).
    pub num_inputs: usize,
    pub num_outputs: usize,
    /// Mixed to the output device.
    pub to_output: bool,
}

static ACTIVE: OnceLock<Arc<Mutex<Vec<ActivePlugin>>>> = OnceLock::new();

/// Explicit audio links as `(target, source)`, replaced by `set_links`.
static LINKS: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());

fn active_list() -> Arc<Mutex<Vec<ActivePlugin>>> {
    ACTIVE.get_or_init(|| Arc::new(Mutex::new(Vec::new()))).clone()
}

/// Ids of the plugins mixed to the output device (armed with `audio connect`).
pub fn active_ids() -> Vec<u32> {
    active_list().lock().unwrap().iter().filter(|p| p.to_output).map(|p| p.id).collect()
}

/// Which nodes must stay in the graph: armed nodes, and every node that is an end of a link.
/// Returns the ids to keep.
pub fn nodes_to_keep(nodes: &[(u32, bool)], links: &[(u32, u32)]) -> Vec<u32> {
    nodes
        .iter()
        .filter(|(id, armed)| *armed || links.iter().any(|(t, s)| t == id || s == id))
        .map(|(id, _)| *id)
        .collect()
}

/// Remove nodes that are neither armed nor linked, and deactivate them.
fn prune(links: &[(u32, u32)]) {
    let active = active_list();
    let removed: Vec<ActivePlugin> = {
        let mut guard = active.lock().unwrap();
        let nodes: Vec<(u32, bool)> = guard.iter().map(|p| (p.id, p.to_output)).collect();
        let keep = nodes_to_keep(&nodes, links);
        let mut removed = Vec::new();
        let mut i = 0;
        while i < guard.len() {
            if keep.contains(&guard[i].id) {
                i += 1;
            } else {
                removed.push(guard.remove(i));
            }
        }
        removed
    };
    for ap in removed {
        if let Ok(mut inst) = ap.instance.lock() {
            inst.deactivate();
        }
    }
}

/// Add a plugin to the graph (activating it), or update an existing node. `to_output` marks
/// it as mixed to the device; an existing node keeps its flag unless `to_output` is true.
pub fn attach(
    id: u32,
    instance: Arc<Mutex<Box<dyn PluginInstance>>>,
    gain: Arc<Mutex<f32>>,
    midi_queue: Arc<Mutex<Vec<MidiEvent>>>,
    to_output: bool,
) -> Result<()> {
    let active = active_list();
    let mut guard = active.lock().unwrap();
    if let Some(node) = guard.iter_mut().find(|p| p.id == id) {
        if to_output {
            node.to_output = true;
        }
        return Ok(());
    }
    let (num_inputs, num_outputs) = {
        let mut inst = instance.lock().unwrap();
        inst.set_sample_rate(get_sample_rate() as f64);
        inst.set_block_size(get_buffer_size());
        let _ = inst.activate();
        let info = inst.info();
        (info.num_inputs as usize, info.num_outputs as usize)
    };
    guard.push(ActivePlugin { id, instance, gain, midi_queue, num_inputs, num_outputs, to_output });
    Ok(())
}

/// Mix plugin `id` to the output device (`audio connect`). Adds it if needed.
pub fn arm(id: u32, instance: Arc<Mutex<Box<dyn PluginInstance>>>, gain: Arc<Mutex<f32>>, midi_queue: Arc<Mutex<Vec<MidiEvent>>>) -> Result<()> {
    attach(id, instance, gain, midi_queue, true)
}

/// Stop mixing plugin `id` to the device (`audio disconnect <id>`). A plugin that is still
/// part of a link keeps processing, so the chain keeps working.
pub fn disarm(id: u32) -> Result<()> {
    let links = LINKS.lock().unwrap().clone();
    if let Some(node) = active_list().lock().unwrap().iter_mut().find(|p| p.id == id) {
        node.to_output = false;
    }
    prune(&links);
    Ok(())
}

/// Stop mixing every plugin to the device. Links keep working.
pub fn unmix_all() {
    let links = LINKS.lock().unwrap().clone();
    for node in active_list().lock().unwrap().iter_mut() {
        node.to_output = false;
    }
    prune(&links);
}

/// Replace the link list and drop nodes that are no longer needed.
pub fn set_links(links: &[(u32, u32)]) {
    *LINKS.lock().unwrap() = links.to_vec();
    prune(links);
}

/// Remove plugin `id` from the graph (unload). Deactivates it.
pub fn detach(id: u32) {
    let removed = {
        let active = active_list();
        let mut guard = active.lock().unwrap();
        guard.iter().position(|p| p.id == id).map(|pos| guard.remove(pos))
    };
    if let Some(ap) = removed {
        if let Ok(mut inst) = ap.instance.lock() {
            inst.deactivate();
        }
    }
    let links: Vec<(u32, u32)> = LINKS.lock().unwrap().iter().copied().filter(|(t, s)| *t != id && *s != id).collect();
    set_links(&links);
}

/// Remove every node and deactivate it (shutdown).
pub fn disarm_all() {
    let active = active_list();
    let all: Vec<ActivePlugin> = active.lock().unwrap().drain(..).collect();
    for ap in all {
        if let Ok(mut inst) = ap.instance.lock() {
            inst.deactivate();
        }
    }
}

/// Lock a plugin instance for one audio block. A plugin that is busy (for example during
/// `plugin show`) is waited for, not skipped: a skipped block would output silence, and any
/// effect downstream of it would lose its input and its tail.
fn lock_instance(m: &Mutex<Box<dyn PluginInstance>>) -> std::sync::MutexGuard<'_, Box<dyn PluginInstance>> {
    match m.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            midi_stats::on_deferred();
            m.lock().unwrap_or_else(|p| p.into_inner())
        }
    }
}

/// Order in which the graph must be rendered: each node after its source. `sources[i]` is the
/// index of node `i`'s source, if any. Cycles cannot be created through `connect`; if one is
/// present anyway, the remaining nodes are appended in index order so every node is rendered.
pub fn topological_order(sources: &[Option<usize>]) -> Vec<usize> {
    let n = sources.len();
    let mut placed = vec![false; n];
    let mut order = Vec::with_capacity(n);
    while order.len() < n {
        let before = order.len();
        for i in 0..n {
            if placed[i] {
                continue;
            }
            let ready = match sources[i] {
                Some(s) if s < n => placed[s],
                _ => true,
            };
            if ready {
                placed[i] = true;
                order.push(i);
            }
        }
        if order.len() == before {
            for i in 0..n {
                if !placed[i] {
                    placed[i] = true;
                    order.push(i);
                }
            }
        }
    }
    order
}

/// Fill `dst` from `src` with the channel mapping used for links:
/// same count copies, mono to stereo duplicates, stereo to mono sums at -3 dB.
/// Without a source, `dst` is silence.
pub fn route_channels(src: &[&[f32]], dst: &mut [&mut [f32]]) {
    let ns = src.len();
    let nd = dst.len();
    if ns == 0 {
        for d in dst.iter_mut() {
            d.fill(0.0);
        }
        return;
    }
    if ns == 2 && nd == 1 {
        let (l, r) = (src[0], src[1]);
        for (i, out) in dst[0].iter_mut().enumerate() {
            *out = (l[i] + r[i]) * std::f32::consts::FRAC_1_SQRT_2;
        }
        return;
    }
    for (c, d) in dst.iter_mut().enumerate() {
        let s = src[c % ns];
        let n = d.len().min(s.len());
        d[..n].copy_from_slice(&s[..n]);
        d[n..].fill(0.0);
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

    // Scratch buffers reused across blocks (one set per stream), so the audio thread does not
    // allocate the sample buffers on every block.
    #[derive(Default)]
    struct Scratch {
        // [node][channel][frame]: each node's output for the current block
        outs: Vec<Vec<Vec<f32>>>,
        // [channel][frame]: input of the node being rendered
        ins: Vec<Vec<f32>>,
    }
    let scratch = Mutex::new(Scratch::default());

    // Renders one block as f32; converted to the device's sample format by `open_stream`.
    // Nodes run in link order, so a node's input is its source's output for this block.
    // Only armed nodes are mixed to the device.
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
        let links = match LINKS.try_lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let mut sc = scratch.lock().unwrap();
        let Scratch { outs, ins } = &mut *sc;
        let master = get_master_gain();

        // Index in `active` of each node's source, if the source is in the graph.
        let sources: Vec<Option<usize>> = active
            .iter()
            .map(|node| {
                links
                    .iter()
                    .find(|(target, _)| *target == node.id)
                    .and_then(|(_, source)| active.iter().position(|p| p.id == *source))
            })
            .collect();
        let order = topological_order(&sources);
        if outs.len() < active.len() {
            outs.resize_with(active.len(), Vec::new);
        }

        for &idx in &order {
            let node = &active[idx];
            let po = node.num_outputs.max(1);
            let nt = node.num_inputs;

            // Input: the source's output for this block, mapped to this node's channel count.
            if ins.len() < nt {
                ins.resize_with(nt, Vec::new);
            }
            for ch in ins.iter_mut().take(nt) {
                if ch.len() < frames {
                    ch.resize(frames, 0.0);
                }
            }
            {
                let mut dst: Vec<&mut [f32]> = ins.iter_mut().take(nt).map(|b| &mut b[..frames]).collect();
                match sources[idx] {
                    Some(sidx) => {
                        let src: Vec<&[f32]> = outs[sidx]
                            .iter()
                            .filter(|b| b.len() >= frames)
                            .map(|b| &b[..frames])
                            .collect();
                        route_channels(&src, &mut dst);
                    }
                    None => route_channels(&[], &mut dst),
                }
            }

            // Output buffers for this node, cleared so a node that is skipped below is silent.
            let out_bufs = &mut outs[idx];
            out_bufs.resize_with(po, Vec::new);
            for b in out_bufs.iter_mut() {
                if b.len() < frames {
                    b.resize(frames, 0.0);
                }
                b[..frames].fill(0.0);
            }

            // Lock the instance first. If another thread holds it, leave the queued MIDI
            // events where they are and try again on the next block; taking them first would
            // throw them away (lost note-offs).
            let mut inst = lock_instance(&node.instance);
            // Never block the audio thread on the queue: if the MIDI thread is pushing right
            // now, play this block without new events and deliver them on the next one.
            let events: Vec<MidiEvent> = node
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
            let in_refs: Vec<&[f32]> = ins.iter().take(nt).map(|b| &b[..frames]).collect();
            let mut out_refs: Vec<&mut [f32]> = outs[idx].iter_mut().map(|b| &mut b[..frames]).collect();
            inst.process(&in_refs, &mut out_refs, frames);
            drop(inst);

            if node.to_output {
                let gain = node.gain.try_lock().map(|g| *g).unwrap_or(1.0);
                let out = &outs[idx];
                for f in 0..frames {
                    for c in 0..channels_c {
                        data[f * channels_c + c] += out[c % po][f] * gain * master;
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
    fn order_renders_source_before_target() {
        // node 0 reads node 1, node 1 reads node 2, node 2 is a generator
        assert_eq!(topological_order(&[Some(1), Some(2), None]), vec![2, 1, 0]);
    }

    #[test]
    fn order_fan_out_keeps_source_first() {
        assert_eq!(topological_order(&[None, Some(0), Some(0)]), vec![0, 1, 2]);
    }

    #[test]
    fn order_ignores_out_of_range_source() {
        assert_eq!(topological_order(&[Some(7), None]), vec![0, 1]);
    }

    #[test]
    fn order_terminates_on_cycle_and_keeps_every_node() {
        let order = topological_order(&[Some(1), Some(0)]);
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(sorted, vec![0, 1]);
    }

    #[test]
    fn lock_instance_waits_for_busy_plugin_instead_of_skipping() {
        let m: Arc<Mutex<Box<dyn PluginInstance>>> = Arc::new(Mutex::new(Box::new(NullPlugin)));
        let holder = {
            let m = m.clone();
            std::thread::spawn(move || {
                let _guard = m.lock().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(30));
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(5));
        // Blocks until the holder releases, then returns the instance (no skipped block).
        let start = std::time::Instant::now();
        let _g = lock_instance(&m);
        assert!(start.elapsed() >= std::time::Duration::from_millis(10));
        holder.join().unwrap();
    }

    struct NullPlugin;
    impl PluginInstance for NullPlugin {
        fn info(&self) -> &spaph_plugin::PluginInfo { unimplemented!() }
        fn set_sample_rate(&mut self, _sr: f64) {}
        fn process(&mut self, _i: &[&[f32]], _o: &mut [&mut [f32]], _n: usize) {}
        fn get_param(&self, _index: u32) -> f64 { 0.0 }
        fn set_param(&mut self, _index: u32, _value: f64) {}
        fn save_state(&self) -> spaph_types::Result<Vec<u8>> { Ok(vec![]) }
        fn load_state(&mut self, _data: &[u8]) -> spaph_types::Result<()> { Ok(()) }
    }

    #[test]
    fn route_same_channel_count_copies() {
        let (l, r) = ([1.0f32, 2.0], [3.0f32, 4.0]);
        let mut a = [0.0f32; 2];
        let mut b = [0.0f32; 2];
        route_channels(&[&l, &r], &mut [&mut a, &mut b]);
        assert_eq!(a, [1.0, 2.0]);
        assert_eq!(b, [3.0, 4.0]);
    }

    #[test]
    fn route_mono_to_stereo_duplicates() {
        let m = [0.5f32, -0.5];
        let mut a = [0.0f32; 2];
        let mut b = [0.0f32; 2];
        route_channels(&[&m], &mut [&mut a, &mut b]);
        assert_eq!(a, [0.5, -0.5]);
        assert_eq!(b, [0.5, -0.5]);
    }

    #[test]
    fn route_stereo_to_mono_sums_at_minus_3db() {
        let (l, r) = ([1.0f32], [1.0f32]);
        let mut m = [0.0f32; 1];
        route_channels(&[&l, &r], &mut [&mut m]);
        assert!((m[0] - std::f32::consts::FRAC_1_SQRT_2 * 2.0).abs() < 1e-6, "{}", m[0]);
    }

    #[test]
    fn route_without_source_gives_silence() {
        let mut a = [9.0f32; 2];
        route_channels(&[], &mut [&mut a]);
        assert_eq!(a, [0.0, 0.0]);
    }

    #[test]
    fn kept_nodes_are_armed_or_linked() {
        // (id, armed); links are (target, source)
        let nodes = [(1, true), (2, false), (3, false), (4, false)];
        let links = [(2, 1)]; // 1 feeds 2 (2 is not armed)
        let keep = nodes_to_keep(&nodes, &links);
        assert!(keep.contains(&1), "armed node kept");
        assert!(keep.contains(&2), "linked target kept even if not armed");
        assert!(!keep.contains(&3), "unarmed unlinked node dropped");
        assert!(!keep.contains(&4));
    }

    #[test]
    fn kept_nodes_drop_unlinked_after_disconnect() {
        let nodes = [(2, false)];
        assert!(nodes_to_keep(&nodes, &[]).is_empty());
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
