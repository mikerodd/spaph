# Plan: multiple plugins playing audio simultaneously

Status: planned, not yet implemented. Single-plugin audio is current.

## Architecture

Single global audio stream with a mixer.

- One cpal output stream, created on first `audio connect`, stopped when no
  plugins are armed.
- Active plugins stored in `Arc<Mutex<Vec<ActivePlugin>>>` shared with the audio
  callback.
- Per active plugin: instance, gain, midi_queue, reusable scratch buffers.
- Audio callback: for each active plugin, drain its MIDI queue, process into
  scratch, apply per-plugin gain, then mix (sum) into the device output buffer
  with channel mapping (`scratch[c % plugin_outputs]`).
- Master gain applied after mixing.

## Commands

- `audio connect <plugin_id>` — arms a plugin and auto-starts the stream.
- `audio disconnect [plugin_id]` — disarms one (or all if no id); stops the
  stream when empty.
- `volume <plugin_id> <level>` — per-plugin gain (already exists).
- `master <level>` — new: global output gain.
- `audio list` — unchanged.
- `disconnect` — disarms all, stops the stream, disconnects MIDI.
- Sample rate / buffer size / device: global only (startup flags + config).

## Anticipated issues

1. CPU load: N plugins processed per block.
2. Plugin channel mismatch: mixed via modulo mapping (mono to stereo
   duplication, extras ignored). No automatic downmix.
3. No audio input: plugins receive silence.
4. VST2/VST3 `set_sample_rate` / `set_block_size` called at arm time, not per
   block.
5. Lock contention: audio callback locks the active list and each plugin
   instance; CLI param changes contend.
6. Unloading an armed plugin must disarm first.
7. Thread safety: active list is `Arc<Mutex<...>>`; audio handle stored globally
   with unsafe Send/Sync (existing pattern).
8. VST2 process block size set to stream buffer size (not capped).

## Tests

- Unit: arm/disarm updates the active list; mix of two mock plugins produces
  summed output; master gain applied; disarm-last stops the stream.
- Integration (`#[ignore]`): arm two plugins, verify both produce audio without
  a crash.

## Open questions

1. Should `audio connect` auto-start the stream (recommended) or require
   `audio start`?
2. Should `master <level>` be added, or reuse `volume` without an id for master?
3. Should arming an already-armed plugin be a no-op (recommended) or an error?
