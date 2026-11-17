# spaph — Simple Portable Audio Plugins Host

GPLv3 CLI host for VST2/VST3 audio plugins, with OSC remote control and per-plugin volume.

## Build requirements

- Rust (edition 2024) via rustup
- Linux: `libasound2-dev`, `libxcb1-dev` (required); `libjack-jackd2-dev` (optional, for JACK); `libpipewire-0.3-dev` + `clang` (optional, for native PipeWire backend)
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

ASIO (Windows, optional, manual build):
```sh
cargo build --release --features spaph-audio/asio
```
(requires the Steinberg ASIO SDK; download from https://www.steinberg.net/asiosdk and set `ASIO_SDK_PATH`).

## Run

```sh
./target/release/spaph                 # uses ./spaph.json if present
./target/release/spaph --config rig.json
```

Command-line options override the configuration file:
`--config`, `--sample-rate`, `--buffer-size`, `--audio-device`, `--log-level`, `--osc-port`.

## Configuration file (`spaph.json`)

Every field is optional; unset fields use built-in defaults.

```json
{
  "audio": { "sample_rate": 48000, "buffer_size": 256, "device_index": 0 },
  "osc": { "port": 9000 },
  "log_level": "info",
  "script": "startup.spaph"
}
```

Unknown keys are an error. Precedence: command line > file > default.

## Startup script

The script is a text file with one prompt command per line. Blank lines and lines
starting with `#` are ignored. A relative `script` path is relative to the config file.

```
# startup.spaph
load /usr/lib/vst/amsynth_vst.so
select 1 5
connect audio 1 0 48000 256
```

- If a command fails, spaph prints the line and exits with code 1.
- `quit` ends the script; the program keeps running and opens the prompt.
- `midi listen` is interactive only: it is skipped with a warning in a script.

## License

GPLv3 or later.
