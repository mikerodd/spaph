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

The audio stream starts at launch (silence when no plugin is connected), so the
host appears as a node named `spaph` in PipeWire/Helvum immediately.


```sh
./target/release/spaph                 # uses ./spaph.json if present
./target/release/spaph --config rig.json
```

Command-line options override the configuration file:
`--config`, `--sample-rate`, `--buffer-size`, `--audio-device`, `--audio-backend`, `--log-level`, `--osc-port`.

## Audio backends

`backend list` shows the backends available on the platform. On Linux the
default is PulseAudio compatibility, which connects to PipeWire; with
`--features spaph-audio/pipewire` the native PipeWire backend is used instead.

On Windows the default is WASAPI. Set `audio.backend` (or `--audio-backend asio`)
to use ASIO; it is reported as available only when a driver such as ASIO4ALL is
installed. The name must match a backend listed by `backend list`.

## Configuration file (`spaph.json`)

Every field is optional; unset fields use built-in defaults.

```json
{
  "audio": { "sample_rate": 48000, "buffer_size": 256, "device_index": 0, "backend": "wasapi" },
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
plugin load /usr/lib/vst/amsynth_vst.so
select 1 5
connect audio 1 0 48000 256
```

- If a command fails, spaph prints the line and exits with code 1.
- `quit` ends the script; the program keeps running and opens the prompt.
- `midi listen` is interactive only: it is skipped with a warning in a script.
- `load <script> [name=value ...]` runs another script, path relative to the config file. It nests
  up to 8 levels. See "Script parameters" below.
- `plugin unload all` unloads every plugin, disconnecting its audio and MIDI.
- `plugin connect <src> <dst>` feeds the output of plugin `src` into the audio input of plugin
  `dst` (mono and stereo; stereo to mono is summed at -3 dB). `plugin disconnect <dst>` removes
  that input. Connections are explicit only: nothing is connected implicitly. A plugin is heard
  on the output only if it is connected with `audio connect`, so a chain ending in an effect
  needs `audio connect <effect> <device>`. A plugin can feed several targets, but each target has
  one source. Cycles are rejected. Unloading a plugin removes its links.
- `plugin load <path> as <name>` gives a plugin an alias. Any command that takes a plugin id
  also accepts the alias, e.g. `midi connect 0 synth`. Aliases are case-sensitive, must start
  with a letter or `_`, may contain letters, digits, `_` and `-`, and cannot be `all`. An alias
  is released when its plugin is unloaded, so it can be reused. Numeric ids keep working.

### Script parameters

A script can take parameters, so one script can load the same plugin several times with a
different alias, MIDI channel or audio device. The script declares its parameters with `# param`
lines and uses them as `$name`. The caller passes values with `load`, as `name=value` pairs
(no positional arguments and no `--` flags):

```
# scripts/multiple/strum.spaph
# param alias
# param device
# param midi=
plugin load "c:\program files\vstplugins\strum GS-2.dll" as $alias
midi connect 0 $alias $midi
audio connect $alias $device
```

```
load scripts/multiple/strum.spaph alias=strum1 device='Scarlett Solo USB'
load scripts/multiple/strum.spaph alias=strum2 midi=2 device='Scarlett Solo USB'
```

- `# param name` is required. `# param name=` is optional and empty when not given.
  `# param name=value` is optional with `value` as its default.
- Quote a value that contains spaces with `'` or `"`. The quotes are not part of the value.
- An empty optional value disappears from its line. So `midi connect 0 $alias $midi` connects all
  MIDI channels when `midi=` is not given, and channel 2 for `midi=2`. Channels are numbered 1 to 16.
- `audio connect` takes a device index from `audio list`, or part of a device name, which must
  match exactly one device (case-insensitive).
- A missing required parameter, an unknown or repeated parameter, a `$name` the script does not
  declare, or parameters for a script without `# param` lines is an error. The error is reported
  before any command of the script runs.
- A script without `# param` lines is run as before, and its `$` characters are not changed.
- Parameters are not passed on to nested `load` calls; pass them again if needed.
- `$name` is substituted only in script files, never in OSC messages.

## OSC

Commands sent to `/spaph/command` (string argument) run on the console thread, so
`plugin show` / `hide` work over OSC. Output is printed to the console. `quit` is ignored
over OSC, and `midi listen` is skipped.

## License

GPLv3 or later.
