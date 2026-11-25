//! Decoding and resampling of a backing track into interleaved stereo at the stream rate.

use std::fs::File;
use std::path::Path;

use rubato::{Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction};
use spaph_types::{Result, SpaphError};
use symphonia::core::audio::{AudioBufferRef, SampleBuffer};
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

/// Frames per resampler call. Decoded packets are queued until a full chunk is available.
const CHUNK_FRAMES: usize = 2048;

pub struct Source {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    resampler: Option<SincFixedIn<f32>>,
    /// Left and right channel samples at the file's rate, waiting to be resampled.
    pending: [Vec<f32>; 2],
    end_of_stream: bool,
    /// Output frames still to drop: the resampler's filter delay, which is silence.
    skip: usize,
    /// Input frames given to the resampler, and output frames emitted, since the last seek.
    fed: u64,
    emitted: u64,
    /// Output frames per input frame.
    ratio: f64,
}

fn resampler_skip(resampler: Option<&SincFixedIn<f32>>) -> usize {
    resampler.map(|r| r.output_delay()).unwrap_or(0)
}

fn audio_err<E: std::fmt::Display>(e: E) -> SpaphError {
    SpaphError::Audio(e.to_string())
}

/// Open `path` for playback at `out_rate`. Only stereo files are accepted.
pub fn open(path: &Path, out_rate: u32) -> Result<Source> {
    let file = File::open(path)?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, stream, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(audio_err)?;
    let format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| SpaphError::Audio(format!("{}: no audio track", path.display())))?;
    let track_id = track.id;
    let params = track.codec_params.clone();

    let channels = params.channels.map(|c| c.count()).unwrap_or(0);
    if channels != 2 {
        return Err(SpaphError::Audio(format!(
            "{}: backing tracks must be stereo, this file has {} channel(s)",
            path.display(),
            channels
        )));
    }
    let in_rate = params
        .sample_rate
        .ok_or_else(|| SpaphError::Audio(format!("{}: unknown sample rate", path.display())))?;
    let decoder = symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .map_err(audio_err)?;

    let resampler = if in_rate == out_rate {
        None
    } else {
        let parameters = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 128,
            window: WindowFunction::BlackmanHarris2,
        };
        let ratio = out_rate as f64 / in_rate as f64;
        Some(SincFixedIn::<f32>::new(ratio, 1.0, parameters, CHUNK_FRAMES, 2).map_err(audio_err)?)
    };
    tracing::info!("backing: {} ({} Hz -> {} Hz)", path.display(), in_rate, out_rate);

    let skip = resampler_skip(resampler.as_ref());
    Ok(Source {
        format,
        decoder,
        track_id,
        resampler,
        pending: [Vec::new(), Vec::new()],
        end_of_stream: false,
        skip,
        fed: 0,
        emitted: 0,
        ratio: out_rate as f64 / in_rate as f64,
    })
}

impl Source {
    /// Move to `secs` from the start of the file, dropping anything already decoded.
    pub fn seek(&mut self, secs: f64) -> Result<()> {
        let time = Time::from(secs.max(0.0));
        self.format
            .seek(SeekMode::Accurate, SeekTo::Time { time, track_id: Some(self.track_id) })
            .map_err(audio_err)?;
        self.decoder.reset();
        if let Some(resampler) = self.resampler.as_mut() {
            resampler.reset();
        }
        self.skip = resampler_skip(self.resampler.as_ref());
        self.fed = 0;
        self.emitted = 0;
        for channel in &mut self.pending {
            channel.clear();
        }
        self.end_of_stream = false;
        Ok(())
    }

    /// The next block of interleaved stereo samples at the output rate, or `None` at the end.
    pub fn next_chunk(&mut self) -> Result<Option<Vec<f32>>> {
        loop {
            if self.pending[0].len() >= CHUNK_FRAMES {
                return self.process(false).map(Some);
            }
            if !self.end_of_stream {
                if !self.decode_packet()? {
                    self.end_of_stream = true;
                }
                continue;
            }
            if self.pending[0].is_empty() {
                return Ok(None);
            }
            return self.process(true).map(Some);
        }
    }

