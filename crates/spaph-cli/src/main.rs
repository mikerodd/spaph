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

    /// Audio backend override (e.g. wasapi, asio, alsa, jack)
    #[arg(long)]
    audio_backend: Option<String>,

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
/// True while a command received over OSC is being executed (non-interactive, like a script).
static IN_OSC: AtomicBool = AtomicBool::new(false);
/// Set by `cmd_err!` when a command fails; checked after each script line.
static CMD_ERROR: AtomicBool = AtomicBool::new(false);
/// Directory of the config file; relative `load` paths are resolved from it.
static CONFIG_DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
/// Current `load` nesting depth.
static LOAD_DEPTH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Maximum `load` nesting depth; deeper loads are refused with an error.
const MAX_LOAD_DEPTH: u32 = 8;

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

/// Resolve a plugin argument (numeric id or alias) to an id. Takes the manager lock, so it must
/// not be called while the lock is already held.
fn resolve_plugin_arg(token: &str) -> Option<u32> {
    manager().lock().unwrap().resolve(token)
}

/// Pick a device from `names`: a numeric token is an index from `audio list`; any other token
/// must be part of exactly one device name (case-insensitive).
fn match_device(token: &str, names: &[String]) -> std::result::Result<usize, String> {
    if let Ok(i) = token.parse::<usize>() {
        return Ok(i);
    }
    let needle = token.to_lowercase();
    let hits: Vec<usize> = names
        .iter()
        .enumerate()
        .filter(|(_, n)| n.to_lowercase().contains(&needle))
        .map(|(i, _)| i)
        .collect();
    match hits.as_slice() {
        [i] => Ok(*i),
        [] => Err(format!("no audio device matches '{}'", token)),
        _ => {
            let list: Vec<String> = hits.iter().map(|&i| format!("[{}] {}", i, names[i])).collect();
            Err(format!("audio device '{}' matches more than one device: {}", token, list.join(", ")))
        }
    }
}

fn resolve_device_arg(token: &str) -> std::result::Result<usize, String> {
    if let Ok(i) = token.parse::<usize>() {
        return Ok(i);
    }
    let names = spaph_audio::list_devices().map_err(|e| e.to_string())?;
    match_device(token, &names)
}

