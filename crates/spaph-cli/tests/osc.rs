//! End-to-end tests of OSC command handling. OSC commands must run on the console thread,
//! print their output to the console, and never end the program.

use std::io::{BufRead, BufReader, Write};
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rosc::{OscMessage, OscPacket, OscType};

const TIMEOUT: Duration = Duration::from_secs(10);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("spaph_osc_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("c.json"), "{}").unwrap();
    dir
}

fn free_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn collect<R: std::io::Read + Send + 'static>(reader: R) -> Arc<Mutex<String>> {
    let buf = Arc::new(Mutex::new(String::new()));
    let sink = buf.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            let mut text = sink.lock().unwrap();
            text.push_str(&line);
            text.push('\n');
        }
    });
    buf
}

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
    port: u16,
}

impl Session {
    fn start(dir: &Path) -> Session {
        let port = free_port();
        let mut child = Command::new(env!("CARGO_BIN_EXE_spaph"))
            .args(["--config", dir.join("c.json").to_str().unwrap(), "--osc-port", &port.to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = collect(child.stdout.take().unwrap());
        let stderr = collect(child.stderr.take().unwrap());
        Session { child, stdin, stdout, stderr, port }
    }

    fn output(&self) -> String {
        format!("{}{}", self.stdout.lock().unwrap(), self.stderr.lock().unwrap())
    }

    /// Send `cmd` over OSC and resend until `needle` shows up in the program output.
    /// Resending covers the short window before the OSC server has bound its socket.
    fn osc_expect(&self, cmd: &str, needle: &str) {
        let packet = rosc::encoder::encode(&OscPacket::Message(OscMessage {
            addr: "/spaph/command".to_string(),
            args: vec![OscType::String(cmd.to_string())],
        }))
        .unwrap();
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let start = Instant::now();
        loop {
            if self.output().contains(needle) {
                return;
            }
            assert!(start.elapsed() < TIMEOUT, "timed out waiting for {needle:?} after OSC {cmd:?}\n{}", self.output());
            sock.send_to(&packet, ("127.0.0.1", self.port)).unwrap();
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Send `cmd` over OSC once (no resend).
    fn osc_send(&self, cmd: &str) {
        let packet = rosc::encoder::encode(&OscPacket::Message(OscMessage {
            addr: "/spaph/command".to_string(),
            args: vec![OscType::String(cmd.to_string())],
        }))
        .unwrap();
        UdpSocket::bind("127.0.0.1:0").unwrap().send_to(&packet, ("127.0.0.1", self.port)).unwrap();
    }

    /// End the program through the console (`quit`) and return (exit code, stdout, stderr).
    fn finish(mut self) -> (Option<i32>, String, String) {
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.write_all(b"quit\n");
        }
        let start = Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if start.elapsed() > TIMEOUT {
                let _ = self.child.kill();
                panic!("program did not exit after quit");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        std::thread::sleep(Duration::from_millis(100));
        let out = self.stdout.lock().unwrap().clone();
        let err = self.stderr.lock().unwrap().clone();
        (status.code(), out, err)
    }
}

#[test]
fn osc_command_output_reaches_console() {
    let dir = scratch("basic");
    let s = Session::start(&dir);
    s.osc_expect("master 0.7", "master volume set to 0.7");
    let (code, _, _) = s.finish();
    assert_eq!(code, Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn osc_plugin_show_runs_on_console_thread() {
    // Before the console routing, OSC ran on its own thread and `plugin show` was refused
    // with "must be used from the console or a script".
    let dir = scratch("show");
    let s = Session::start(&dir);
    s.osc_expect("plugin show 99", "plugin 99 not found");
    assert!(!s.output().contains("must be used from the console"), "{}", s.output());
    let _ = s.finish();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn osc_quit_is_ignored_and_does_not_end_program() {
    let dir = scratch("quit");
    let s = Session::start(&dir);
    s.osc_expect("master 0.1", "master volume set to 0.1");
    s.osc_send("quit");
    s.osc_expect("master 0.4", "master volume set to 0.4");
    assert!(s.output().contains("'quit' is ignored over OSC"), "{}", s.output());
    let (code, _, _) = s.finish();
    assert_eq!(code, Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn osc_midi_listen_is_skipped() {
    let dir = scratch("midi");
    let s = Session::start(&dir);
    s.osc_expect("midi listen 0", "interactive only");
    let (code, _, _) = s.finish();
    assert_eq!(code, Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn osc_load_runs_script_from_config_dir() {
    let dir = scratch("load");
    std::fs::write(dir.join("child.spaph"), "master 0.6\n").unwrap();
    let s = Session::start(&dir);
    s.osc_expect("load child.spaph", "master volume set to 0.6");
    let (code, _, _) = s.finish();
    assert_eq!(code, Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn osc_unload_all_runs_on_console() {
    let dir = scratch("unload");
    let s = Session::start(&dir);
    s.osc_expect("plugin unload all", "no plugins to unload");
    let (code, _, _) = s.finish();
    assert_eq!(code, Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn osc_alias_reaches_the_same_resolver() {
    // An unknown alias sent over OSC is resolved on the console thread like a typed one.
    let dir = scratch("alias");
    let s = Session::start(&dir);
    s.osc_expect("plugin show nosuch", "invalid plugin id or alias");
    let (code, _, _) = s.finish();
    assert_eq!(code, Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}
