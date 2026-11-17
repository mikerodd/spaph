<<<<<<< HEAD
# spaph — Simple Portable Audio Plugins Host

GPLv3 CLI host for VST2/VST3 (and later CLAP) audio plugins, with OSC remote control.

## Build requirements

- Rust (edition 2024) via rustup
- CMake (for JUCE? not needed)
- Linux: `libpipewire-dev`, `libjack-jackd2-dev`, `libasound2-dev`, `clang` (for bindgen), `pkg-config`
- Windows: WASAPI (default); optional ASIO SDK for ASIO support
- macOS: CoreAudio (future)

## Build

```sh
cargo build --release
```

Native PipeWire support is enabled by default and requires `libpipewire-dev` + `clang`.

## Run

```sh
./target/debug/spaph
spaph> help
```
=======
# spaph
the Simple Portable Audio Plugins Host
>>>>>>> origin/main