fn load_plugin(path: &PathBuf, alias: Option<&str>) -> Result<()> {
    tracing::info!("load: calling plugin loader for {}", path.display());
    let id = manager().lock().unwrap().load_as(path, alias)?;
    tracing::info!("load: plugin registered as #{}", id);
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
    println!("  plugin load <path> [as <name>] Load a plugin and display its interface; optional alias");
    println!("  plugin list                   List loaded plugins");
    println!("  plugin unload <id|all>        Unload a plugin by id or alias, or all plugins (also disconnects MIDI and audio)");
    println!("  plugin show <id>              Open plugin editor window (VST2 and VST3); prompt stays usable");
    println!("  plugin hide <id>              Close plugin editor window");
    println!("  plugin connect <src> <dst>    Feed plugin <src>'s output into plugin <dst>'s audio input");
    println!("  plugin disconnect <dst>       Remove the audio input link of plugin <dst>");
    println!("  plugin preset list <id>       List presets of a plugin");
    println!("  plugin preset select <id> <n> Select a preset (1-based)");
    println!("  plugin preset scan <dir>      Scan a directory for preset files");
    println!("  plugin preset load <id> <file> Load a preset file into a plugin");
    println!("  plugin preset save <id> <file> Save current state as preset file");
    println!("  plugin param list <id>         List plugin parameters");
    println!("  plugin param get <id> <idx>   Get a parameter value");
    println!("  plugin param set <id> <idx|volume> <value>  Set a parameter or volume");
    println!("  midi list                     List MIDI input ports");
    println!("  midi listen <port> [ch]       Capture MIDI input, optional channel filter 1..=16");
    println!("  midi connect <port> <id> [ch] Connect a MIDI input to a plugin, optional channel filter");
    println!("  midi disconnect               Stop all MIDI connections");
    println!("  midi stats [reset]            Show or reset MIDI event counters");
    println!("  midi disconnect <port> <id> [ch]  Disconnect one MIDI connection");
    println!("  audio list                    List audio output devices");
    println!("  backend list                  List available audio backends");
    println!("  audio connect <id> [dev]      Connect plugin output to an audio device");
    println!("  audio disconnect [id] [dev]   Disconnect audio (one or all)");
    println!("  volume [<id>] <level>         Set host-side output gain (per plugin)");
    println!("  master <level>                Set master output gain");
    println!("  disconnect                    Stop all audio and MIDI connections");
    println!("  load <script> [name=value...] Run a script file (relative to the config file folder), passing its parameters");
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

fn midi_listen(port: &str, channel: Option<u8>) {
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
                Ok(ev) => {
                    if midi_matches(&ev.data, channel) {
                        println!("{}", midi_name(&ev));
                    }
                }
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

fn midi_channel_of(msg: &[u8]) -> Option<u8> {
    if msg.is_empty() {
        return None;
    }
    let status = msg[0];
    if (0x80..=0xEF).contains(&status) {
        Some((status & 0x0F) + 1)
    } else {
        None
    }
}

fn midi_matches(msg: &[u8], filter: Option<u8>) -> bool {
    match (filter, midi_channel_of(msg)) {
        (Some(f), Some(ch)) => f == ch,
        _ => true,
    }
}

fn parse_midi_channel(s: &str) -> std::result::Result<u8, String> {
    match s.parse::<u8>() {
        Ok(c) if (1..=16).contains(&c) => Ok(c),
        _ => Err(format!("midi channel must be 1..=16, got '{}'", s)),
    }
}

fn midi_connect(port: String, plugin_id: u32, channel: Option<u8>) {
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
    let port_name = if let Ok(idx) = port.parse::<usize>() {
        spaph_midi::list_input_ports().ok().and_then(|ps| ps.get(idx).cloned()).unwrap_or_else(|| port.clone())
    } else {
        port.clone()
    };
    // reject duplicate connection (same port + same channel filter) with a warning
    {
        let mut mgr = manager().lock().unwrap();
        if let Err(e) = mgr.add_midi_connection(plugin_id, port_name.clone(), channel) {
            eprintln!("warning: {}", e);
            return;
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let port_clone = port.clone();
    let port_name_for_thread = port_name.clone();
    let stop_for_conn = stop.clone();
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
                manager().lock().unwrap().remove_midi_connection(plugin_id, &port_name_for_thread, channel);
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
                    spaph_audio::midi_stats::on_received();
                    if midi_matches(&ev.data, channel) {
                        spaph_audio::midi_stats::on_queued();
                        let mut q = q.lock().unwrap();
                        q.push(ev);
                    } else {
                        spaph_audio::midi_stats::on_filtered();
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(_) => {}
            }
        }
    });
    midi_connections().lock().unwrap().push(MidiConn {
        port: port_name.clone(),
        plugin_id,
        channel,
        stop: stop_for_conn,
        handle,
    });
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

struct MidiConn {
    port: String,
    plugin_id: u32,
    channel: Option<u8>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

static MIDI_CONNECTIONS: std::sync::OnceLock<std::sync::Mutex<Vec<MidiConn>>> =
    std::sync::OnceLock::new();

fn midi_connections() -> &'static std::sync::Mutex<Vec<MidiConn>> {
    MIDI_CONNECTIONS.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

fn resolve_midi_port_name(port: &str) -> String {
    if let Ok(idx) = port.parse::<usize>() {
        spaph_midi::list_input_ports()
            .ok()
            .and_then(|ps| ps.get(idx).cloned())
            .unwrap_or_else(|| port.to_string())
    } else {
        port.to_string()
    }
}

fn disconnect_all_midi() {
    let conns: Vec<MidiConn> = midi_connections().lock().unwrap().drain(..).collect();
    for c in conns {
        c.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = c.handle.join();
    }
    manager().lock().unwrap().clear_all_midi_connections();
}

fn disconnect_midi_filtered(
    port: Option<&str>,
    plugin_id: Option<u32>,
    channel: Option<u8>,
) -> std::result::Result<(), String> {
    let port_name = port.map(resolve_midi_port_name);
    let mut conns = midi_connections().lock().unwrap();
    let mut to_remove = Vec::new();
    for (i, c) in conns.iter().enumerate() {
        let port_ok = port_name.as_ref().map_or(true, |p| c.port == *p);
        let plugin_ok = plugin_id.map_or(true, |id| c.plugin_id == id);
        let channel_ok = channel.map_or(true, |ch| c.channel == Some(ch));
        if port_ok && plugin_ok && channel_ok {
            to_remove.push(i);
        }
    }
    if to_remove.is_empty() {
        return Err("no matching MIDI connection".to_string());
    }
    let mut removed = Vec::new();
    for &i in to_remove.iter().rev() {
        let c = conns.remove(i);
        c.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = c.handle.join();
        removed.push((c.plugin_id, c.port, c.channel));
    }
    drop(conns);
    let mut mgr = manager().lock().unwrap();
    for (pid, port, ch) in removed {
        mgr.remove_midi_connection(pid, &port, ch);
    }
    Ok(())
}

/// Unload one plugin: close its editor, disconnect its audio and MIDI, then drop it.
/// The manager lock must not be held by the caller, since the MIDI teardown takes it.
fn unload_plugin(id: u32) -> std::result::Result<(), String> {
    // The editor window uses the plugin, so close it first.
    close_editor_window(id);
    // MIDI threads keep feeding the plugin's queue until stopped; "no matching connection"
    // just means this plugin had no MIDI input.
    let _ = disconnect_midi_filtered(None, Some(id), None);
    {
        let mut mgr = manager().lock().unwrap();
        mgr.clear_audio_device(id);
        // Unloading also removes links where this plugin was a source.
        mgr.unload(id).map_err(|e| e.to_string())?;
    }
    spaph_audio::detach(id);
    sync_links();
    Ok(())
}

/// Push the current plugin links to the audio graph.
fn sync_links() {
    let links = manager().lock().unwrap().links();
    spaph_audio::set_links(&links);
}

/// Add plugin `id` to the audio graph without mixing it to the output (it only feeds others).
fn attach_to_graph(id: u32) {
    let handles = {
        let mgr = manager().lock().unwrap();
        (mgr.get_instance(id), mgr.get_gain_handle(id), mgr.get_midi_queue(id))
    };
    if let (Some(inst), Some(gain), Some(queue)) = handles {
        let _ = spaph_audio::attach(id, inst, gain, queue, false);
    }
}

/// `plugin connect <src> <dst>`: validate in the manager, then add both ends to the graph.
fn connect_plugins(src: u32, dst: u32) -> std::result::Result<(), String> {
    manager().lock().unwrap().connect(src, dst).map_err(|e| e.to_string())?;
    attach_to_graph(src);
    attach_to_graph(dst);
    sync_links();
    Ok(())
}

/// `plugin disconnect <dst>`: remove the input link of `dst`.
fn disconnect_plugin(dst: u32) -> std::result::Result<(), String> {
    manager().lock().unwrap().disconnect(dst).map_err(|e| e.to_string())?;
    sync_links();
    Ok(())
}

/// `plugin unload all`: unload every plugin, reporting each failure.
fn unload_all_plugins() {
    let ids: Vec<u32> = manager().lock().unwrap().list().iter().map(|p| p.id).collect();
    if ids.is_empty() {
        println!("no plugins to unload");
        return;
    }
    let mut unloaded = 0;
    for id in ids {
        match unload_plugin(id) {
            Ok(()) => unloaded += 1,
            Err(e) => cmd_err!("error: plugin {}: {}", id, e),
        }
    }
    println!("unloaded {} plugin(s)", unloaded);
}

fn disconnect_all_audio() {
    let armed = spaph_audio::active_ids();
    // Stop mixing to the device; plugins that are part of a link keep processing.
    spaph_audio::unmix_all();
    for id in &armed {
        manager().lock().unwrap().clear_audio_device(*id);
    }
}

fn disconnect_audio(plugin_id: Option<u32>, dev: Option<&str>) -> std::result::Result<(), String> {
    let armed = spaph_audio::active_ids();
    match plugin_id {
        Some(id) => {
            if !armed.contains(&id) {
                return Err(format!("plugin {} is not connected to audio", id));
            }
            if let Some(dev) = dev {
                let name = manager().lock().unwrap().audio_device_name(id).unwrap_or_default();
                if dev != name {
                    return Err(format!("plugin {} is not connected to audio device '{}'", id, dev));
                }
            }
            let _ = spaph_audio::disarm(id);
            manager().lock().unwrap().clear_audio_device(id);
            Ok(())
        }
        None => {
            disconnect_all_audio();
            Ok(())
        }
    }
}

static STDIN_STOP: std::sync::OnceLock<std::sync::Arc<std::sync::atomic::AtomicBool>> =
    std::sync::OnceLock::new();
static STDIN_THREAD: std::sync::OnceLock<std::sync::Mutex<Option<std::thread::JoinHandle<()>>>> =
    std::sync::OnceLock::new();

fn stdin_stop() -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    STDIN_STOP.get_or_init(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))).clone()
}
fn stdin_thread_slot() -> &'static std::sync::Mutex<Option<std::thread::JoinHandle<()>>> {
    STDIN_THREAD.get_or_init(|| std::sync::Mutex::new(None))
}

// Saved terminal state, captured before rustyline puts the terminal in raw mode.
#[cfg(unix)]
static SAVED_TERM: std::sync::OnceLock<libc::termios> = std::sync::OnceLock::new();
#[cfg(windows)]
static SAVED_CONSOLE_MODE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

fn save_terminal() {
    #[cfg(unix)]
    unsafe {
        if libc::isatty(0) != 0 {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) == 0 {
                let _ = SAVED_TERM.set(t);
            }
        }
    }
    #[cfg(windows)]
    unsafe {
        use winapi::um::consoleapi::GetConsoleMode;
        use winapi::um::processenv::GetStdHandle;
        use winapi::um::winbase::STD_INPUT_HANDLE;
        let handle = GetStdHandle(STD_INPUT_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(handle, &mut mode) != 0 {
            let _ = SAVED_CONSOLE_MODE.set(mode);
        }
    }
}

