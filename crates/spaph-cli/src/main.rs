use clap::{Parser, Subcommand, ValueEnum};
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

#[derive(Parser)]
#[command(name = "spaph", version, about = "Simple Portable Audio Plugins Host")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(long)]
    sample_rate: Option<u32>,

    #[arg(long)]
    buffer_size: Option<u32>,

    #[arg(long)]
    audio_device: Option<usize>,

    #[arg(long, default_value = "none")]
    log_level: LogLevel,
}

#[derive(Subcommand)]
enum Commands {
    Load { path: PathBuf },
    List {
        #[command(subcommand)]
        target: ListTarget,
    },
    Midi {
        #[command(subcommand)]
        action: MidiAction,
    },
    Shell,
    Unload { id: u32 },
    Show { plugin_id: u32 },
    Select { plugin_id: u32, preset_id: u32 },
    Connect {
        #[command(subcommand)]
        target: ConnectTarget,
    },
    Disconnect,
    Preset {
        #[command(subcommand)]
        action: PresetAction,
    },
    Volume { plugin_id: Option<u32>, level: f32 },
    Param {
        #[command(subcommand)]
        action: ParamAction,
    },
}

#[derive(Subcommand)]
enum ListTarget {
    Midi,
    Audio,
    Plugins,
}

#[derive(Subcommand)]
enum MidiAction {
    Listen { port: String },
}

#[derive(Subcommand)]
enum ConnectTarget {
    Audio { plugin_id: u32, device_index: Option<usize>, sample_rate: Option<u32> },
    Midi { port: String, plugin_id: u32 },
}

#[derive(Subcommand)]
enum ParamAction {
    List { plugin_id: u32 },
    Get { plugin_id: u32, index: u32 },
    Set { plugin_id: u32, name_or_index: String, value: f32 },
}

#[derive(Subcommand)]
enum PresetAction {
    Scan { dir: PathBuf },
    Load { plugin_id: u32, file: PathBuf },
    Save { plugin_id: u32, file: PathBuf },
}