    /// Resample the queued frames (all of them when `last`, otherwise one full chunk).
    fn process(&mut self, last: bool) -> Result<Vec<f32>> {
        let take = if last { self.pending[0].len() } else { CHUNK_FRAMES };
        let input: [Vec<f32>; 2] = [
            self.pending[0].drain(..take).collect(),
            self.pending[1].drain(..take).collect(),
        ];
        self.fed += take as u64;
        let output: Vec<Vec<f32>> = match self.resampler.as_mut() {
            None => Vec::from(input),
            Some(resampler) => {
                let result = if last {
                    resampler.process_partial(Some(&input), None)
                } else {
                    resampler.process(&input, None)
                };
                result.map_err(audio_err)?
            }
        };
        let mut frames = output[0].len().min(output[1].len());
        let start = self.skip.min(frames);
        self.skip -= start;
        if last && self.resampler.is_some() {
            // The resampler pads a short final block to a full chunk. Keep only the frames the
            // input really produces, so the file ends where it should.
            let expected = (self.fed as f64 * self.ratio).round() as u64;
            let left = expected.saturating_sub(self.emitted) as usize + start;
            frames = frames.min(left);
        }
        let mut interleaved = Vec::with_capacity((frames - start) * 2);
        for f in start..frames {
            interleaved.push(output[0][f]);
            interleaved.push(output[1][f]);
        }
        self.emitted += (frames - start) as u64;
        Ok(interleaved)
    }

    /// Decode the next packet of the track into `pending`. `false` at the end of the file.
    fn decode_packet(&mut self) -> Result<bool> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(false);
                }
                Err(SymphoniaError::ResetRequired) => {
                    self.decoder.reset();
                    continue;
                }
                Err(e) => return Err(audio_err(e)),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    queue(&mut self.pending, decoded);
                    return Ok(true);
                }
                Err(SymphoniaError::DecodeError(e)) => {
                    tracing::warn!("backing: skipping undecodable packet: {e}");
                }
                Err(e) => return Err(audio_err(e)),
            }
        }
    }

}

/// Split a decoded packet into the per-channel queues.
fn queue(pending: &mut [Vec<f32>; 2], decoded: AudioBufferRef<'_>) {
    let spec = *decoded.spec();
    let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
    buffer.copy_interleaved_ref(decoded);
    for frame in buffer.samples().chunks_exact(2) {
        pending[0].push(frame[0]);
        pending[1].push(frame[1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a 16-bit stereo WAV with a 440 Hz tone on the left and silence on the right.
    fn write_wav(path: &Path, rate: u32, frames: u32) {
        let data_len = frames * 4;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * 4).to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for n in 0..frames {
            let s = (f32::sin(2.0 * std::f32::consts::PI * 440.0 * n as f32 / rate as f32) * 0.5 * 32767.0) as i16;
            bytes.extend_from_slice(&s.to_le_bytes());
            bytes.extend_from_slice(&0i16.to_le_bytes());
        }
        std::fs::write(path, bytes).unwrap();
    }

    fn drain(source: &mut Source) -> Vec<f32> {
        let mut out = Vec::new();
        while let Some(chunk) = source.next_chunk().unwrap() {
            out.extend(chunk);
        }
        out
    }

    fn temp_wav(name: &str, rate: u32, frames: u32) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("spaph-backing-{}-{}.wav", std::process::id(), name));
        write_wav(&path, rate, frames);
        path
    }

    #[test]
    fn same_rate_passes_every_frame_through() {
        let path = temp_wav("same", 48000, 48000);
        let mut source = open(&path, 48000).unwrap();
        let samples = drain(&mut source);
        assert_eq!(samples.len(), 48000 * 2);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn resampled_length_matches_the_duration() {
        let path = temp_wav("resample", 44100, 44100);
        let mut source = open(&path, 48000).unwrap();
        let frames = drain(&mut source).len() / 2;
        let expected = 48000usize;
        let error = (frames as i64 - expected as i64).unsigned_abs() as usize;
        assert!(error < expected / 100, "got {frames} frames, expected about {expected}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn mono_files_are_rejected() {
        let path = std::env::temp_dir().join(format!("spaph-backing-{}-mono.wav", std::process::id()));
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36u32 + 8).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&44100u32.to_le_bytes());
        bytes.extend_from_slice(&(44100u32 * 2).to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&8u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 8]);
        std::fs::write(&path, bytes).unwrap();
        let err = open(&path, 48000).err().expect("mono must be rejected");
        assert!(err.to_string().contains("stereo"));
        let _ = std::fs::remove_file(path);
    }
}
