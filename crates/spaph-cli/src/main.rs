use clap::{Parser, ValueEnum};
use spaph_config::{Config, Overrides, ScriptLine};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use spaph_midi::{self, MidiEvent};
use spaph_types::Result;
use spaph_core::PluginManager;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy)]
struct AudioDefaults {
    sample_rate: Option<u32>,
    buffer_size: Option<u32>,
    device_index: Option<usize>,
}

static AUDIO_DEFAULTS: std::sync::OnceLock<std::sync::Mutex<AudioDefaults>> = std::sync::OnceLock::new();

static ACTIVE_AUDIO_PLUGIN: std::sync::Mutex<Option<u32>> = std::sync::Mutex::new(None);

fn audio_defaults() -> &'static std::sync::Mutex<AudioDefaults> {
    AUDIO_DEFAULTS.get_or_init(|| std::sync::Mutex::new(AudioDefaults {
        sample_rate: None,
        buffer_size: None,
        device_index: None,
    }))
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum LogLevel {
    None,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

/// Command-line options. Commands are not taken here: they come from the
/// configuration file (`script`) or are typed at the prompt.
#[derive(Parser)]
#[command(name = "spaph", version, about = "Simple Portable Audio Plugins Host")]
struct Cli {
    /// Configuration file (default: ./spaph.json if present)
    #[arg(long)]
    config: Option<PathBuf>,

    /// Sample rate override (Hz)
    #[arg(long)]
    sample_rate: Option<u32>,

    /// Buffer size override (frames)
    #[arg(long)]
    buffer_size: Option<u32>,

    /// Audio output device index override
    #[arg(long)]
    audio_device: Option<usize>,

    /// Log level override
    #[arg(long, value_enum)]
    log_level: Option<LogLevel>,

    /// OSC port override
    #[arg(long)]
    osc_port: Option<u16>,
}

impl LogLevel {
    fn name(self) -> &'static str {
        match self {
            LogLevel::None => "none",
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

/// True while a script line is being executed (see `run_script`).
static IN_SCRIPT: AtomicBool = AtomicBool::new(false);
/// Set by `cmd_err!` when a command fails; checked after each script line.
static CMD_ERROR: AtomicBool = AtomicBool::new(false);

/// Print a command error and flag it, so a script can stop on it.
macro_rules! cmd_err {
    ($($arg:tt)*) => {{
        CMD_ERROR.store(true, Ordering::Relaxed);
        eprintln!($($arg)*);
    }};
}

fn split_args(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    // Quotes may be " or '; a quote is closed only by the same character that opened it,
    // so the other kind can appear inside (e.g. "it's here").
    let mut quote: Option<char> = None;
    for c in line.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn load_plugin(path: &PathBuf) -> Result<()> {
    let id = manager().lock().unwrap().load(path)?;
    let mgr = manager().lock().unwrap();
    let summary = mgr.list().into_iter().find(|s| s.id == id).unwrap();
    drop(mgr);
    println!("Loaded plugin #{}: {}", id, summary.name);
    println!("Vendor:  {}", summary.vendor);
    println!("Format:  {:?}", summary.format);
    println!("Inputs:  {}", summary.num_inputs);
    println!("Outputs: {}", summary.num_outputs);
    println!("Params:  {}", summary.num_params);
    Ok(())
}

fn print_help() {
    println!("spaph - Simple Portable Audio Plugins Host");
    println!("Commands:");
    println!("  help                          Show this help");
    println!("  load <path>                   Load a plugin and display its interface");
    println!("  list midi                     List MIDI input ports");
    println!("  list plugins                  List loaded plugins");
    println!("  list presets <id>             List presets of a plugin");
    println!("  list audio                    List audio output devices");
    println!("  midi listen <port>            Capture MIDI input (index or name), Enter to stop");
    println!("  unload <id>                   Unload a plugin by id");
    println!("  show <plugin_id>              Show plugin editor UI (VST3)");
    println!("  select <plugin_id> <preset_id> Select a preset (1-based)");
    println!("  connect audio <id> [dev]      Connect plugin output to an audio device");
    println!("  connect midi <port> <id>      Connect a MIDI input to a plugin");
    println!("  volume [<id>] <level>         Set host-side output gain (per plugin)");
    println!("  param list <id>               List plugin parameters");
    println!("  param get <id> <index>        Get a parameter value");
    println!("  param set <id> <idx|volume> <value>  Set a parameter or volume");
    println!("  preset scan <dir>             Scan a directory for preset files");
    println!("  preset load <id> <file>       Load a preset file into a plugin");
    println!("  preset save <id> <file>       Save current state as preset file");
    println!("  disconnect                    Stop audio and MIDI connections");
    println!("  quit | exit                   Exit the shell");
}

fn midi_name(ev: &MidiEvent) -> String {
    // basic pretty print
    if ev.data.is_empty() {
        return format!("ts={} <empty>", ev.timestamp);
    }
    let status = ev.data[0];
    match status & 0xF0 {
        0x90 => format!("ts={} NoteOn ch{} note{} vel{}", ev.timestamp, status & 0x0F, ev.data.get(1).copied().unwrap_or(0), ev.data.get(2).copied().unwrap_or(0)),
        0x80 => format!("ts={} NoteOff ch{} note{} vel{}", ev.timestamp, status & 0x0F, ev.data.get(1).copied().unwrap_or(0), ev.data.get(2).copied().unwrap_or(0)),
        0xB0 => format!("ts={} CC ch{} ctrl{} val{}", ev.timestamp, status & 0x0F, ev.data.get(1).copied().unwrap_or(0), ev.data.get(2).copied().unwrap_or(0)),
        0xC0 => format!("ts={} ProgramChange ch{} prog{}", ev.timestamp, status & 0x0F, ev.data.get(1).copied().unwrap_or(0)),
        _ => format!("ts={} bytes {:?}", ev.timestamp, ev.data),
    }
}

fn midi_listen(port: &str) {
    let listener = if let Ok(idx) = port.parse::<usize>() {
        spaph_midi::open_input(idx)
    } else {
        spaph_midi::open_input_by_name(port)
    };
    let listener = match listener {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: {}", e);
            return;
        }
    };
    println!("Listening on '{}'. Press Enter to stop.", port);
    use std::sync::atomic::{AtomicBool, Ordering};
    let stop = Arc::new(AtomicBool::new(false));
    let listener = Arc::new(Mutex::new(listener));
    let l2 = listener.clone();
    let stop2 = stop.clone();
    let handle = std::thread::spawn(move || {
        while !stop2.load(Ordering::Relaxed) {
            let res = {
                let l = l2.lock().unwrap();
                l.rx.recv_timeout(Duration::from_millis(100))
            };
            match res {
                Ok(ev) => println!("{}", midi_name(&ev)),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(_) => {}
            }
        }
    });
    // wait for enter
    let mut buf = String::new();
    let _ = std::io::stdin().read_line(&mut buf);
    stop.store(true, Ordering::Relaxed);
    drop(listener);
    let _ = handle.join();
    println!("stopped.");
}

static MIDI_STOP_FLAGS: std::sync::OnceLock<std::sync::Mutex<Vec<std::sync::Arc<std::sync::atomic::AtomicBool>>>> =
    std::sync::OnceLock::new();

fn midi_stop_flags() -> &'static std::sync::Mutex<Vec<std::sync::Arc<std::sync::atomic::AtomicBool>>> {
    MIDI_STOP_FLAGS.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

fn midi_connect(port: String, plugin_id: u32) {
    use std::sync::atomic::{AtomicBool, Ordering};
    let mgr = manager().lock().unwrap();
    let q = mgr.get_midi_queue(plugin_id);
    drop(mgr);
    let q = match q {
        Some(q) => q,
        None => {
            cmd_err!("plugin {} not found", plugin_id);
            return;
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    midi_stop_flags().lock().unwrap().push(stop.clone());
    let port_clone = port.clone();
    let handle = std::thread::spawn(move || {
        let listener = if let Ok(idx) = port_clone.parse::<usize>() {
            spaph_midi::open_input(idx)
        } else {
            spaph_midi::open_input_by_name(&port_clone)
        };
        let listener = match listener {
            Ok(l) => l,
            Err(e) => {
                cmd_err!("error: {}", e);
                return;
            }
        };
        println!("midi connected (port {}) to plugin {}", port_clone, plugin_id);
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            match listener.rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(ev) => {
                    let mut q = q.lock().unwrap();
                    q.push(ev);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(_) => {}
            }
        }
    });
    midi_thread_handles().lock().unwrap().push(handle);
}

fn parse_select_args(parts: &[String]) -> Option<(u32, u32)> {
    if parts.len() >= 4 && parts[1] == "preset" {
        Some((parts[2].parse().ok()?, parts[3].parse().ok()?))
    } else if parts.len() >= 3 {
        Some((parts[1].parse().ok()?, parts[2].parse().ok()?))
    } else {
        None
    }
}

fn scan_presets(dir: &PathBuf) {
    fn walk(dir: &PathBuf, out: &mut Vec<PathBuf>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                    let ext = ext.to_lowercase();
                    if ext == "vstpreset" || ext == "fxp" || ext == "fxb" {
                        out.push(p);
                    }
                }
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, &mut files);
    files.sort();
    if files.is_empty() {
        println!("(no preset files found)");
    } else {
        for f in files {
            println!("{}", f.display());
        }
    }
}

static OSC_STOP: std::sync::OnceLock<std::sync::Arc<std::sync::atomic::AtomicBool>> = std::sync::OnceLock::new();
static OSC_THREAD: std::sync::OnceLock<std::sync::Mutex<Option<std::thread::JoinHandle<()>>>> = std::sync::OnceLock::new();

fn osc_stop() -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    OSC_STOP.get_or_init(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))).clone()
}
fn osc_thread_slot() -> &'static std::sync::Mutex<Option<std::thread::JoinHandle<()>>> {
    OSC_THREAD.get_or_init(|| std::sync::Mutex::new(None))
}