fn split_args(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in line.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
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
            eprintln!("plugin {} not found", plugin_id);
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
                eprintln!("error: {}", e);
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

fn cleanup_connections() {
    AUDIO_HANDLE.with(|h| { *h.borrow_mut() = None; });
    for f in midi_stop_flags().lock().unwrap().iter() {
        f.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    midi_stop_flags().lock().unwrap().clear();
    let handles: Vec<_> = midi_thread_handles().lock().unwrap().drain(..).collect();
    for h in handles {
        let _ = h.join();
    }
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
                eprintln!("usage: load <path>");
            } else {
                let path = PathBuf::from(&parts[1]);
                if let Err(e) = load_plugin(&path) {
                    eprintln!("error: {}", e);
                }
            }
        }
        "list" => {
            if parts.len() < 2 {
                eprintln!("usage: list <midi|audio|plugins|presets>");
            } else {
                match parts[1].as_str() {
                    "presets" => {
                        if parts.len() < 3 {
                            eprintln!("usage: list presets <plugin_id>");
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
                                Err(e) => eprintln!("error: {}", e),
                            }
                        } else {
                            eprintln!("invalid plugin id");
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
                        Err(e) => eprintln!("error: {}", e),
                    },
                    "audio" => match spaph_audio::list_devices() {
                        Ok(devs) => {
                            for d in devs {
                                println!("{}", d);
                            }
                        }
                        Err(e) => eprintln!("error: {}", e),
                    },
                    other => eprintln!("unknown list target: {}", other),
                }
            }
        }
        "midi" => {
            if parts.len() >= 3 && parts[1] == "listen" {
                midi_listen(&parts[2]);
            } else {
                eprintln!("usage: midi listen <port>");
            }
        }
        "unload" => {
            if parts.len() < 2 {
                eprintln!("usage: unload <id>");
            } else if let Ok(id) = parts[1].parse::<u32>() {
                let mut mgr = manager().lock().unwrap();
                match mgr.unload(id) {
                    Ok(_) => println!("unloaded {}", id),
                    Err(e) => eprintln!("error: {}", e),
                }
            } else {
                eprintln!("invalid id");
            }
        }
        "connect" => {
            if parts.len() < 2 {
                eprintln!("usage: connect <audio|midi> ...");
            } else {
                match parts[1].as_str() {
                    "audio" => {
                        if parts.len() < 3 {
                            eprintln!("usage: connect audio <plugin_id> [device_index] [sample_rate] [buffer_size]");
                        } else if let Ok(pid) = parts[2].parse::<u32>() {
                            let dev_idx = parts.get(3).and_then(|s| s.parse::<usize>().ok());
                            let sr = parts.get(4).and_then(|s| s.parse::<u32>().ok());
                            let bs = parts.get(5).and_then(|s| s.parse::<u32>().ok());
                            let mgr = manager().lock().unwrap();
                            let inst = mgr.get_instance(pid);
                            let q = mgr.get_midi_queue(pid);
                            let gain = mgr.get_gain_handle(pid).unwrap();
                            drop(mgr);
                            let defaults = audio_defaults().lock().unwrap();
                            let dev_idx = dev_idx.or(defaults.device_index);
                            let sr = sr.or(defaults.sample_rate);
                            let bs = bs.or(defaults.buffer_size);
                            drop(defaults);
                            match (inst, q) {
                                (Some(inst), Some(q)) => {
                                    match spaph_audio::start_audio(inst, q, dev_idx, sr, bs, gain) {
                                        Ok(handle) => {
                                            AUDIO_HANDLE.with(|h| *h.borrow_mut() = Some(handle));
                                            println!("audio started for plugin {}", pid);
                                            *ACTIVE_AUDIO_PLUGIN.lock().unwrap() = Some(pid);
                                        }
                                        Err(e) => eprintln!("error: {}", e),
                                    }
                                }
                                _ => eprintln!("plugin {} not found", pid),
                            }
                        } else {
                            eprintln!("invalid plugin id");
                        }
                    }
                    "midi" => {
                        if parts.len() < 4 {
                            eprintln!("usage: connect midi <port> <plugin_id>");
                        } else if let Ok(pid) = parts[3].parse::<u32>() {
                            midi_connect(parts[2].clone(), pid);
                        } else {
                            eprintln!("invalid plugin id");
                        }
                    }
                    other => eprintln!("unknown connect target: {}", other),
                }
            }
        }
        "disconnect" => {
            cleanup_connections();
            println!("disconnected");
        }
        "select" => {
            match parse_select_args(&parts) {
                None => eprintln!("usage: select <plugin_id> <preset_id>  (or: select preset <plugin_id> <preset_id>)"),
                Some((pid, preset)) => {
                    if preset == 0 {
                        eprintln!("preset id must be >= 1");
                    } else {
                        let mut mgr = manager().lock().unwrap();
                        match mgr.select_preset(pid, (preset - 1) as usize) {
                            Ok(_) => {
                                let name = mgr.list_presets(pid).ok().and_then(|ps| ps.get((preset - 1) as usize).cloned()).unwrap_or_default();
                                println!("selected preset {} for plugin {}: {}", preset, pid, name);
                            }
                            Err(e) => eprintln!("error: {}", e),
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
                                Err(e) => eprintln!("error: {}", e),
                            }
                        }
                        None => eprintln!("no active audio plugin; use: volume <plugin_id> <level>"),
                    }
                } else {
                    eprintln!("usage: volume [<plugin_id>] <level>");
                }
            } else if parts.len() == 3 {
                if let (Ok(pid), Ok(level)) = (parts[1].parse::<u32>(), parts[2].parse::<f32>()) {
                    let mgr = manager().lock().unwrap();
                    match mgr.set_gain(pid, level) {
                        Ok(_) => println!("volume set to {} for plugin {}", level, pid),
                        Err(e) => eprintln!("error: {}", e),
                    }
                } else {
                    eprintln!("usage: volume [<plugin_id>] <level>");
                }
            } else {
                eprintln!("usage: volume [<plugin_id>] <level>");
            }
        }
        "param" => {
            if parts.len() < 2 {
                eprintln!("usage: param <list|get|set> ...");
            } else {
                match parts[1].as_str() {
                    "list" => {
                        if parts.len() < 3 {
                            eprintln!("usage: param list <plugin_id>");
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
                                eprintln!("plugin {} not found", pid);
                            }
                        } else {
                            eprintln!("invalid plugin id");
                        }
                    }
                    "get" => {
                        if parts.len() < 4 {
                            eprintln!("usage: param get <plugin_id> <index>");
                        } else if let (Ok(pid), Ok(idx)) = (parts[2].parse::<u32>(), parts[3].parse::<u32>()) {
                            let mgr = manager().lock().unwrap();
                            let inst = mgr.get_instance(pid);
                            drop(mgr);
                            if let Some(inst) = inst {
                                let inst = inst.lock().unwrap();
                                let val = inst.get_param(idx);
                                println!("{}", val);
                            } else {
                                eprintln!("plugin {} not found", pid);
                            }
                        } else {
                            eprintln!("invalid ids");
                        }
                    }
                    "set" => {
                        if parts.len() < 5 {
                            eprintln!("usage: param set <plugin_id> <index|volume> <value>");
                        } else if let (Ok(pid), Ok(val)) = (parts[2].parse::<u32>(), parts[4].parse::<f32>()) {
                            if parts[3] == "volume" {
                                let mgr = manager().lock().unwrap();
                                match mgr.set_gain(pid, val) {
                                    Ok(_) => println!("volume set to {} for plugin {}", val, pid),
                                    Err(e) => eprintln!("error: {}", e),
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
                                    eprintln!("plugin {} not found", pid);
                                }
                            } else {
                                eprintln!("invalid param index");
                            }
                        } else {
                            eprintln!("invalid args");
                        }
                    }
                    other => eprintln!("unknown param action: {}", other),
                }
            }
        }
        "preset" => {
            if parts.len() < 2 {
                eprintln!("usage: preset <scan|load|save> ...");
            } else {
                match parts[1].as_str() {
                    "save" => {
                        if parts.len() < 4 {
                            eprintln!("usage: preset save <plugin_id> <file>");
                        } else if let Ok(pid) = parts[2].parse::<u32>() {
                            let file = PathBuf::from(&parts[3]);
                            let mgr = manager().lock().unwrap();
                            let inst = mgr.get_instance(pid);
                            drop(mgr);
                            if let Some(inst) = inst {
                                let inst = inst.lock().unwrap();
                                match inst.save_preset_file(&file) {
                                    Ok(_) => println!("saved preset {}", file.display()),
                                    Err(e) => eprintln!("error: {}", e),
                                }
                            } else {
                                eprintln!("plugin {} not found", pid);
                            }
                        } else {
                            eprintln!("invalid plugin id");
                        }
                    }
                    "scan" => {
                        if parts.len() < 3 {
                            eprintln!("usage: preset scan <dir>");
                        } else {
                            let dir = PathBuf::from(&parts[2]);
                            scan_presets(&dir);
                        }
                    }
                    "load" => {
                        if parts.len() < 4 {
                            eprintln!("usage: preset load <plugin_id> <file>");
                        } else if let Ok(pid) = parts[2].parse::<u32>() {
                            let file = PathBuf::from(&parts[3]);
                            let mgr = manager().lock().unwrap();
                            let inst = mgr.get_instance(pid);
                            drop(mgr);
                            if let Some(inst) = inst {
                                let mut inst = inst.lock().unwrap();
                                match inst.load_preset_file(&file) {
                                    Ok(_) => println!("loaded preset {}", file.display()),
                                    Err(e) => eprintln!("error: {}", e),
                                }
                            } else {
                                eprintln!("plugin {} not found", pid);
                            }
                        } else {
                            eprintln!("invalid plugin id");
                        }
                    }
                    other => eprintln!("unknown preset action: {}", other),
                }
            }
        }
        "show" => {
            if parts.len() < 2 {
                eprintln!("usage: show <plugin_id>");
            } else if let Ok(pid) = parts[1].parse::<u32>() {
                let mgr = manager().lock().unwrap();
                let inst = mgr.get_instance(pid);
                drop(mgr);
                if let Some(inst) = inst {
                    let mut inst = inst.lock().unwrap();
                    if !inst.has_editor() {
                        eprintln!("plugin {} has no editor", pid);
                    } else {
                        println!("opening editor for plugin {} (close window to continue)", pid);
                        if let Err(e) = inst.open_editor() {
                            eprintln!("error: {}", e);
                        }
                    }
                } else {
                    eprintln!("plugin {} not found", pid);
                }
            } else {
                eprintln!("invalid plugin id");
            }
        }
        "quit" | "exit" => {
            cleanup_connections();
            return false;
        }
        other => eprintln!("unknown command: {}", other),
    }
    true
}

