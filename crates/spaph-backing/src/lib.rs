//! Backing-track player for spaph.
//!
//! A stereo file is decoded on a worker thread, resampled to the stream rate and queued in a
//! buffer. The audio callback mixes that buffer into the output with [`mix_into`], so it never
//! decodes or touches the disk. Only one track is loaded at a time.

mod decode;

use std::collections::VecDeque;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use spaph_types::{Result, SpaphError};

pub const DEFAULT_VOLUME_DB: f32 = 0.0;
pub const VOLUME_MIN_DB: f32 = -60.0;
pub const VOLUME_MAX_DB: f32 = 6.0;
/// Amount `backing volume +` / `-` changes the level by, in dB.
pub const VOLUME_STEP_DB: f32 = 2.0;
/// How far ahead of playback the decoder keeps the buffer filled.
const BUFFER_SECONDS: f32 = 2.0;
const DEFAULT_OUT_RATE: u32 = 48000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Stopped,
    Playing,
    Paused,
}

impl State {
    fn from_u8(v: u8) -> State {
        match v {
            1 => State::Playing,
            2 => State::Paused,
            _ => State::Stopped,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub file: Option<String>,
    pub state: State,
    pub position_secs: f64,
    pub volume_db: f32,
}

struct Shared {
    state: AtomicU8,
    /// Interleaved stereo at the output rate, waiting to be mixed.
    buffer: Mutex<VecDeque<f32>>,
    /// Frames already played since the start of the file.
    position: AtomicU64,
    /// The decoder has queued the whole file.
    end_of_stream: AtomicBool,
    /// Linear gain of the current volume setting (f32 bits).
    gain: AtomicU32,
    volume_db: Mutex<f32>,
    out_rate: AtomicU32,
    file: Mutex<Option<String>>,
}

static SHARED: OnceLock<Shared> = OnceLock::new();
static WORKER: Mutex<Option<Worker>> = Mutex::new(None);
static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

fn shared() -> &'static Shared {
    SHARED.get_or_init(|| Shared {
        state: AtomicU8::new(State::Stopped as u8),
        buffer: Mutex::new(VecDeque::new()),
        position: AtomicU64::new(0),
        end_of_stream: AtomicBool::new(false),
        gain: AtomicU32::new(db_to_gain(DEFAULT_VOLUME_DB).to_bits()),
        volume_db: Mutex::new(DEFAULT_VOLUME_DB),
        out_rate: AtomicU32::new(DEFAULT_OUT_RATE),
        file: Mutex::new(None),
    })
}

enum Command {
    /// Move to this many seconds from the start of the file.
    Seek(f64),
}

struct Worker {
    tx: Sender<Command>,
    handle: JoinHandle<()>,
}

/// Folder that backing file names are resolved in (from `backing_dir` in the config).
pub fn set_backing_dir(dir: Option<PathBuf>) {
    *DIR.lock().unwrap() = dir;
}

/// Sample rate of the output stream. Tracks at another rate are resampled to it.
pub fn set_output_rate(rate: u32) {
    shared().out_rate.store(rate, Relaxed);
}

pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

pub fn clamp_db(db: f32) -> f32 {
    db.clamp(VOLUME_MIN_DB, VOLUME_MAX_DB)
}

/// Resolve `name` inside `dir`. Only plain relative names are accepted, so a track cannot
/// reach outside the backing folder.
pub fn resolve(dir: &Path, name: &str) -> Result<PathBuf> {
    let name = name.trim().trim_matches('"');
    let rel = Path::new(name);
    let plain = !name.is_empty()
        && !name.contains(['\0', ':'])
        && !rel.is_absolute()
        && rel.components().all(|c| matches!(c, Component::Normal(_)));
    if !plain {
        return Err(SpaphError::Audio(format!("invalid backing file name: {name:?}")));
    }
    Ok(dir.join(rel))
}

/// Load `name` (relative to the backing folder), replacing any track already loaded. Decoding
/// errors are returned here, before the previous track is dropped.
pub fn load(name: &str) -> Result<()> {
    let dir = DIR
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| SpaphError::Audio("backing_dir is not set in the config".into()))?;
    let path = resolve(&dir, name)?;
    let rate = shared().out_rate.load(Relaxed);
    let source = decode::open(&path, rate)?;

