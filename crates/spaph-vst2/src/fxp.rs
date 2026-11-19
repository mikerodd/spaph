//! VST 2 program files (`.fxp`), following `vstfxstore.h` from the VST 2.4 SDK.
//!
//! Layout (all integers and floats are big-endian):
//!
//! ```text
//! 0   'CcnK'          chunk magic
//! 4   byteSize        file length - 8
//! 8   fxMagic         'FxCk' (float parameters) or 'FPCh' (opaque chunk)
//! 12  version         1
//! 16  fxID            plugin unique id
//! 20  fxVersion       plugin version
//! 24  numParams       number of float parameters ('FxCk'); 0 for chunks
//! 28  prgName         28 bytes, NUL padded
//! 56  content         'FxCk': numParams * f32
//!                     'FPCh': u32 chunk size, then the chunk bytes
//! ```

const CHUNK_MAGIC: &[u8; 4] = b"CcnK";
const PARAMS_MAGIC: &[u8; 4] = b"FxCk";
const CHUNK_PRESET_MAGIC: &[u8; 4] = b"FPCh";
const FORMAT_VERSION: u32 = 1;
const NAME_LEN: usize = 28;
const HEADER_LEN: usize = 56;

/// Program body: either the plugin's float parameters or its opaque chunk.
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    Params(Vec<f32>),
    Chunk(Vec<u8>),
}

/// A single VST 2 program.
#[derive(Debug, Clone, PartialEq)]
pub struct Fxp {
    pub fx_id: i32,
    pub fx_version: i32,
    pub name: String,
    pub body: Body,
}

/// Serialize a program to `.fxp` bytes.
pub fn encode(p: &Fxp) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(CHUNK_MAGIC);
    out.extend_from_slice(&[0; 4]); // byteSize, patched below
    let fx_magic = match p.body {
        Body::Params(_) => PARAMS_MAGIC,
        Body::Chunk(_) => CHUNK_PRESET_MAGIC,
    };
    out.extend_from_slice(fx_magic);
    out.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
    out.extend_from_slice(&p.fx_id.to_be_bytes());
    out.extend_from_slice(&p.fx_version.to_be_bytes());
    let num_params = match &p.body {
        Body::Params(v) => v.len() as u32,
        Body::Chunk(_) => 0,
    };
    out.extend_from_slice(&num_params.to_be_bytes());
    out.extend_from_slice(&name_field(&p.name));
    match &p.body {
        Body::Params(values) => {
            for v in values {
                out.extend_from_slice(&v.to_be_bytes());
            }
        }
        Body::Chunk(data) => {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(data);
        }
    }
    let byte_size = (out.len() - 8) as u32;
    out[4..8].copy_from_slice(&byte_size.to_be_bytes());
    out
}

/// Parse `.fxp` bytes. Rejects anything that is not a well-formed single program.
pub fn decode(bytes: &[u8]) -> Result<Fxp, String> {
    if bytes.len() < HEADER_LEN {
        return Err("file too short for an .fxp header".into());
    }
    if &bytes[0..4] != CHUNK_MAGIC {
        return Err("not a VST preset file (missing 'CcnK' magic)".into());
    }
    let byte_size = be_u32(bytes, 4) as usize;
    if byte_size + 8 != bytes.len() {
        return Err(format!(
            "size mismatch: header says {} bytes, file has {}",
            byte_size + 8,
            bytes.len()
        ));
    }
    let fx_id = be_u32(bytes, 16) as i32;
    let fx_version = be_u32(bytes, 20) as i32;
    let num_params = be_u32(bytes, 24) as usize;
    let name = name_from_field(&bytes[28..HEADER_LEN]);
    let content = &bytes[HEADER_LEN..];

    let body = match &bytes[8..12] {
        m if m == PARAMS_MAGIC => {
            if content.len() != num_params * 4 {
                return Err(format!(
                    "parameter block has {} bytes, expected {} for {} parameters",
                    content.len(),
                    num_params * 4,
                    num_params
                ));
            }
            Body::Params(
                content
                    .chunks_exact(4)
                    .map(|c| f32::from_be_bytes([c[0], c[1], c[2], c[3]]))
                    .collect(),
            )
        }
        m if m == CHUNK_PRESET_MAGIC => {
            if content.len() < 4 {
                return Err("chunk preset is missing its size field".into());
            }
            let size = be_u32(content, 0) as usize;
            if content.len() - 4 != size {
                return Err(format!(
                    "chunk size mismatch: header says {} bytes, file has {}",
                    size,
                    content.len() - 4
                ));
            }
            Body::Chunk(content[4..].to_vec())
        }
        m => {
            return Err(format!(
                "unsupported preset kind '{}' (only regular programs are supported)",
                String::from_utf8_lossy(m)
            ))
        }
    };

    Ok(Fxp { fx_id, fx_version, name, body })
}

