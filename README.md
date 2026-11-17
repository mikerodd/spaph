# spaph — Simple Portable Audio Plugins Host

GPLv3 CLI host for VST2/VST3 audio plugins, with OSC remote control and per-plugin volume.

## Build requirements

- Rust (edition 2024) via rustup
- Linux: `libasound2-dev` (required); `libjack-jackd2-dev` (optional, for JACK); `libpipewire-0.3-dev` + `clang` (optional, for native PipeWire backend)
- Windows: WASAPI is built in; optional ASIO needs the Steinberg ASIO SDK
- macOS: CoreAudio (future)

## Build

```sh
cargo build --release
```

Native PipeWire backend (optional):
```sh
cargo build --release --features spaph-audio/pipewire
```
(requires `libpipewire-0.3-dev` and `clang`).

ASIO (Windows, optional):
```sh
cargo build --release --features spaph-audio/asio
```
(requires the Steinberg ASIO SDK).

## Run

```sh
./target/release/spaph
spaph> help
```

## License

GPLv3 or later.