fn cleanup_connections() {
    tracing::debug!("shutdown: stopping audio");
    *AUDIO_HANDLE.lock().unwrap() = None;
    tracing::debug!("shutdown: stopping MIDI");
    for f in midi_stop_flags().lock().unwrap().iter() {
        f.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    midi_stop_flags().lock().unwrap().clear();
    let handles: Vec<_> = midi_thread_handles().lock().unwrap().drain(..).collect();
    for h in handles {
        let _ = h.join();
    }
    tracing::debug!("shutdown: stopping OSC server");
    osc_stop().store(true, std::sync::atomic::Ordering::Relaxed);
    if let Some(h) = osc_thread_slot().lock().unwrap().take() {
        let _ = h.join();
    }
}

/// Seconds allowed for the whole shutdown (stop audio/MIDI/OSC, then unload plugins).
const SHUTDOWN_TIMEOUT_SECS: u64 = 6;

/// Stop everything and unload all plugins, then exit with `code`.
///
/// Runs on a helper thread with a timeout, so a plugin whose teardown hangs cannot freeze
/// the program: after the timeout a warning is printed and the process exits anyway.
fn shutdown(code: i32) -> ! {
    let finished = spaph_core::run_with_timeout(
        || {
            cleanup_connections();
            let plugins = manager().lock().unwrap().take_all();
            tracing::debug!("shutdown: unloading {} plugin(s)", plugins.len());
            drop(plugins);
            tracing::debug!("shutdown: plugins unloaded");
        },
        std::time::Duration::from_secs(SHUTDOWN_TIMEOUT_SECS),
    );
    if finished {
        tracing::debug!("shutdown: complete");
    } else {
        eprintln!(
            "warning: shutdown did not finish within {}s (a plugin may be hung); forcing exit",
            SHUTDOWN_TIMEOUT_SECS
        );
    }
    std::process::exit(code);
}

fn run_line(line: &str) -> bool {
    let parts = split_args(line);
    if parts.is_empty() {
        return true;
    }
    match parts[0].as_str() {
        "help" => print_help(),
        "load" => {
            if parts.len() < 2 {
                cmd_err!("usage: load <path>");
            } else {
                let path = PathBuf::from(&parts[1]);
                if let Err(e) = load_plugin(&path) {
                    cmd_err!("error: {}", e);
                }
            }
        }
        "list" => {
            if parts.len() < 2 {
                cmd_err!("usage: list <midi|audio|plugins|presets>");
            } else {
                match parts[1].as_str() {
                    "presets" => {
                        if parts.len() < 3 {
                            cmd_err!("usage: list presets <plugin_id>");
                        } else if let Ok(pid) = parts[2].parse::<u32>() {
                            let mgr = manager().lock().unwrap();
                            match mgr.list_presets(pid) {
                                Ok(ps) => {
                                    if ps.is_empty() {
                                        println!("(no presets)");
                                    } else {
                                        for (i, name) in ps.iter().enumerate() {
                                            println!("  [{}] {}", i + 1, name);
                                        }
                                    }
                                }
                                Err(e) => cmd_err!("error: {}", e),
                            }
                        } else {
                            cmd_err!("invalid plugin id");
                        }
                    }
                    "plugins" => {
                        let mgr = manager().lock().unwrap();
                        let list = mgr.list();
                        drop(mgr);
                        if list.is_empty() {
                            println!("(no plugins loaded)");
                        } else {
                            for p in list {
                                println!("  #{} {} ({:?}) - {} params", p.id, p.name, p.format, p.num_params);
                            }
                        }
                    }
                    "midi" => match spaph_midi::list_input_ports() {
                        Ok(ps) => {
                            if ps.is_empty() {
                                println!("(no midi input ports)");
                            }
                            for (i, p) in ps.iter().enumerate() {
                                println!("  [{}] {}", i, p);
                            }
                        }
                        Err(e) => cmd_err!("error: {}", e),
                    },
                    "audio" => match spaph_audio::list_devices() {
                        Ok(devs) => {
                            for d in devs {
                                println!("{}", d);
                            }
                        }
                        Err(e) => cmd_err!("error: {}", e),
                    },
                    other => cmd_err!("unknown list target: {}", other),
                }
            }
        }
        "midi" => {
            if parts.len() >= 3 && parts[1] == "listen" {
                if IN_SCRIPT.load(Ordering::Relaxed) {
                    eprintln!("warning: 'midi listen' is interactive only; skipped in script");
                } else {
                    midi_listen(&parts[2]);
                }
            } else {
                cmd_err!("usage: midi listen <port>");
            }
        }
        "unload" => {
            if parts.len() < 2 {
                cmd_err!("usage: unload <id>");
            } else if let Ok(id) = parts[1].parse::<u32>() {
                let mut mgr = manager().lock().unwrap();
                match mgr.unload(id) {
                    Ok(_) => println!("unloaded {}", id),
                    Err(e) => cmd_err!("error: {}", e),
                }
            } else {
                cmd_err!("invalid id");
            }
        }
        "connect" => {
            if parts.len() < 2 {
                cmd_err!("usage: connect <audio|midi> ...");
            } else {
                match parts[1].as_str() {
                    "audio" => {
                        if parts.len() < 3 {
                            cmd_err!("usage: connect audio <plugin_id> [device_index] [sample_rate] [buffer_size]");
                        } else if let Ok(pid) = parts[2].parse::<u32>() {
                            let dev_idx = parts.get(3).and_then(|s| s.parse::<usize>().ok());
                            let sr = parts.get(4).and_then(|s| s.parse::<u32>().ok());
                            let bs = parts.get(5).and_then(|s| s.parse::<u32>().ok());
                            let mgr = manager().lock().unwrap();
                            let inst = mgr.get_instance(pid);
                            let q = mgr.get_midi_queue(pid);
                            let gain = mgr.get_gain_handle(pid);
                            drop(mgr);
                            let defaults = audio_defaults().lock().unwrap();
                            let dev_idx = dev_idx.or(defaults.device_index);
                            let sr = sr.or(defaults.sample_rate);
                            let bs = bs.or(defaults.buffer_size);
                            drop(defaults);
                            match (inst, q, gain) {
                                (Some(inst), Some(q), Some(gain)) => {
                                    match spaph_audio::start_audio(inst, q, dev_idx, sr, bs, gain) {
                                        Ok(handle) => {
                                            *AUDIO_HANDLE.lock().unwrap() = Some(handle);
                                            println!("audio started for plugin {}", pid);
                                            *ACTIVE_AUDIO_PLUGIN.lock().unwrap() = Some(pid);
                                        }
                                        Err(e) => cmd_err!("error: {}", e),
                                    }
                                }
                                _ => cmd_err!("plugin {} not found", pid),
                            }
                        } else {
                            cmd_err!("invalid plugin id");
                        }
                    }
                    "midi" => {
                        if parts.len() < 4 {
                            cmd_err!("usage: connect midi <port> <plugin_id>");
                        } else if let Ok(pid) = parts[3].parse::<u32>() {
                            midi_connect(parts[2].clone(), pid);
                        } else {
                            cmd_err!("invalid plugin id");
                        }
                    }
                    other => cmd_err!("unknown connect target: {}", other),
                }
            }
        }
        "disconnect" => {
            cleanup_connections();
            println!("disconnected");
        }
        "select" => {
            match parse_select_args(&parts) {
                None => cmd_err!("usage: select <plugin_id> <preset_id>  (or: select preset <plugin_id> <preset_id>)"),
                Some((pid, preset)) => {
                    if preset == 0 {
                        cmd_err!("preset id must be >= 1");
                    } else {
                        let mut mgr = manager().lock().unwrap();
                        match mgr.select_preset(pid, (preset - 1) as usize) {
                            Ok(_) => {
                                let name = mgr.list_presets(pid).ok().and_then(|ps| ps.get((preset - 1) as usize).cloned()).unwrap_or_default();
                                println!("selected preset {} for plugin {}: {}", preset, pid, name);
                            }
                            Err(e) => cmd_err!("error: {}", e),
                        }
                    }
                }
            }
        }
        "volume" => {
            if parts.len() == 2 {
                // volume <level> -> active plugin
                if let Ok(level) = parts[1].parse::<f32>() {
                    let active = *ACTIVE_AUDIO_PLUGIN.lock().unwrap();
                    match active {
                        Some(pid) => {
                            let mgr = manager().lock().unwrap();
                            match mgr.set_gain(pid, level) {
                                Ok(_) => println!("volume set to {} for plugin {}", level, pid),
                                Err(e) => cmd_err!("error: {}", e),
                            }
                        }
                        None => cmd_err!("no active audio plugin; use: volume <plugin_id> <level>"),
                    }
                } else {
                    cmd_err!("usage: volume [<plugin_id>] <level>");
                }
            } else if parts.len() == 3 {
                if let (Ok(pid), Ok(level)) = (parts[1].parse::<u32>(), parts[2].parse::<f32>()) {
                    let mgr = manager().lock().unwrap();
                    match mgr.set_gain(pid, level) {
                        Ok(_) => println!("volume set to {} for plugin {}", level, pid),
                        Err(e) => cmd_err!("error: {}", e),
                    }
                } else {
                    cmd_err!("usage: volume [<plugin_id>] <level>");
                }
            } else {
                cmd_err!("usage: volume [<plugin_id>] <level>");
            }
        }
        "param" => {
            if parts.len() < 2 {
                cmd_err!("usage: param <list|get|set> ...");
            } else {
                match parts[1].as_str() {
                    "list" => {
                        if parts.len() < 3 {
                            cmd_err!("usage: param list <plugin_id>");
                        } else if let Ok(pid) = parts[2].parse::<u32>() {
                            let mgr = manager().lock().unwrap();
                            let inst = mgr.get_instance(pid);
                            drop(mgr);
                            if let Some(inst) = inst {
                                let inst = inst.lock().unwrap();
                                let info = inst.info();
                                for p in &info.params {
                                    let val = inst.get_param(p.index);
                                    println!("  [{}] {} = {:.4} (range {}..{})", p.index, p.name, val, p.min, p.max);
                                }
                            } else {
                                cmd_err!("plugin {} not found", pid);
                            }
                        } else {
                            cmd_err!("invalid plugin id");
                        }
                    }
                    "get" => {
                        if parts.len() < 4 {
                            cmd_err!("usage: param get <plugin_id> <index>");
                        } else if let (Ok(pid), Ok(idx)) = (parts[2].parse::<u32>(), parts[3].parse::<u32>()) {
                            let mgr = manager().lock().unwrap();
                            let inst = mgr.get_instance(pid);
                            drop(mgr);
                            if let Some(inst) = inst {
                                let inst = inst.lock().unwrap();
                                let val = inst.get_param(idx);
                                println!("{}", val);
                            } else {
                                cmd_err!("plugin {} not found", pid);
                            }
                        } else {
                            cmd_err!("invalid ids");
                        }
                    }
                    "set" => {
                        if parts.len() < 5 {
                            cmd_err!("usage: param set <plugin_id> <index|volume> <value>");
                        } else if let (Ok(pid), Ok(val)) = (parts[2].parse::<u32>(), parts[4].parse::<f32>()) {
                            if parts[3] == "volume" {
                                let mgr = manager().lock().unwrap();
                                match mgr.set_gain(pid, val) {
                                    Ok(_) => println!("volume set to {} for plugin {}", val, pid),
                                    Err(e) => cmd_err!("error: {}", e),
                                }
                            } else if let Ok(idx) = parts[3].parse::<u32>() {
                                let mgr = manager().lock().unwrap();
                                let inst = mgr.get_instance(pid);
                                drop(mgr);
                                if let Some(inst) = inst {
                                    let mut inst = inst.lock().unwrap();
                                    inst.set_param(idx, val as f64);
                                    println!("set param {} = {} for plugin {}", idx, val, pid);
                                } else {
                                    cmd_err!("plugin {} not found", pid);
                                }
                            } else {
                                cmd_err!("invalid param index");
                            }
                        } else {
                            cmd_err!("invalid args");
                        }
                    }
                    other => cmd_err!("unknown param action: {}", other),
                }
            }
        }
        "preset" => {
            if parts.len() < 2 {
                cmd_err!("usage: preset <scan|load|save> ...");
            } else {
                match parts[1].as_str() {
                    "save" => {
                        if parts.len() < 4 {
                            cmd_err!("usage: preset save <plugin_id> <file>");
                        } else if let Ok(pid) = parts[2].parse::<u32>() {
                            let file = PathBuf::from(&parts[3]);
                            let mgr = manager().lock().unwrap();
                            let inst = mgr.get_instance(pid);
                            drop(mgr);
                            if let Some(inst) = inst {
                                let inst = inst.lock().unwrap();
                                match inst.save_preset_file(&file) {
                                    Ok(_) => println!("saved preset {}", file.display()),
                                    Err(e) => cmd_err!("error: {}", e),
                                }
                            } else {
                                cmd_err!("plugin {} not found", pid);
                            }
                        } else {
                            cmd_err!("invalid plugin id");
                        }
                    }
                    "scan" => {
                        if parts.len() < 3 {
                            cmd_err!("usage: preset scan <dir>");
                        } else {
                            let dir = PathBuf::from(&parts[2]);
                            scan_presets(&dir);
                        }
                    }
                    "load" => {
                        if parts.len() < 4 {
                            cmd_err!("usage: preset load <plugin_id> <file>");
                        } else if let Ok(pid) = parts[2].parse::<u32>() {
                            let file = PathBuf::from(&parts[3]);
                            let mgr = manager().lock().unwrap();
                            let inst = mgr.get_instance(pid);
                            drop(mgr);
                            if let Some(inst) = inst {
                                let mut inst = inst.lock().unwrap();
                                match inst.load_preset_file(&file) {
                                    Ok(_) => println!("loaded preset {}", file.display()),
                                    Err(e) => cmd_err!("error: {}", e),
                                }
                            } else {
                                cmd_err!("plugin {} not found", pid);
                            }
                        } else {
                            cmd_err!("invalid plugin id");
                        }
                    }
                    other => cmd_err!("unknown preset action: {}", other),
                }
            }
        }
        "show" => {
            if parts.len() < 2 {
                cmd_err!("usage: show <plugin_id>");
            } else if let Ok(pid) = parts[1].parse::<u32>() {
                let mgr = manager().lock().unwrap();
                let inst = mgr.get_instance(pid);
                drop(mgr);
                if let Some(inst) = inst {
                    let mut inst = inst.lock().unwrap();
                    if !inst.has_editor() {
                        cmd_err!("plugin {} has no editor", pid);
                    } else {
                        println!("opening editor for plugin {} (close window to continue)", pid);
                        if let Err(e) = inst.open_editor() {
                            cmd_err!("error: {}", e);
                        }
                    }
                } else {
                    cmd_err!("plugin {} not found", pid);
                }
            } else {
                cmd_err!("invalid plugin id");
            }
        }
        "quit" | "exit" => {
            // In a script, quit only ends the script; the program keeps running.
            return false;
        }
        other => cmd_err!("unknown command: {}", other),
    }
    true
}

fn run_shell(osc_port: u16) {
    let mut rl = match DefaultEditor::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("readline init failed: {}", e);
            let mut line = String::new();
            loop {
                print!("spaph> ");
                use std::io::Write;
                std::io::stdout().flush().ok();
                line.clear();
                if std::io::stdin().read_line(&mut line).is_err() {
                    break;
                }
                if !run_line(line.trim()) {
                    break;
                }
            }
            return;
        }
    };
    // Start OSC server
    let bind = format!("0.0.0.0:{}", osc_port);
    let stop = osc_stop();
    stop.store(false, std::sync::atomic::Ordering::Relaxed);
    let handle = std::thread::spawn(move || {
        if let Err(e) = spaph_osc::run_server(&bind, |cmd| { let _ = run_line(&cmd); }, stop.clone()) {
            eprintln!("OSC server error: {}", e);
        }
    });
    *osc_thread_slot().lock().unwrap() = Some(handle);
    eprintln!("OSC listening on 0.0.0.0:{}", osc_port);

    loop {
        match rl.readline("spaph> ") {
            Ok(line) => {
                let _ = rl.add_history_entry(line.as_str());
                if !run_line(line.trim()) {
                    break;
                }
            }
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => break,
            Err(e) => {
                eprintln!("readline error: {}", e);
                break;
            }
        }
    }
}