    unload();
    let s = shared();
    *s.file.lock().unwrap() = Some(name.trim().trim_matches('"').to_string());
    let (tx, rx) = mpsc::channel();
    let handle = thread::Builder::new()
        .name("backing-decoder".into())
        .spawn(move || run(source, rx))
        .map_err(|e| SpaphError::Audio(format!("cannot start decoder thread: {e}")))?;
    *WORKER.lock().unwrap() = Some(Worker { tx, handle });
    Ok(())
}

/// Stop and forget the loaded track.
pub fn unload() {
    let worker = WORKER.lock().unwrap().take();
    if let Some(Worker { tx, handle }) = worker {
        drop(tx);
        let _ = handle.join();
    }
    let s = shared();
    s.state.store(State::Stopped as u8, Relaxed);
    s.buffer.lock().unwrap().clear();
    s.position.store(0, Relaxed);
    s.end_of_stream.store(false, Relaxed);
    *s.file.lock().unwrap() = None;
}

fn send(cmd: Command) -> Result<()> {
    let guard = WORKER.lock().unwrap();
    let worker = guard
        .as_ref()
        .ok_or_else(|| SpaphError::Audio("no backing track loaded".into()))?;
    worker
        .tx
        .send(cmd)
        .map_err(|_| SpaphError::Audio("backing decoder stopped".into()))
}

fn require_loaded() -> Result<()> {
    if shared().file.lock().unwrap().is_none() {
        return Err(SpaphError::Audio("no backing track loaded".into()));
    }
    Ok(())
}

/// Start playback. After the end of the file, starts again from the beginning.
pub fn play() -> Result<()> {
    require_loaded()?;
    let s = shared();
    if s.end_of_stream.load(Relaxed) && s.buffer.lock().unwrap().is_empty() {
        send(Command::Seek(0.0))?;
    }
    s.state.store(State::Playing as u8, Relaxed);
    Ok(())
}

/// Pause in place. Does nothing when not playing.
pub fn pause() {
    let s = shared();
    let _ = s
        .state
        .compare_exchange(State::Playing as u8, State::Paused as u8, Relaxed, Relaxed);
}

/// Stop and return to the beginning.
pub fn stop() -> Result<()> {
    require_loaded()?;
    shared().state.store(State::Stopped as u8, Relaxed);
    send(Command::Seek(0.0))
}

/// Jump to the beginning, keeping the current state.
pub fn begin() -> Result<()> {
    require_loaded()?;
    send(Command::Seek(0.0))
}

/// Move by `delta` seconds from the current position (negative goes back).
pub fn seek_relative(delta: f64) -> Result<()> {
    require_loaded()?;
    let s = shared();
    let rate = s.out_rate.load(Relaxed) as f64;
    let current = s.position.load(Relaxed) as f64 / rate;
    send(Command::Seek((current + delta).max(0.0)))
}

/// Set the volume in dB, clamped to the supported range. Returns the level that was set.
pub fn set_volume_db(db: f32) -> f32 {
    let db = clamp_db(db);
    let s = shared();
    s.gain.store(db_to_gain(db).to_bits(), Relaxed);
    *s.volume_db.lock().unwrap() = db;
    db
}

/// Change the volume by `delta` dB.
pub fn change_volume_db(delta: f32) -> f32 {
    let current = *shared().volume_db.lock().unwrap();
    set_volume_db(current + delta)
}

pub fn status() -> Status {
    let s = shared();
    let rate = s.out_rate.load(Relaxed) as f64;
    Status {
        file: s.file.lock().unwrap().clone(),
        state: State::from_u8(s.state.load(Relaxed)),
        position_secs: s.position.load(Relaxed) as f64 / rate,
        volume_db: *s.volume_db.lock().unwrap(),
    }
}

/// Add `played` frames of the backing track into `out` (interleaved, `channels` per frame).
/// Called from the audio callback. Silent when not playing, and never waits for the decoder.
pub fn mix_into(out: &mut [f32], channels: usize) {
    let s = shared();
    if channels == 0 || State::from_u8(s.state.load(Relaxed)) != State::Playing {
        return;
    }
    let Ok(mut buffer) = s.buffer.try_lock() else {
        return;
    };
    let gain = f32::from_bits(s.gain.load(Relaxed));
    let frames = out.len() / channels;
    let played = mix_block(&mut buffer, out, channels, gain);
    s.position.fetch_add(played as u64, Relaxed);
    if played < frames && s.end_of_stream.load(Relaxed) && buffer.is_empty() {
        s.state.store(State::Stopped as u8, Relaxed);
    }
}

