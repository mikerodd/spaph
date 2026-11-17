//! MIDI input/output support for spaph, built on `midir`.

use midir::MidiInput;
use spaph_types::{Result, SpaphError};
use std::sync::mpsc::{self, Receiver, Sender};

pub use spaph_types::MidiEvent;

pub fn list_input_ports() -> Result<Vec<String>> {
    let input = MidiInput::new("spaph").map_err(|e| SpaphError::Audio(e.to_string()))?;
    let ports = input.ports();
    let mut names = Vec::with_capacity(ports.len());
    for p in ports.iter() {
        let name = input.port_name(p).unwrap_or_else(|_| "<unknown>".into());
        names.push(name);
    }
    Ok(names)
}

pub fn list_output_ports() -> Result<Vec<String>> {
    let output = midir::MidiOutput::new("spaph").map_err(|e| SpaphError::Audio(e.to_string()))?;
    let ports = output.ports();
    let mut names = Vec::with_capacity(ports.len());
    for p in ports.iter() {
        let name = output.port_name(p).unwrap_or_else(|_| "<unknown>".into());
        names.push(name);
    }
    Ok(names)
}

pub struct MidiListener {
    pub rx: Receiver<MidiEvent>,
    _conn: midir::MidiInputConnection<Sender<MidiEvent>>,
}

impl MidiListener {
    pub fn recv_timeout(&self, dur: std::time::Duration) -> Option<MidiEvent> {
        self.rx.recv_timeout(dur).ok()
    }
}

pub fn open_input(index: usize) -> Result<MidiListener> {
    let input = MidiInput::new("spaph").map_err(|e| SpaphError::Audio(e.to_string()))?;
    let ports = input.ports();
    let port = ports
        .get(index)
        .ok_or_else(|| SpaphError::Audio(format!("no midi input port at index {}", index)))?;
    let (tx, rx) = mpsc::channel::<MidiEvent>();
    let conn = input
        .connect(port, "spaph-in", move |timestamp, message, sender| {
            // trace
            if !message.is_empty() {
                let st = message[0];
                let ch = (st & 0x0F) + 1;
                let kind = match st & 0xF0 {
                    0x90 => if message.get(2).copied().unwrap_or(0) == 0 { "NoteOff" } else { "NoteOn " },
                    0x80 => "NoteOff",
                    0xB0 => "CC     ",
                    0xC0 => "ProgChg",
                    0xE0 => "PitchB ",
                    _ => "Other  ",
                };
                let note = message.get(1).copied().unwrap_or(0);
                tracing::debug!("MIDI IN: {} ch={} note={} data={:?}", kind, ch, note, message);
            }
            let _ = sender.send(MidiEvent {
                timestamp,
                data: message.to_vec(),
            });
        }, tx.clone())
        .map_err(|e| SpaphError::Audio(e.to_string()))?;
    Ok(MidiListener {
        rx,
        _conn: conn,
    })
}

pub fn open_input_by_name(name: &str) -> Result<MidiListener> {
    let input = MidiInput::new("spaph").map_err(|e| SpaphError::Audio(e.to_string()))?;
    let ports = input.ports();
    let mut found = None;
    for p in ports.iter() {
        if let Ok(pname) = input.port_name(p) {
            if pname == name {
                found = Some(p.clone());
                break;
            }
        }
    }
    let port = found.ok_or_else(|| SpaphError::Audio(format!("midi input port not found: {}", name)))?;
    let (tx, rx) = mpsc::channel::<MidiEvent>();
    let conn = input
        .connect(&port, "spaph-in", move |timestamp, message, sender| {
            if !message.is_empty() {
                let st = message[0];
                let ch = (st & 0x0F) + 1;
                let kind = match st & 0xF0 {
                    0x90 => if message.get(2).copied().unwrap_or(0) == 0 { "NoteOff" } else { "NoteOn " },
                    0x80 => "NoteOff",
                    0xB0 => "CC     ",
                    0xC0 => "ProgChg",
                    0xE0 => "PitchB ",
                    _ => "Other  ",
                };
                let note = message.get(1).copied().unwrap_or(0);
                tracing::debug!("MIDI IN: {} ch={} note={} data={:?}", kind, ch, note, message);
            }
            let _ = sender.send(MidiEvent {
                timestamp,
                data: message.to_vec(),
            });
        }, tx.clone())
        .map_err(|e| SpaphError::Audio(e.to_string()))?;
    Ok(MidiListener {
        rx,
        _conn: conn,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midi_event_debug() {
        let ev = MidiEvent { timestamp: 1, data: vec![0x90, 60, 100] };
        let s = format!("{:?}", ev);
        assert!(s.contains("144"));
    }
    #[test]
    #[ignore]
    fn list_input_ports_does_not_panic() {
        let ports = list_input_ports();
        assert!(ports.is_ok());
    }
}