static MANAGER: std::sync::OnceLock<std::sync::Mutex<PluginManager>> = std::sync::OnceLock::new();

fn manager() -> &'static std::sync::Mutex<PluginManager> {
    MANAGER.get_or_init(|| std::sync::Mutex::new(PluginManager::new()))
}

static AUDIO_HANDLE: std::sync::Mutex<Option<spaph_audio::AudioHandle>> = std::sync::Mutex::new(None);

static MIDI_THREAD_HANDLES: std::sync::OnceLock<std::sync::Mutex<Vec<std::thread::JoinHandle<()>>>> =
    std::sync::OnceLock::new();

fn midi_thread_handles() -> &'static std::sync::Mutex<Vec<std::thread::JoinHandle<()>>> {
    MIDI_THREAD_HANDLES.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Load the configuration: `--config` if given (must exist), else `./spaph.json` if present.
fn load_startup_config(cli: &Cli) -> (Option<PathBuf>, Config) {
    let path = match &cli.config {
        Some(p) => Some(p.clone()),
        None => std::env::current_dir()
            .ok()
            .and_then(|d| spaph_config::find_default_config(&d)),
    };
    let cfg = match &path {
        Some(p) => spaph_config::load_config(p).unwrap_or_else(|e| {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }),
        None => Config::default(),
    };
    (path, cfg)
}