fn close_stdin() {
    #[cfg(unix)]
    unsafe {
        libc::close(0);
    }
    #[cfg(windows)]
    unsafe {
        use winapi::um::processenv::GetStdHandle;
        use winapi::um::winbase::STD_INPUT_HANDLE;
        use winapi::um::handleapi::CloseHandle;
        let handle = GetStdHandle(STD_INPUT_HANDLE);
        CloseHandle(handle);
    }
}

fn restore_terminal() {
    #[cfg(unix)]
    unsafe {
        if libc::isatty(0) != 0 {
            if let Some(t) = SAVED_TERM.get() {
                libc::tcsetattr(0, libc::TCSANOW, t);
            } else {
                let _ = std::process::Command::new("stty").arg("sane").status();
            }
        }
    }
    #[cfg(windows)]
    unsafe {
        use winapi::um::consoleapi::SetConsoleMode;
        use winapi::um::processenv::GetStdHandle;
        use winapi::um::winbase::STD_INPUT_HANDLE;
        let handle = GetStdHandle(STD_INPUT_HANDLE);
        if let Some(mode) = SAVED_CONSOLE_MODE.get() {
            SetConsoleMode(handle, *mode);
        }
    }
}

/// Interval between console-loop passes that service open editor windows (about 60 Hz).
const EDITOR_FRAME: std::time::Duration = std::time::Duration::from_millis(16);

/// The thread that owns the console. Plugins are loaded here, and their editor windows must be
/// created and serviced here too (VST3 in particular is bound to the thread that loaded it).
static CONSOLE_THREAD: std::sync::OnceLock<std::thread::ThreadId> = std::sync::OnceLock::new();

fn on_console_thread() -> bool {
    CONSOLE_THREAD.get().map_or(false, |id| *id == std::thread::current().id())
}

/// An open editor window. Only ever touched on the console thread.
struct OpenEditor(Box<dyn spaph_plugin::EditorSession>);

// SAFETY: sessions are created, ticked and closed only on the console thread (enforced by
// `open_editor_window`); the mutex is only needed to reach the registry from a static.
unsafe impl Send for OpenEditor {}

static EDITORS: std::sync::OnceLock<Mutex<std::collections::HashMap<u32, OpenEditor>>> =
    std::sync::OnceLock::new();

fn editors() -> &'static Mutex<std::collections::HashMap<u32, OpenEditor>> {
    EDITORS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Open the editor window of plugin `pid`. Returns at once; the window is serviced by
/// `pump_editors` from the console loop, so the prompt, MIDI and audio keep running.
fn open_editor_window(pid: u32) -> std::result::Result<(), String> {
    if !on_console_thread() {
        return Err("plugin show/hide must be used from the console or a script".into());
    }
    if editors().lock().unwrap().contains_key(&pid) {
        return Err(format!("editor for plugin {} is already open", pid));
    }
    let inst = manager()
        .lock()
        .unwrap()
        .get_instance(pid)
        .ok_or_else(|| format!("plugin {} not found", pid))?;
    // The instance lock is held only while the window is created, not while it is open.
    let session = inst.lock().unwrap().open_editor_session().map_err(|e| e.to_string())?;
    editors().lock().unwrap().insert(pid, OpenEditor(session));
    Ok(())
}

/// Service every open editor window once. Windows the user has closed are released.
fn pump_editors() {
    let mut map = editors().lock().unwrap();
    let closed: Vec<u32> = map
        .iter_mut()
        .filter_map(|(id, e)| if e.0.tick() { None } else { Some(*id) })
        .collect();
    for id in closed {
        if let Some(e) = map.remove(&id) {
            e.0.close();
        }
    }
}

/// Close the editor window of plugin `pid`. Returns `false` if no window was open.
fn close_editor_window(pid: u32) -> bool {
    let entry = editors().lock().unwrap().remove(&pid);
    match entry {
        Some(e) => {
            e.0.close();
            true
        }
        None => false,
    }
}

/// Close every editor window (shutdown and disconnect). Only the console thread may do this;
/// from any other thread the windows are left open and are closed by the console later.
fn close_all_editor_windows() {
    if !on_console_thread() {
        return;
    }
    let all: Vec<OpenEditor> = editors().lock().unwrap().drain().map(|(_, e)| e).collect();
    for e in all {
        e.0.close();
    }
}

fn cleanup_connections() {
    tracing::debug!("shutdown: closing editor windows");
    close_all_editor_windows();
    tracing::debug!("shutdown: stopping audio");
    disconnect_all_audio();
    tracing::debug!("shutdown: stopping MIDI");
    disconnect_all_midi();
    tracing::debug!("shutdown: stopping OSC server");
    osc_stop().store(true, std::sync::atomic::Ordering::Relaxed);
    if let Some(h) = osc_thread_slot().lock().unwrap().take() {
        let _ = h.join();
    }
    stdin_stop().store(true, std::sync::atomic::Ordering::Relaxed);
    close_stdin();
    if let Some(h) = stdin_thread_slot().lock().unwrap().take() {
        let _ = h.join();
    }
    restore_terminal();
}

/// Seconds allowed for the whole shutdown (stop audio/MIDI/OSC, then unload plugins).
const SHUTDOWN_TIMEOUT_SECS: u64 = 6;