/// Mix as many stereo frames from `buffer` as fit in `out`. Returns the number of frames used.
pub fn mix_block(buffer: &mut VecDeque<f32>, out: &mut [f32], channels: usize, gain: f32) -> usize {
    let frames = out.len() / channels;
    let mut used = 0;
    while used < frames && buffer.len() >= 2 {
        let left = buffer.pop_front().unwrap_or(0.0) * gain;
        let right = buffer.pop_front().unwrap_or(0.0) * gain;
        let base = used * channels;
        if channels == 1 {
            out[base] += 0.5 * (left + right);
        } else {
            out[base] += left;
            out[base + 1] += right;
        }
        used += 1;
    }
    used
}

/// Decoder thread: keeps the buffer filled and applies seek requests.
fn run(mut source: decode::Source, rx: Receiver<Command>) {
    let s = shared();
    let target = (BUFFER_SECONDS * s.out_rate.load(Relaxed) as f32) as usize * 2;
    loop {
        loop {
            match rx.try_recv() {
                Ok(Command::Seek(secs)) => {
                    match source.seek(secs) {
                        Ok(()) => {
                            let mut buffer = s.buffer.lock().unwrap();
                            buffer.clear();
                            let rate = s.out_rate.load(Relaxed) as f64;
                            s.position.store((secs * rate) as u64, Relaxed);
                            s.end_of_stream.store(false, Relaxed);
                        }
                        Err(e) => tracing::error!("backing seek failed: {e}"),
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        let full = s.buffer.lock().unwrap().len() >= target;
        if s.end_of_stream.load(Relaxed) || full {
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        match source.next_chunk() {
            Ok(Some(samples)) => s.buffer.lock().unwrap().extend(samples),
            Ok(None) => s.end_of_stream.store(true, Relaxed),
            Err(e) => {
                tracing::error!("backing decode failed: {e}");
                s.end_of_stream.store(true, Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_resolve_inside_the_folder() {
        let dir = Path::new("/music/backing");
        assert_eq!(resolve(dir, "tenessee.flac").unwrap(), dir.join("tenessee.flac"));
        assert_eq!(resolve(dir, "\"we are family.flac\"").unwrap(), dir.join("we are family.flac"));
        assert_eq!(resolve(dir, "sub/a.flac").unwrap(), dir.join("sub").join("a.flac"));
    }

    #[test]
    fn names_that_leave_the_folder_are_rejected() {
        let dir = Path::new("/music/backing");
        for bad in ["", "   ", "../x.flac", "a/../../x.flac", "/etc/passwd", "C:x.flac", "a\0b.flac"] {
            assert!(resolve(dir, bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn gain_and_volume_limits() {
        assert!((db_to_gain(0.0) - 1.0).abs() < 1e-6);
        assert!((db_to_gain(-6.0206) - 0.5).abs() < 1e-4);
        assert_eq!(clamp_db(-100.0), VOLUME_MIN_DB);
        assert_eq!(clamp_db(100.0), VOLUME_MAX_DB);
        assert_eq!(clamp_db(-3.0), -3.0);
    }

    #[test]
    fn mix_block_adds_stereo_frames_with_gain() {
        let mut buffer: VecDeque<f32> = [0.5, -0.5, 0.25, 0.25].into_iter().collect();
        let mut out = [0.0f32; 4];
        let used = mix_block(&mut buffer, &mut out, 2, 2.0);
        assert_eq!(used, 2);
        assert_eq!(out, [1.0, -1.0, 0.5, 0.5]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn mix_block_stops_when_the_buffer_runs_dry() {
        let mut buffer: VecDeque<f32> = [0.5, 0.5].into_iter().collect();
        let mut out = [0.0f32; 8];
        assert_eq!(mix_block(&mut buffer, &mut out, 2, 1.0), 1);
        assert_eq!(&out[2..], &[0.0; 6]);
    }

    #[test]
    fn mix_block_sums_to_mono_outputs() {
        let mut buffer: VecDeque<f32> = [1.0, 0.0].into_iter().collect();
        let mut out = [0.0f32; 1];
        assert_eq!(mix_block(&mut buffer, &mut out, 1, 1.0), 1);
        assert_eq!(out[0], 0.5);
    }

    #[test]
    fn transport_needs_a_loaded_track() {
        unload();
        assert!(play().is_err());
        assert!(stop().is_err());
        assert!(seek_relative(10.0).is_err());
        pause();
        assert_eq!(status().state, State::Stopped);
    }
}