fn init_logging(level: &str) {
    let filter_str = if level == "none" {
        "off".to_string()
    } else {
        format!("{},pulseaudio=off,vst3_host=error,rustyline=off", level)
    };
    let filter = tracing_subscriber::EnvFilter::try_new(&filter_str)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Run a script line by line. Stops at the first failing command (returns its location),
/// and `quit` ends the script without ending the program.
fn run_script(path: &Path) -> std::result::Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read script {}: {}", path.display(), e))?;
    IN_SCRIPT.store(true, Ordering::Relaxed);
    let mut result = Ok(());
    for ScriptLine { line, command } in spaph_config::parse_script(&text) {
        CMD_ERROR.store(false, Ordering::Relaxed);
        let keep_going = run_line(&command);
        if CMD_ERROR.load(Ordering::Relaxed) {
            result = Err(format!(
                "script {}, line {}: '{}' failed; stopping",
                path.display(),
                line,
                command
            ));
            break;
        }
        if !keep_going {
            break;
        }
    }
    IN_SCRIPT.store(false, Ordering::Relaxed);
    result
}

fn main() {
    let cli = Cli::parse();
    let (config_path, mut cfg) = load_startup_config(&cli);

    let overrides = Overrides {
        sample_rate: cli.sample_rate,
        buffer_size: cli.buffer_size,
        device_index: cli.audio_device,
        log_level: cli.log_level.map(|l| l.name().to_string()),
        osc_port: cli.osc_port,
    };
    if let Err(e) = spaph_config::apply_overrides(&mut cfg, &overrides) {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }

    init_logging(cfg.log_level.as_deref().unwrap_or("none"));
    tracing::debug!("config loaded: {:?}", config_path);
    {
        let mut d = audio_defaults().lock().unwrap();
        d.sample_rate = cfg.audio.sample_rate;
        d.buffer_size = cfg.audio.buffer_size;
        d.device_index = cfg.audio.device_index;
    }

    let script = spaph_config::resolve_script(
        &cfg,
        config_path.as_deref().and_then(|p| p.parent()),
    );
    if let Some(script) = script {
        if let Err(msg) = run_script(&script) {
            eprintln!("error: {}", msg);
            shutdown(1);
        }
    }

    run_shell(cfg.osc.port.unwrap_or(9000));
    shutdown(0);
}

