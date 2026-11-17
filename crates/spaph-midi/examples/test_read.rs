use midir::{Ignore, MidiInput};
use std::io::{stdin, stdout, Write};
fn main() {
    let mut midi_in = MidiInput::new("test").unwrap();
    midi_in.ignore(Ignore::None);
    let ports = midi_in.ports();
    println!("ports: {}", ports.len());
    for (i,p) in ports.iter().enumerate() { println!("{}: {}", i, midi_in.port_name(p).unwrap()); }
    let in_port = &ports[0];
    let _conn = midi_in.connect(in_port, "test", move |stamp, msg, _| {
        println!("{}: {:?}", stamp, msg);
    }, ()).unwrap();
    println!("listening... press enter");
    let mut input = String::new();
    stdout().flush().unwrap();
    stdin().read_line(&mut input).unwrap();
}