fn be_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Fixed 28-byte NUL-padded name. Longer names are truncated; a NUL is always kept.
fn name_field(name: &str) -> [u8; NAME_LEN] {
    let mut field = [0u8; NAME_LEN];
    let bytes = name.as_bytes();
    let n = bytes.len().min(NAME_LEN - 1);
    field[..n].copy_from_slice(&bytes[..n]);
    field
}

fn name_from_field(field: &[u8]) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(values: Vec<f32>) -> Fxp {
        Fxp { fx_id: 0x5678_1234, fx_version: 3, name: "Init".into(), body: Body::Params(values) }
    }

    #[test]
    fn params_round_trip() {
        let p = params(vec![0.0, 0.5, 1.0]);
        assert_eq!(decode(&encode(&p)).unwrap(), p);
    }

    #[test]
    fn chunk_round_trip_keeps_bytes_unchanged() {
        let p = Fxp {
            fx_id: 42,
            fx_version: 1,
            name: "Bass".into(),
            body: Body::Chunk(vec![0x00, 0xff, 0x12, 0x34, 0x56]),
        };
        assert_eq!(decode(&encode(&p)).unwrap(), p);
    }

    #[test]
    fn params_bytes_match_sdk_layout() {
        // Hand-built from vstfxstore.h, independent of `encode`.
        let mut expected = Vec::new();
        expected.extend_from_slice(b"CcnK");
        expected.extend_from_slice(&52u32.to_be_bytes()); // 56 + 4 - 8
        expected.extend_from_slice(b"FxCk");
        expected.extend_from_slice(&1u32.to_be_bytes());
        expected.extend_from_slice(&7i32.to_be_bytes());
        expected.extend_from_slice(&2i32.to_be_bytes());
        expected.extend_from_slice(&1u32.to_be_bytes());
        let mut name = [0u8; 28];
        name[..3].copy_from_slice(b"Abc");
        expected.extend_from_slice(&name);
        expected.extend_from_slice(&0.5f32.to_be_bytes());

        let p = Fxp { fx_id: 7, fx_version: 2, name: "Abc".into(), body: Body::Params(vec![0.5]) };
        assert_eq!(encode(&p), expected);
    }

    #[test]
    fn chunk_bytes_match_sdk_layout() {
        let p = Fxp { fx_id: 9, fx_version: 0, name: String::new(), body: Body::Chunk(vec![1, 2]) };
        let bytes = encode(&p);
        assert_eq!(&bytes[0..4], b"CcnK");
        assert_eq!(be_u32(&bytes, 4) as usize, bytes.len() - 8);
        assert_eq!(&bytes[8..12], b"FPCh");
        assert_eq!(be_u32(&bytes, 24), 0);
        assert_eq!(be_u32(&bytes, 56), 2);
        assert_eq!(&bytes[60..], &[1, 2]);
    }

    #[test]
    fn long_name_is_truncated_and_nul_terminated() {
        let p = params(vec![]);
        let p = Fxp { name: "x".repeat(60), ..p };
        let back = decode(&encode(&p)).unwrap();
        assert_eq!(back.name.len(), NAME_LEN - 1);
    }

    #[test]
    fn rejects_short_input() {
        assert!(decode(&[0u8; 10]).is_err());
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = encode(&params(vec![0.1]));
        bytes[0] = b'X';
        assert!(decode(&bytes).unwrap_err().contains("CcnK"));
    }

    #[test]
    fn rejects_truncated_file() {
        let bytes = encode(&params(vec![0.1, 0.2]));
        assert!(decode(&bytes[..bytes.len() - 1]).unwrap_err().contains("size mismatch"));
    }

    #[test]
    fn rejects_parameter_count_mismatch() {
        let mut bytes = encode(&params(vec![0.1, 0.2]));
        bytes[24..28].copy_from_slice(&5u32.to_be_bytes());
        assert!(decode(&bytes).unwrap_err().contains("parameter block"));
    }

    #[test]
    fn rejects_chunk_size_mismatch() {
        let p = Fxp { fx_id: 1, fx_version: 1, name: String::new(), body: Body::Chunk(vec![1, 2, 3]) };
        let mut bytes = encode(&p);
        bytes[56..60].copy_from_slice(&9u32.to_be_bytes());
        assert!(decode(&bytes).unwrap_err().contains("chunk size mismatch"));
    }

    #[test]
    fn rejects_bank_files() {
        let mut bytes = encode(&params(vec![0.1]));
        bytes[8..12].copy_from_slice(b"FxBk");
        assert!(decode(&bytes).unwrap_err().contains("unsupported preset kind"));
    }
}