#[cfg(test)]
mod tests {
    use super::{parse_select_args, split_args};

    #[test]
    fn split_simple() {
        assert_eq!(split_args("help"), vec!["help".to_string()]);
    }

    #[test]
    fn split_single_quoted_path_with_spaces() {
        assert_eq!(
            split_args("load '/usr/lib/vst3/Surge XT.vst3'"),
            vec!["load".to_string(), "/usr/lib/vst3/Surge XT.vst3".to_string()]
        );
    }

    #[test]
    fn split_apostrophe_inside_double_quotes() {
        assert_eq!(
            split_args("load \"it's here\""),
            vec!["load".to_string(), "it's here".to_string()]
        );
    }

    #[test]
    fn split_double_quote_inside_single_quotes() {
        assert_eq!(
            split_args("preset load 1 '/tmp/say \"hi\".fxp'"),
            vec!["preset".to_string(), "load".to_string(), "1".to_string(), "/tmp/say \"hi\".fxp".to_string()]
        );
    }

    #[test]
    fn split_quoted_path_with_spaces() {
        assert_eq!(
            split_args("load \"/usr/lib/vst3/Surge XT.vst3\""),
            vec!["load".to_string(), "/usr/lib/vst3/Surge XT.vst3".to_string()]
        );
    }

    #[test]
    fn parse_select_simple() {
        let parts = split_args("select 1 5");
        assert_eq!(parse_select_args(&parts), Some((1, 5)));
    }

    #[test]
    fn parse_select_preset_keyword() {
        let parts = split_args("select preset 1 5");
        assert_eq!(parse_select_args(&parts), Some((1, 5)));
    }

    #[test]
    fn parse_select_invalid() {
        let parts = split_args("select foo bar");
        assert_eq!(parse_select_args(&parts), None);
    }
}