fn run_shell() {
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
    cleanup_connections();
}

static MANAGER: std::sync::OnceLock<std::sync::Mutex<PluginManager>> = std::sync::OnceLock::new();

fn manager() -> &'static std::sync::Mutex<PluginManager> {
    MANAGER.get_or_init(|| std::sync::Mutex::new(PluginManager::new()))
}

thread_local! {
    static AUDIO_HANDLE: std::cell::RefCell<Option<spaph_audio::AudioHandle>> = std::cell::RefCell::new(None);
    static MIDI_LISTENERS: std::cell::RefCell<Vec<spaph_midi::MidiListener>> = std::cell::RefCell::new(Vec::new());
}

static MIDI_THREAD_HANDLES: std::sync::OnceLock<std::sync::Mutex<Vec<std::thread::JoinHandle<()>>>> =
    std::sync::OnceLock::new();

fn midi_thread_handles() -> &'static std::sync::Mutex<Vec<std::thread::JoinHandle<()>>> {
    MIDI_THREAD_HANDLES.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

fn main() {
    let cli = Cli::parse();
    let level_filter = match cli.log_level {
        LogLevel::None => "off".to_string(),
        LogLevel::Error => "error".to_string(),
        LogLevel::Warn => "warn".to_string(),
        LogLevel::Info => "info".to_string(),
        LogLevel::Debug => "debug".to_string(),
        LogLevel::Trace => "trace".to_string(),
    };
    let filter_str = if level_filter == "off" {
        "off".to_string()
    } else {
        format!("{},pulseaudio=off,vst3_host=warn,rustyline=off", level_filter)
    };
    let filter = tracing_subscriber::EnvFilter::try_new(&filter_str)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
    tracing::debug!("logging initialized");
    {
        let mut d = audio_defaults().lock().unwrap();
        d.sample_rate = cli.sample_rate;
        d.buffer_size = cli.buffer_size;
        d.device_index = cli.audio_device;
    }
    match cli.command {
        None => run_shell(),
        Some(Commands::Shell) => run_shell(),
        Some(Commands::Load { path }) => {
            if let Err(e) = load_plugin(&path) {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
        }
        Some(Commands::List { target }) => match target {
            ListTarget::Midi => match spaph_midi::list_input_ports() {
                Ok(ps) => {
                    if ps.is_empty() {
                        println!("(no midi input ports)");
                    }
                    for (i, p) in ps.iter().enumerate() {
                        println!("  [{}] {}", i, p);
                    }
                }
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(1);
                }
            },
            ListTarget::Plugins => {
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
            ListTarget::Audio => match spaph_audio::list_devices() {
                Ok(devs) => {
                    for d in devs {
                        println!("{}", d);
                    }
                }
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(1);
                }
            },
        },
        Some(Commands::Midi { action }) => match action {
            MidiAction::Listen { port } => midi_listen(&port),
        },
        Some(Commands::Connect { target }) => match target {
            ConnectTarget::Audio { plugin_id, device_index, sample_rate } => {
                let defaults = audio_defaults().lock().unwrap();
                let dev_idx = device_index.or(defaults.device_index);
                let sr = sample_rate.or(defaults.sample_rate);
                let bs = defaults.buffer_size;
                let gain = {
                    let m = manager().lock().unwrap();
                    m.get_gain_handle(plugin_id).unwrap()
                };
                drop(defaults);
                let mgr = manager().lock().unwrap();
                let inst = mgr.get_instance(plugin_id);
                let q = mgr.get_midi_queue(plugin_id);
                drop(mgr);
                match (inst, q) {
                    (Some(inst), Some(q)) => {
                        match spaph_audio::start_audio(inst, q, dev_idx, sr, bs, gain) {
                            Ok(handle) => {
                                AUDIO_HANDLE.with(|h| *h.borrow_mut() = Some(handle));
                                println!("audio started for plugin {}", plugin_id);
                                *ACTIVE_AUDIO_PLUGIN.lock().unwrap() = Some(plugin_id);
                            }
                            Err(e) => {
                                eprintln!("error: {}", e);
                                std::process::exit(1);
                            }
                        }
                    }
                    _ => {
                        eprintln!("plugin {} not found", plugin_id);
                        std::process::exit(1);
                    }
                }
            }
            ConnectTarget::Midi { port, plugin_id } => {
                midi_connect(port, plugin_id);
            }
        },
        Some(Commands::Disconnect) => {
            AUDIO_HANDLE.with(|h| { *h.borrow_mut() = None; });
            for f in midi_stop_flags().lock().unwrap().iter() {
                f.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            midi_stop_flags().lock().unwrap().clear();
            println!("disconnected");
        }
        Some(Commands::Select { plugin_id, preset_id }) => {
            if preset_id == 0 {
                eprintln!("preset id must be >= 1");
                std::process::exit(1);
            }
            let mut mgr = manager().lock().unwrap();
            match mgr.select_preset(plugin_id, (preset_id - 1) as usize) {
                Ok(_) => {
                    let name = mgr.list_presets(plugin_id).ok().and_then(|ps| ps.get((preset_id - 1) as usize).cloned()).unwrap_or_default();
                    println!("selected preset {} for plugin {}: {}", preset_id, plugin_id, name);
                }
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Some(Commands::Volume { plugin_id, level }) => {
            let pid = plugin_id.or(*ACTIVE_AUDIO_PLUGIN.lock().unwrap());
            match pid {
                Some(pid) => {
                    let mgr = manager().lock().unwrap();
                    match mgr.set_gain(pid, level) {
                        Ok(_) => println!("volume set to {} for plugin {}", level, pid),
                        Err(e) => {
                            eprintln!("error: {}", e);
                            std::process::exit(1);
                        }
                    }
                }
                None => {
                    eprintln!("no active audio plugin; specify plugin id");
                    std::process::exit(1);
                }
            }
        }
        Some(Commands::Param { action }) => match action {
            ParamAction::List { plugin_id } => {
                let mgr = manager().lock().unwrap();
                let inst = mgr.get_instance(plugin_id);
                drop(mgr);
                if let Some(inst) = inst {
                    let inst = inst.lock().unwrap();
                    let info = inst.info();
                    for p in &info.params {
                        let val = inst.get_param(p.index);
                        println!("  [{}] {} = {:.4} (range {}..{})", p.index, p.name, val, p.min, p.max);
                    }
                } else {
                    eprintln!("plugin {} not found", plugin_id);
                    std::process::exit(1);
                }
            }
            ParamAction::Get { plugin_id, index } => {
                let mgr = manager().lock().unwrap();
                let inst = mgr.get_instance(plugin_id);
                drop(mgr);
                if let Some(inst) = inst {
                    let inst = inst.lock().unwrap();
                    println!("{}", inst.get_param(index));
                } else {
                    eprintln!("plugin {} not found", plugin_id);
                    std::process::exit(1);
                }
            }
            ParamAction::Set { plugin_id, name_or_index, value } => {
                if name_or_index == "volume" {
                    let mgr = manager().lock().unwrap();
                    match mgr.set_gain(plugin_id, value) {
                        Ok(_) => println!("volume set to {} for plugin {}", value, plugin_id),
                        Err(e) => {
                            eprintln!("error: {}", e);
                            std::process::exit(1);
                        }
                    }
                } else if let Ok(idx) = name_or_index.parse::<u32>() {
                    let mgr = manager().lock().unwrap();
                    let inst = mgr.get_instance(plugin_id);
                    drop(mgr);
                    if let Some(inst) = inst {
                        let mut inst = inst.lock().unwrap();
                        inst.set_param(idx, value as f64);
                        println!("set param {} = {} for plugin {}", idx, value, plugin_id);
                    } else {
                        eprintln!("plugin {} not found", plugin_id);
                        std::process::exit(1);
                    }
                } else {
                    eprintln!("invalid param index");
                    std::process::exit(1);
                }
            }
        }
        Some(Commands::Preset { action }) => match action {
            PresetAction::Scan { dir } => scan_presets(&dir),
            PresetAction::Load { plugin_id, file } => {
                let mgr = manager().lock().unwrap();
                let inst = mgr.get_instance(plugin_id);
                drop(mgr);
                if let Some(inst) = inst {
                    let mut inst = inst.lock().unwrap();
                    match inst.load_preset_file(&file) {
                        Ok(_) => println!("loaded preset {}", file.display()),
                        Err(e) => {
                            eprintln!("error: {}", e);
                            std::process::exit(1);
                        }
                    }
                } else {
                    eprintln!("plugin {} not found", plugin_id);
                    std::process::exit(1);
                }
            }
            PresetAction::Save { plugin_id, file } => {
                let mgr = manager().lock().unwrap();
                let inst = mgr.get_instance(plugin_id);
                drop(mgr);
                if let Some(inst) = inst {
                    let inst = inst.lock().unwrap();
                    match inst.save_preset_file(&file) {
                        Ok(_) => println!("saved preset {}", file.display()),
                        Err(e) => {
                            eprintln!("error: {}", e);
                            std::process::exit(1);
                        }
                    }
                } else {
                    eprintln!("plugin {} not found", plugin_id);
                    std::process::exit(1);
                }
            }
        }
        Some(Commands::Show { plugin_id }) => {
            let mgr = manager().lock().unwrap();
            let inst = mgr.get_instance(plugin_id);
            drop(mgr);
            if let Some(inst) = inst {
                let mut inst = inst.lock().unwrap();
                if !inst.has_editor() {
                    eprintln!("plugin {} has no editor", plugin_id);
                    std::process::exit(1);
                }
                println!("opening editor for plugin {} (close window to continue)", plugin_id);
                if let Err(e) = inst.open_editor() {
                    eprintln!("error: {}", e);
                    std::process::exit(1);
                }
            } else {
                eprintln!("plugin {} not found", plugin_id);
                std::process::exit(1);
            }
        }
        Some(Commands::Unload { id }) => {
            let mut mgr = manager().lock().unwrap();
            match mgr.unload(id) {
                Ok(_) => println!("unloaded {}", id),
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(1);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_select_args, split_args};

    #[test]
    fn split_simple() {
        assert_eq!(split_args("help"), vec!["help".to_string()]);
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