/// Stop everything and unload all plugins, then exit with `code`.
///
/// Runs on a helper thread with a timeout, so a plugin whose teardown hangs cannot freeze
/// the program: after the timeout a warning is printed and the process exits anyway.
fn shutdown(code: i32) -> ! {
    // Editor windows must be closed on the console thread that created them, so do it here
    // rather than inside the timed helper thread below.
    close_all_editor_windows();
    let finished = spaph_core::run_with_timeout(
        || {
            cleanup_connections();
            // Full stop: also drops plugins that were kept only for their links.
            spaph_audio::disarm_all();
            // Stop the output stream before the process exits. With ASIO the driver keeps its
            // callback running until the stream is dropped, which can block at exit.
            tracing::debug!("shutdown: stopping output stream");
            spaph_audio::stop_stream();
            tracing::debug!("shutdown: output stream stopped");
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
        "plugin" => {
            if parts.len() < 2 {
                cmd_err!("usage: plugin <load|list|unload|show|hide|connect|disconnect|preset|param|volume> ...");
            } else {
                match parts[1].as_str() {
                    "load" => {
                        // plugin load <path> [as <name>]
                        let alias = match parts.len() {
                            3 => Some(None),
                            5 if parts[3] == "as" => Some(Some(parts[4].as_str())),
                            _ => None,
                        };
                        match alias {
                            None => cmd_err!("usage: plugin load <path> [as <name>]"),
                            Some(alias) => {
                                let path = PathBuf::from(&parts[2]);
                                if let Err(e) = load_plugin(&path, alias) {
                                    cmd_err!("error: {}", e);
                                }
                            }
                        }
                    }
                    "list" => {
                        let mgr = manager().lock().unwrap();
                        let list = mgr.list();
                        // snapshot midi connections and audio armed status
                        let midi_info: Vec<(u32, Vec<(String, Option<u8>)>)> = list.iter().map(|p| (p.id, mgr.midi_connections(p.id))).collect();
                        let audio_armed = spaph_audio::active_ids();
                        drop(mgr);
                        if list.is_empty() {
                            println!("(no plugins loaded)");
                        } else {
                            for p in list {
                                let midi = midi_info.iter().find(|(id, _)| *id == p.id).map(|(_, c)| c.clone()).unwrap_or_default();
                                let midi_str = if midi.is_empty() {
                                    "none".to_string()
                                } else {
                                    midi.iter().map(|(port, ch)| match ch {
                                        Some(c) => format!("{} (ch{})", port, c),
                                        None => format!("{} (all)", port),
                                    }).collect::<Vec<_>>().join(", ")
                                };
                                let audio_str = if audio_armed.contains(&p.id) {
                                    manager().lock().unwrap().audio_device_name(p.id).unwrap_or_else(|| "yes".to_string())
                                } else {
                                    "no".to_string()
                                };
                                let alias_str = p.alias.as_ref().map(|a| format!(" as {}", a)).unwrap_or_default();
                                let input_str = p.input_from.map(|s| format!(" | input: #{}", s)).unwrap_or_default();
                                println!("  #{}{} {} ({:?}) - {} params{} | midi: {} | audio: {}", p.id, alias_str, p.name, p.format, p.num_params, input_str, midi_str, audio_str);
                            }
                        }
                    }
                    "connect" => {
                        // plugin connect <source> <target>
                        if parts.len() != 4 {
                            cmd_err!("usage: plugin connect <source> <target>");
                        } else {
                            match (resolve_plugin_arg(&parts[2]), resolve_plugin_arg(&parts[3])) {
                                (Some(src), Some(dst)) => match connect_plugins(src, dst) {
                                    Ok(()) => println!("plugin {} feeds plugin {}", src, dst),
                                    Err(e) => cmd_err!("error: {}", e),
                                },
                                _ => cmd_err!("invalid plugin id or alias"),
                            }
                        }
                    }
                    "disconnect" => {
                        // plugin disconnect <target>
                        if parts.len() != 3 {
                            cmd_err!("usage: plugin disconnect <target>");
                        } else if let Some(dst) = resolve_plugin_arg(&parts[2]) {
                            match disconnect_plugin(dst) {
                                Ok(()) => println!("plugin {} input disconnected", dst),
                                Err(e) => cmd_err!("error: {}", e),
                            }
                        } else {
                            cmd_err!("invalid plugin id or alias");
                        }
                    }
                    "unload" => {
                        if parts.len() < 3 {
                            cmd_err!("usage: plugin unload <plugin_id|all>");
                        } else if parts[2] == "all" {
                            unload_all_plugins();
                        } else if let Some(id) = resolve_plugin_arg(&parts[2]) {
                            match unload_plugin(id) {
                                Ok(()) => println!("unloaded {}", id),
                                Err(e) => cmd_err!("error: {}", e),
                            }
                        } else {
                            cmd_err!("invalid plugin id or alias");
                        }
                    }
                    "show" => {
                        if parts.len() < 3 {
                            cmd_err!("usage: plugin show <plugin_id>");
                        } else if let Some(pid) = resolve_plugin_arg(&parts[2]) {
                            match open_editor_window(pid) {
                                Ok(()) => println!("editor opened for plugin {} (plugin hide {} closes it)", pid, pid),
                                Err(e) => cmd_err!("error: {}", e),
                            }
                        } else {
                            cmd_err!("invalid plugin id or alias");
                        }
                    }
                    "hide" => {
                        if parts.len() < 3 {
                            cmd_err!("usage: plugin hide <plugin_id>");
                        } else if let Some(pid) = resolve_plugin_arg(&parts[2]) {
                            if close_editor_window(pid) {
                                println!("editor closed for plugin {}", pid);
                            } else if manager().lock().unwrap().get_instance(pid).is_some() {
                                cmd_err!("plugin {} has no open editor", pid);
                            } else {
                                cmd_err!("plugin {} not found", pid);
                            }
                        } else {
                            cmd_err!("invalid plugin id or alias");
                        }
                    }
                    "preset" => {
                        if parts.len() < 3 {
                            cmd_err!("usage: plugin preset <list|select|scan|load|save> ...");
                        } else {
                            match parts[2].as_str() {
                                "list" => {
                                    if parts.len() < 4 {
                                        cmd_err!("usage: plugin preset list <plugin_id>");
                                    } else if let Some(pid) = resolve_plugin_arg(&parts[3]) {
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
                                        cmd_err!("invalid plugin id or alias");
                                    }
                                }
                                "select" => {
                                    // plugin preset select <plugin_id> <preset_id>
                                    // rebuild a pseudo-parts for parse_select_args compatibility is awkward; parse directly:
                                    if parts.len() < 5 {
                                        cmd_err!("usage: plugin preset select <plugin_id> <preset_id>");
                                    } else if let (Some(pid), Ok(preset)) = (resolve_plugin_arg(&parts[3]), parts[4].parse::<u32>()) {
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
                                    } else {
                                        cmd_err!("invalid ids");
                                    }
                                }
                                "scan" => {
                                    if parts.len() < 4 {
                                        cmd_err!("usage: plugin preset scan <dir>");
                                    } else {
                                        let dir = PathBuf::from(&parts[3]);
                                        scan_presets(&dir);
                                    }
                                }
                                "load" => {
                                    if parts.len() < 5 {
                                        cmd_err!("usage: plugin preset load <plugin_id> <file>");
                                    } else if let Some(pid) = resolve_plugin_arg(&parts[3]) {
                                        let file = PathBuf::from(&parts[4]);
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
                                        cmd_err!("invalid plugin id or alias");
                                    }
                                }
                                "save" => {
                                    if parts.len() < 5 {
                                        cmd_err!("usage: plugin preset save <plugin_id> <file>");
                                    } else if let Some(pid) = resolve_plugin_arg(&parts[3]) {
                                        let file = PathBuf::from(&parts[4]);
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
                                        cmd_err!("invalid plugin id or alias");
                                    }
                                }
                                other => cmd_err!("unknown preset action: {}", other),
                            }
                        }
                    }
                    "param" => {
                        if parts.len() < 3 {
                            cmd_err!("usage: plugin param <list|get|set> ...");
                        } else {
                            match parts[2].as_str() {
                                "list" => {
                                    if parts.len() < 4 {
                                        cmd_err!("usage: plugin param list <plugin_id>");
                                    } else if let Some(pid) = resolve_plugin_arg(&parts[3]) {
                                        let mgr = manager().lock().unwrap();
                                        let inst = mgr.get_instance(pid);
                                        drop(mgr);
                                        if let Some(inst) = inst {
                                            let inst = inst.lock().unwrap();
                                            let info = inst.info();
                                            for p in &info.params {
                                                let val = inst.get_param(p.index);
                                                let name = inst.param_name(p.index);
                                                println!("  [{}] {} = {:.4} (range {}..{})", p.index, name, val, p.min, p.max);
                                            }
                                        } else {
                                            cmd_err!("plugin {} not found", pid);
                                        }
                                    } else {
                                        cmd_err!("invalid plugin id or alias");
                                    }
                                }
                                "get" => {
                                    if parts.len() < 5 {
                                        cmd_err!("usage: plugin param get <plugin_id> <index>");
                                    } else if let (Some(pid), Ok(idx)) = (resolve_plugin_arg(&parts[3]), parts[4].parse::<u32>()) {
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
                                    if parts.len() < 6 {
                                        cmd_err!("usage: plugin param set <plugin_id> <index|volume> <value>");
                                    } else if let (Some(pid), Ok(val)) = (resolve_plugin_arg(&parts[3]), parts[5].parse::<f32>()) {
                                        if parts[4] == "volume" {
                                            let mgr = manager().lock().unwrap();
                                            match mgr.set_gain(pid, val) {
                                                Ok(_) => println!("volume set to {} for plugin {}", val, pid),
                                                Err(e) => cmd_err!("error: {}", e),
                                            }
                                        } else if let Ok(idx) = parts[4].parse::<u32>() {
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
                    other => cmd_err!("unknown plugin action: {}", other),
                }
            }
        }
        "midi" => {
            if parts.len() < 2 {
                cmd_err!("usage: midi <list|listen|connect|disconnect|stats> ...");
            } else {
                match parts[1].as_str() {
                    "list" => match spaph_midi::list_input_ports() {
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
                    "listen" => {
                        if parts.len() < 3 {
                            cmd_err!("usage: midi listen <port> [midi_channel]");
                        } else {
                            let ch = if parts.len() >= 4 {
                                match parse_midi_channel(&parts[3]) {
                                    Ok(c) => Some(c),
                                    Err(e) => { cmd_err!("{}", e); None }
                                }
                            } else {
                                None
                            };
                            // if parse failed, ch is None but we already errored; skip
                            if parts.len() >= 4 && ch.is_none() {
                                // error already reported
                            } else if IN_SCRIPT.load(Ordering::Relaxed) || IN_OSC.load(Ordering::Relaxed) {
                                eprintln!("warning: 'midi listen' is interactive only; skipped in script or over OSC");
                            } else {
                                midi_listen(&parts[2], ch);
                            }
                        }
                    }
                    "connect" => {
                        if parts.len() < 4 {
                            cmd_err!("usage: midi connect <port> <plugin_id> [midi_channel]");
                        } else if let Some(pid) = resolve_plugin_arg(&parts[3]) {
                            let ch = if parts.len() >= 5 {
                                match parse_midi_channel(&parts[4]) {
                                    Ok(c) => Some(c),
                                    Err(e) => { cmd_err!("{}", e); None }
                                }
                            } else {
                                None
                            };
                            if parts.len() >= 5 && ch.is_none() {
                                // error already reported
                            } else {
                                // register connection with channel
                                midi_connect(parts[2].clone(), pid, ch);
                            }
                        } else {
                            cmd_err!("invalid plugin id or alias");
                        }
                    }
                    "stats" => {
                        if parts.len() >= 3 && parts[2] == "reset" {
                            spaph_audio::midi_stats::reset();
                            println!("midi stats reset");
                        } else {
                            let s = spaph_audio::midi_stats::snapshot();
                            println!("midi events (all connections, since start or last reset):");
                            println!("  received  {:>10}  from input ports", s.received);
                            println!("  filtered  {:>10}  rejected by channel filter", s.filtered);
                            println!("  queued    {:>10}  waiting for a plugin", s.queued);
                            println!("  delivered {:>10}  handed to plugins", s.delivered);
                            println!("  deferred  {:>10}  audio blocks that waited for a busy plugin", s.deferred);
                            println!("  pending   {:>10}  queued but not yet delivered", s.queued.saturating_sub(s.delivered));
                            println!("  note-on   {:>10}  delivered", s.note_on);
                            println!("  note-off  {:>10}  delivered", s.note_off);
                            println!("  held      {:>10}  note-on minus note-off (0 when all keys are released)", s.note_on as i64 - s.note_off as i64);
                        }
                    }
                    "disconnect" => {
                        if parts.len() == 2 {
                            // midi disconnect (no args) -> disconnect all MIDI
                            disconnect_all_midi();
                            println!("midi disconnected");
                        } else if parts.len() == 4 {
                            // midi disconnect <port> <id>
                            let port = &parts[2];
                            let pid: u32 = match resolve_plugin_arg(&parts[3]) {
                                Some(v) => v,
                                None => { cmd_err!("invalid plugin id or alias"); return true; }
                            };
                            match disconnect_midi_filtered(Some(port), Some(pid), None) {
                                Ok(_) => println!("midi disconnected"),
                                Err(e) => cmd_err!("error: {}", e),
                            }
                        } else if parts.len() == 5 {
                            // midi disconnect <port> <id> <channel>
                            let port = &parts[2];
                            let pid: u32 = match resolve_plugin_arg(&parts[3]) {
                                Some(v) => v,
                                None => { cmd_err!("invalid plugin id or alias"); return true; }
                            };
                            let ch = match parse_midi_channel(&parts[4]) {
                                Ok(c) => c,
                                Err(e) => { cmd_err!("{}", e); return true; }
                            };
                            match disconnect_midi_filtered(Some(port), Some(pid), Some(ch)) {
                                Ok(_) => println!("midi disconnected"),
                                Err(e) => cmd_err!("error: {}", e),
                            }
                        } else {
                            cmd_err!("usage: midi disconnect [<port> <plugin_id> [channel]]");
                        }
                    }
                    other => cmd_err!("unknown midi action: {}", other),
                }
            }
        }
        "audio" => {
            if parts.len() < 2 {
                cmd_err!("usage: audio <list|connect|disconnect> ...");
            } else {
                match parts[1].as_str() {
                    "list" => match spaph_audio::list_devices() {
                        Ok(devs) => {
                            if devs.is_empty() {
                                println!("(no audio output devices)");
                            }
                            for (i, d) in devs.iter().enumerate() {
                                println!("  [{}] {}", i, d);
                            }
                        }
                        Err(e) => cmd_err!("error: {}", e),
                    },
                    "connect" => {
                        if parts.len() < 3 {
                            cmd_err!("usage: audio connect <plugin_id> [device_index]");
                        } else if let Some(pid) = resolve_plugin_arg(&parts[2]) {
                            let dev_idx = match parts.get(3) {
                                Some(token) => match resolve_device_arg(token) {
                                    Ok(i) => Some(i),
                                    Err(e) => {
                                        cmd_err!("{}", e);
                                        return true;
                                    }
                                },
                                None => None,
                            };
                            let mgr = manager().lock().unwrap();
                            let inst = mgr.get_instance(pid);
                            let q = mgr.get_midi_queue(pid);
                            let gain = mgr.get_gain_handle(pid);
                            drop(mgr);
                            let defaults = audio_defaults().lock().unwrap();
                            let dev_idx = dev_idx.or(defaults.device_index);
                            drop(defaults);
                            match (inst, q, gain) {
                                (Some(inst), Some(q), Some(gain)) => {
                                    if let Err(e) = spaph_audio::set_output_device(dev_idx) {
                                        cmd_err!("error: {}", e);
                                    } else {
                                        match spaph_audio::arm(pid, inst, gain, q) {
                                            Ok(_) => {
                                                let dev_name = match dev_idx {
                                                    Some(i) => spaph_audio::device_name(Some(i)).unwrap_or_else(|| format!("device {}", i)),
                                                    None => "default".to_string(),
                                                };
                                                manager().lock().unwrap().set_audio_device(pid, dev_name.clone());
                                                println!("audio connected for plugin {} on '{}'", pid, dev_name);
                                            }
                                            Err(e) => cmd_err!("error: {}", e),
                                        }
                                    }
                                }
                                _ => cmd_err!("plugin {} not found", pid),
                            }
                        } else {
                            cmd_err!("invalid plugin id or alias");
                        }
                    }
                    "disconnect" => {
                        let pid = parts.get(2).and_then(|s| resolve_plugin_arg(s));
                        let dev = parts.get(3).map(|s| s.as_str());
                        match disconnect_audio(pid, dev) {
                            Ok(_) => println!("audio disconnected"),
                            Err(e) => cmd_err!("error: {}", e),
                        }
                    }
                    other => cmd_err!("unknown audio action: {}", other),
                }
            }
        }
        "volume" => {
            if parts.len() == 2 {
                if let Ok(level) = parts[1].parse::<f32>() {
                    let active = spaph_audio::active_ids().into_iter().next();
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
                if let (Some(pid), Ok(level)) = (resolve_plugin_arg(&parts[1]), parts[2].parse::<f32>()) {
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
        "master" => {
            if parts.len() == 2 {
                if let Ok(level) = parts[1].parse::<f32>() {
                    spaph_audio::set_master_gain(level);
                    println!("master volume set to {}", level);
                } else {
                    cmd_err!("usage: master <level>");
                }
            } else {
                cmd_err!("usage: master <level>");
            }
        }
        "disconnect" => {
            cleanup_connections();
            println!("disconnected");
        }
        "backend" => {
            if parts.len() >= 2 && parts[1] == "list" {
                for (name, avail, current) in spaph_audio::backend_list() {
                    let mark = if current { " (current)" } else { "" };
                    let status = if avail { "available" } else { "unavailable" };
                    println!("  {}  {}{}", name, status, mark);
                }
            } else {
                cmd_err!("usage: backend list");
            }
        }
        "load" => {
            if parts.len() < 2 {
                cmd_err!("usage: load <script>");
            } else if LOAD_DEPTH.load(Ordering::Relaxed) >= MAX_LOAD_DEPTH {
                cmd_err!("load: scripts nested too deep (max {})", MAX_LOAD_DEPTH);
            } else {
                match spaph_config::parse_param_args(&parts[2..]) {
                    Err(e) => cmd_err!("load {}: {}", parts[1], e),
                    Ok(params) => {
                        let base = CONFIG_DIR.get().and_then(|d| d.as_deref());
                        let cfg = spaph_config::Config { script: Some(PathBuf::from(&parts[1])), ..Default::default() };
                        match spaph_config::resolve_script(&cfg, base) {
                            Some(path) => {
                                LOAD_DEPTH.fetch_add(1, Ordering::Relaxed);
                                let result = run_script(&path, &params);
                                LOAD_DEPTH.fetch_sub(1, Ordering::Relaxed);
                                // A failing line in the loaded script is an error of this command too,
                                // so the calling script stops as well.
                                if let Err(msg) = result {
                                    cmd_err!("load {}: {}", parts[1], msg);
                                }
                            }
                            None => cmd_err!("usage: load <script>"),
                        }
                    }
                }
            }
        }
        "backing" => backing_command(&parts),
        "quit" | "exit" => {
            return false;
        }
        other => cmd_err!("unknown command: {}", other),
    }
    true
}


/// `backing` commands: play a stereo file from `backing_dir` alongside the plugins.
///
/// `backing load <name>`, `play`, `pause`, `stop`, `begin`, `unload`, `seek <+/-seconds>`,
/// `volume <dB>` (absolute) or `volume <+dB/-dB>` (relative), and `backing` alone for status.
fn backing_command(parts: &[String]) {
    use spaph_types::SpaphError;
    let name = || parts[2..].join(" ");
    let result: spaph_types::Result<()> = match parts.get(1).map(String::as_str) {
        None => {
            let st = spaph_backing::status();
            println!(
                "backing: {} | {:?} | {:.1} s | {:+.1} dB",
                st.file.as_deref().unwrap_or("(none)"),
                st.state,
                st.position_secs,
                st.volume_db
            );
            Ok(())
        }
        Some("load") if parts.len() >= 3 => spaph_backing::load(&name()),
        Some("unload") => {
            spaph_backing::unload();
            Ok(())
        }
        Some("play") => spaph_backing::play(),
        Some("pause") => {
            spaph_backing::pause();
            Ok(())
        }
        Some("stop") => spaph_backing::stop(),
        Some("begin") => spaph_backing::begin(),
        Some("seek") if parts.len() == 3 => match parts[2].parse::<f64>() {
            Ok(secs) => spaph_backing::seek_relative(secs),
            Err(_) => Err(SpaphError::Audio(format!("seek: not a number: {}", parts[2]))),
        },
        Some("volume") if parts.len() == 3 => match parts[2].parse::<f32>() {
            Ok(db) if parts[2].starts_with(['+', '-']) => {
                spaph_backing::change_volume_db(db);
                Ok(())
            }
            Ok(db) => {
                spaph_backing::set_volume_db(db);
                Ok(())
            }
            Err(_) => Err(SpaphError::Audio(format!("volume: not a number: {}", parts[2]))),
        },
        _ => Err(SpaphError::Audio(
            "usage: backing [load <name> | play | pause | stop | begin | unload | seek <s> | volume <dB>]".into(),
        )),
    };
    if let Err(e) = result {
        cmd_err!("backing: {}", e);
    }
}

/// Block until the main thread acknowledges the last command. Returns `false` if the
/// session is ending (ack channel closed or stop requested), so the caller should exit.
fn wait_for_ack(ack_rx: &std::sync::mpsc::Receiver<()>, stop: &std::sync::atomic::AtomicBool) -> bool {
    loop {
        match ack_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(()) => return true,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    return false;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// A command waiting for the console thread: typed at the prompt, or received over OSC.
struct ConsoleMsg {
    text: String,
    from_osc: bool,
}

/// True if `line` would end the program (`quit` / `exit` as its first word).
fn is_quit_command(line: &str) -> bool {
    matches!(split_args(line).first().map(String::as_str), Some("quit") | Some("exit"))
}

/// Run a command received over OSC. It runs on the console thread like a typed command, but
/// is non-interactive (see `IN_OSC`), and it cannot end the program.
fn run_osc_line(line: &str) {
    if is_quit_command(line) {
        eprintln!("warning: 'quit' is ignored over OSC");
        return;
    }
    IN_OSC.store(true, Ordering::Relaxed);
    let _ = run_line(line);
    IN_OSC.store(false, Ordering::Relaxed);
}

fn run_shell(osc_port: u16) {
    // Start OSC server
    let bind = format!("0.0.0.0:{}", osc_port);
    let stop = osc_stop();
    stop.store(false, std::sync::atomic::Ordering::Relaxed);
    // Console lines and OSC commands both end up on the console thread through this channel:
    // that thread owns the editor windows, so `plugin show/hide/unload` work from OSC too.
    let (msg_tx, msg_rx) = std::sync::mpsc::channel::<ConsoleMsg>();
    let osc_tx = msg_tx.clone();
    let handle = std::thread::spawn(move || {
        let on_command = move |cmd: String| {
            let _ = osc_tx.send(ConsoleMsg { text: cmd, from_osc: true });
        };
        if let Err(e) = spaph_osc::run_server(&bind, on_command, stop.clone()) {
            eprintln!("OSC server error: {}", e);
        }
    });
    *osc_thread_slot().lock().unwrap() = Some(handle);
    eprintln!("OSC listening on 0.0.0.0:{}", osc_port);

    // Save terminal state before rustyline changes it, so we can restore on exit.
    save_terminal();

    // Readline in a background thread so the main thread is free for the audio stream.
    // Acknowledgements from the main thread: a new prompt is shown only after the previous
    // command has finished, so command output never interleaves with the prompt.
    let (ack_tx, ack_rx) = std::sync::mpsc::channel::<()>();
    let stdin_stop_flag = stdin_stop();
    stdin_stop_flag.store(false, std::sync::atomic::Ordering::Relaxed);
    let handle = std::thread::spawn(move || {
        let mut rl = match DefaultEditor::new() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("readline init failed: {}", e);
                return;
            }
        };
        loop {
            if stdin_stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            match rl.readline("spaph> ") {
                Ok(line) => {
                    let _ = rl.add_history_entry(line.as_str());
                    let msg = ConsoleMsg { text: line.trim().to_string(), from_osc: false };
                    if msg_tx.send(msg).is_err() {
                        break;
                    }
                }
                Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
                    let _ = msg_tx.send(ConsoleMsg { text: "quit".to_string(), from_osc: false });
                    break;
                }
                Err(e) => {
                    if !stdin_stop().load(std::sync::atomic::Ordering::Relaxed) {
                        eprintln!("readline error: {}", e);
                    }
                    // The OSC thread also holds a sender, so end the console loop explicitly.
                    let _ = msg_tx.send(ConsoleMsg { text: "quit".to_string(), from_osc: false });
                    break;
                }
            }
            // Wait for the command to finish before prompting again.
            if !wait_for_ack(&ack_rx, &stdin_stop_flag) {
                break;
            }
        }
        // Dropping rl here restores the terminal (rustyline's Drop).
    });
    *stdin_thread_slot().lock().unwrap() = Some(handle);

    // Wait for commands, but wake up regularly to service open editor windows. The prompt
    // itself runs on its own thread, so this loop never blocks the console.
    loop {
        match msg_rx.recv_timeout(EDITOR_FRAME) {
            Ok(msg) if msg.from_osc => run_osc_line(&msg.text),
            Ok(msg) => {
                if !run_line(&msg.text) {
                    break;
                }
                let _ = ack_tx.send(());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        pump_editors();
    }
    // Close editor windows on this thread, which created them.
    close_all_editor_windows();
    // Disconnecting the ack channel wakes the reader thread so it exits without needing input.
    drop(ack_tx);
    // Signal the stdin thread to stop and wait briefly for it to restore the terminal.
    stdin_stop().store(true, std::sync::atomic::Ordering::Relaxed);
    restore_terminal();
    // Teardown (including the audio stream) runs in shutdown(), under its timeout.
}



static MANAGER: std::sync::OnceLock<std::sync::Mutex<PluginManager>> = std::sync::OnceLock::new();

fn manager() -> &'static std::sync::Mutex<PluginManager> {
    MANAGER.get_or_init(|| std::sync::Mutex::new(PluginManager::new()))
}


/// Load the configuration: `--config` if given (must exist), else `./spaph.json` if present.
fn load_startup_config(cli: &Cli) -> (Option<PathBuf>, Config) {
    let path = match &cli.config {
        Some(p) => Some(p.clone()),
        None => spaph_config::find_default_config_any()
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
fn run_script(path: &Path, params: &spaph_config::ParamValues) -> std::result::Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read script {}: {}", path.display(), e))?;
    // Parameters are checked and substituted before the first command runs.
    let lines = spaph_config::bind_script(&text, params)
        .map_err(|e| format!("script {}: {}", path.display(), e))?;
    let was_in_script = IN_SCRIPT.swap(true, Ordering::Relaxed);
    let mut result = Ok(());
    for ScriptLine { line, command } in lines {
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
    // Restore rather than clear: a nested `load` runs inside an outer script.
    IN_SCRIPT.store(was_in_script, Ordering::Relaxed);
    result
}

/// Last-chance handler for crashes (e.g. a plugin dereferencing a bad pointer). Without it
/// Windows ends the process silently; this prints the exception code and address first.
#[cfg(windows)]
unsafe extern "system" fn report_crash(
    info: *mut winapi::um::winnt::EXCEPTION_POINTERS,
) -> winapi::um::winnt::LONG {
    use winapi::um::libloaderapi::{
        GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
        GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    };
    let record = (*info).ExceptionRecord;
    let addr = (*record).ExceptionAddress as usize;
    // Which module (plugin DLL or host) the faulting instruction belongs to, and the offset in it.
    let mut module = std::ptr::null_mut();
    let located = GetModuleHandleExW(
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        addr as *const u16,
        &mut module,
    );
    let (name, offset) = if located != 0 {
        let mut buf = [0u16; 512];
        let n = GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32) as usize;
        (String::from_utf16_lossy(&buf[..n]), addr - module as usize)
    } else {
        ("unknown module".to_string(), 0)
    };
    // For access violations: ExceptionInformation[0] is 0 = read, 1 = write, 8 = DEP;
    // [1] is the address that was accessed. A value near 0 means a null or bad pointer.
    let access = if (*record).NumberParameters >= 2 {
        let kind = match (*record).ExceptionInformation[0] {
            0 => "read",
            1 => "write",
            8 => "execute",
            _ => "?",
        };
        format!(" ({} of 0x{:X})", kind, (*record).ExceptionInformation[1])
    } else {
        String::new()
    };
    eprintln!(
        "spaph: fatal exception 0x{:08X}{} at 0x{:X} in {} + 0x{:X} (the program stops; see the last log lines above)",
        (*record).ExceptionCode,
        access,
        addr,
        name,
        offset
    );
    // EXCEPTION_EXECUTE_HANDLER: terminate after reporting.
    1
}

fn main() {
    #[cfg(windows)]
    unsafe {
        winapi::um::errhandlingapi::SetUnhandledExceptionFilter(Some(report_crash));
    }
    let _ = CONSOLE_THREAD.set(std::thread::current().id());
    let cli = Cli::parse();
    let (config_path, mut cfg) = load_startup_config(&cli);
    let _ = CONFIG_DIR.set(config_path.as_deref().and_then(|p| p.parent()).map(Path::to_path_buf));

    let overrides = Overrides {
        sample_rate: cli.sample_rate,
        buffer_size: cli.buffer_size,
        device_index: cli.audio_device,
        backend: cli.audio_backend.clone(),
        log_level: cli.log_level.map(|l| l.name().to_string()),
        osc_port: cli.osc_port,
    };
    if let Err(e) = spaph_config::apply_overrides(&mut cfg, &overrides) {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }

    init_logging(cfg.log_level.as_deref().unwrap_or("none"));
    tracing::debug!("config loaded: {:?}", config_path);
    if let Err(e) = spaph_audio::set_backend(cfg.audio.backend.as_deref()) {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
    {
        let mut d = audio_defaults().lock().unwrap();
        d.sample_rate = cfg.audio.sample_rate;
        d.buffer_size = cfg.audio.buffer_size;
        d.device_index = cfg.audio.device_index;
    }

    let config_dir = config_path.as_deref().and_then(|p| p.parent());
    if let Some(rate) = cfg.audio.sample_rate {
        spaph_backing::set_output_rate(rate);
    }
    spaph_backing::set_backing_dir(spaph_config::resolve_backing_dir(&cfg, config_dir));

    let script = spaph_config::resolve_script(
        &cfg,
        config_path.as_deref().and_then(|p| p.parent()),
    );
    if let Some(script) = script {
        if let Err(msg) = run_script(&script, &spaph_config::ParamValues::new()) {
            eprintln!("error: {}", msg);
            shutdown(1);
        }
    }

    // Eagerly create the audio stream so the node is always visible.
    if let Err(e) = spaph_audio::start_stream_with(cfg.audio.sample_rate, cfg.audio.buffer_size, cfg.audio.device_index) {
        eprintln!("audio stream error: {}", e);
    }
    run_shell(cfg.osc.port.unwrap_or(9000));
    shutdown(0);
}

#[cfg(test)]
mod tests {
    use super::{is_quit_command, match_device, midi_channel_of, midi_matches, parse_midi_channel, split_args};

    #[test]
    fn quit_detected_as_first_word() {
        assert!(is_quit_command("quit"));
        assert!(is_quit_command("  exit  "));
        assert!(is_quit_command("quit now"));
    }

    #[test]
    fn other_commands_are_not_quit() {
        assert!(!is_quit_command("master 0.5"));
        assert!(!is_quit_command("load quit.spaph"));
        assert!(!is_quit_command(""));
    }

    #[test]
    fn split_simple() {
        assert_eq!(split_args("help"), vec!["help".to_string()]);
    }

    #[test]
    fn split_quoted_path_with_spaces() {
        assert_eq!(
            split_args("plugin load \"/usr/lib/vst3/Surge XT.vst3\""),
            vec!["plugin".to_string(), "load".to_string(), "/usr/lib/vst3/Surge XT.vst3".to_string()]
        );
    }

    #[test]
    fn split_single_quoted_path_with_spaces() {
        assert_eq!(
            split_args("plugin load '/usr/lib/vst3/Surge XT.vst3'"),
            vec!["plugin".to_string(), "load".to_string(), "/usr/lib/vst3/Surge XT.vst3".to_string()]
        );
    }

    #[test]
    fn split_apostrophe_inside_double_quotes() {
        assert_eq!(
            split_args("plugin load \"it's here\""),
            vec!["plugin".to_string(), "load".to_string(), "it's here".to_string()]
        );
    }

    #[test]
    fn split_empty() {
        assert!(split_args("   ").is_empty());
    }

    #[test]
    fn midi_channel_of_note_on() {
        assert_eq!(midi_channel_of(&[0x90, 60, 100]), Some(1));
        assert_eq!(midi_channel_of(&[0x9F, 60, 100]), Some(16));
    }

    #[test]
    fn midi_channel_of_cc() {
        assert_eq!(midi_channel_of(&[0xB3, 7, 100]), Some(4));
    }

    #[test]
    fn midi_channel_of_system_is_none() {
        assert_eq!(midi_channel_of(&[0xF8]), None);
        assert_eq!(midi_channel_of(&[0xF0, 0x7E, 0x7F, 0x09, 0x01, 0xF7]), None);
        assert_eq!(midi_channel_of(&[]), None);
    }

    #[test]
    fn midi_matches_no_filter_matches_all() {
        assert!(midi_matches(&[0x90, 60, 100], None));
        assert!(midi_matches(&[0x80, 60, 0], None));
        assert!(midi_matches(&[0xF8], None));
    }

    #[test]
    fn midi_matches_filter_accepts_matching_channel() {
        assert!(midi_matches(&[0x90, 60, 100], Some(1)));
        assert!(midi_matches(&[0xB0, 7, 100], Some(1)));
    }

    #[test]
    fn midi_matches_filter_rejects_other_channel() {
        assert!(!midi_matches(&[0x91, 60, 100], Some(1)));
        assert!(!midi_matches(&[0x9F, 60, 100], Some(1)));
    }

    #[test]
    fn midi_matches_filter_passes_system_messages() {
        assert!(midi_matches(&[0xF8], Some(1)));
        assert!(midi_matches(&[0xF0, 0x00], Some(1)));
    }

    #[test]
    fn parse_midi_channel_valid() {
        for c in 1u8..=16 {
            assert_eq!(parse_midi_channel(&c.to_string()).unwrap(), c);
        }
    }

    #[test]
    fn parse_midi_channel_invalid() {
        assert!(parse_midi_channel("0").is_err());
        assert!(parse_midi_channel("17").is_err());
        assert!(parse_midi_channel("abc").is_err());
        assert!(parse_midi_channel("").is_err());
    }

    #[test]
    fn match_device_by_index_needs_no_name() {
        assert_eq!(match_device("1", &[]).unwrap(), 1);
    }

    #[test]
    fn match_device_by_partial_name_ignores_case() {
        let names = vec![
            "Speakers (Realtek)".to_string(),
            "Haut-parleurs (Scarlett Solo USB)".to_string(),
        ];
        assert_eq!(match_device("scarlett solo", &names).unwrap(), 1);
    }

    #[test]
    fn match_device_unknown_name_is_an_error() {
        let names = vec!["Speakers (Realtek)".to_string()];
        assert!(match_device("Scarlett", &names).unwrap_err().contains("no audio device matches"));
    }

    #[test]
    fn match_device_ambiguous_name_lists_candidates() {
        let names = vec!["USB Audio A".to_string(), "USB Audio B".to_string()];
        let err = match_device("usb", &names).unwrap_err();
        assert!(err.contains("more than one device"), "{err}");
        assert!(err.contains("[0] USB Audio A") && err.contains("[1] USB Audio B"), "{err}");
    }
}
