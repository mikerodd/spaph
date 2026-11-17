//! OSC remote control for spaph.

use rosc::{OscPacket, OscType};
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub fn run_server<F>(bind: &str, on_command: F, stop: Arc<AtomicBool>) -> Result<(), Box<dyn std::error::Error>>
where
    F: Fn(String) + Send + Sync + 'static,
{
    let socket = UdpSocket::bind(bind)?;
    socket.set_nonblocking(true)?;
    let mut buf = vec![0u8; 65536];
    while !stop.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((size, _src)) => {
                if let Ok((_, packet)) = rosc::decoder::decode_udp(&buf[..size]) {
                    handle_packet(&packet, &on_command);
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                tracing::warn!("OSC recv error: {}", e);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

pub fn extract_command(packet: &OscPacket) -> Option<String> {
    match packet {
        OscPacket::Message(msg) => {
            if msg.addr == "/spaph/command" {
                if let Some(OscType::String(cmd)) = msg.args.first() {
                    return Some(cmd.clone());
                }
            }
            None
        }
        OscPacket::Bundle(bundle) => {
            bundle.content.iter().find_map(extract_command)
        }
    }
}

fn handle_packet<F>(packet: &OscPacket, on_command: &F)
where
    F: Fn(String) + Send + Sync + 'static,
{
    match packet {
        OscPacket::Message(_) => {
            if let Some(cmd) = extract_command(packet) {
                tracing::debug!("OSC command: {}", cmd);
                on_command(cmd);
            }
        }
        OscPacket::Bundle(bundle) => {
            for p in &bundle.content {
                handle_packet(p, on_command);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rosc::{OscMessage, OscPacket, OscType};

    #[test]
    fn extract_command_from_message() {
        let pkt = OscPacket::Message(OscMessage {
            addr: "/spaph/command".to_string(),
            args: vec![OscType::String("load /tmp/x.vst3".to_string())],
        });
        assert_eq!(extract_command(&pkt), Some("load /tmp/x.vst3".to_string()));
    }

    #[test]
    fn extract_command_wrong_address() {
        let pkt = OscPacket::Message(OscMessage {
            addr: "/foo".to_string(),
            args: vec![OscType::String("bar".to_string())],
        });
        assert_eq!(extract_command(&pkt), None);
    }

    #[test]
    fn extract_command_from_bundle() {
        let pkt = OscPacket::Bundle(rosc::OscBundle {
            timetag: rosc::OscTime { seconds: 0, fractional: 1 },
            content: vec![OscPacket::Message(OscMessage {
                addr: "/spaph/command".to_string(),
                args: vec![OscType::String("quit".to_string())],
            })],
        });
        assert_eq!(extract_command(&pkt), Some("quit".to_string()));
    }
}
