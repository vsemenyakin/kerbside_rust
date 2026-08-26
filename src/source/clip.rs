//! A raw frame clip: the byte-exact output of [`crate::source::RoadScene`]
//! written to disk, and read back as a [`FrameSource`].
//!
//! The point is that a `dist` build can *analyse* a clip instead of *generating*
//! the scene -- so the generator (the crown asset that never fell in eight
//! reverse-engineering rounds) need not ship in the analysing binary at all. For
//! the oracle to survive, the frames a clip yields must be **byte-identical** to
//! what `render()` produced; a raw format (no codec) makes that trivially true.
//!
//! Format (little-endian): a 24-byte header
//! `magic[4] "KRW1" | width i32 | height i32 | fps i32 | n_frames i64`
//! followed by `n_frames` frames of `width*height*3` bytes, BGR, row-major --
//! exactly a `CV_8UC3` Mat's data buffer. No compression, no colour conversion:
//! the bytes written are the bytes read.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::sync::Mutex;

use opencv::core::{Mat, Scalar, CV_8UC3};
use opencv::prelude::*;

use crate::source::FrameSource;

const MAGIC: &[u8; 4] = b"KRW1";
const HEADER_LEN: u64 = 24; // 4 + 4 + 4 + 4 + 8

/// Render `n_frames` frames and write them to `path` as a raw clip.
///
/// `render(id)` must return the full-resolution `CV_8UC3` frame for index `id`
/// (i.e. `RoadScene::render(id).0`). The frame must be continuous, which a
/// freshly built scene Mat always is. Introspection-only: dist reads clips, never
/// writes them.
#[cfg(feature = "introspection")]
pub fn write_clip(
    path: &str,
    width: i32,
    height: i32,
    fps: i32,
    n_frames: i64,
    mut render: impl FnMut(i64) -> Result<Mat, String>,
) -> Result<(), String> {
    let mut f = BufWriter::new(
        File::create(path).map_err(|e| format!("{}{path:?}: {e}", "cannot create the clip: "))?,
    );
    let mut hdr = Vec::with_capacity(HEADER_LEN as usize);
    hdr.extend_from_slice(MAGIC);
    hdr.extend_from_slice(&width.to_le_bytes());
    hdr.extend_from_slice(&height.to_le_bytes());
    hdr.extend_from_slice(&fps.to_le_bytes());
    hdr.extend_from_slice(&n_frames.to_le_bytes());
    f.write_all(&hdr)
        .map_err(|e| format!("{}{e}", "cannot write the clip header: "))?;

    let want = (width as usize) * (height as usize) * 3;
    for id in 0..n_frames {
        let mat = render(id)?;
        if !mat.is_continuous() {
            return Err("clip frame is not contiguous".to_string());
        }
        let bytes = mat
            .data_bytes()
            .map_err(|e| format!("{}{e}", "cannot read a frame buffer: "))?;
        if bytes.len() != want {
            return Err(format!("frame {id} has {} bytes, expected {want}", bytes.len()));
        }
        f.write_all(bytes)
            .map_err(|e| format!("{}{e}", "cannot write a clip frame: "))?;
    }
    f.flush()
        .map_err(|e| format!("{}{e}", "cannot flush the clip: "))?;
    Ok(())
}

/// A clip on disk, read one frame at a time (streamed -- only one frame is ever
/// in memory, so a multi-GB clip needs disk, not RAM).
pub struct ClipSource {
    file: Mutex<BufReader<File>>,
    width: i32,
    height: i32,
    frames: i64,
    frame_bytes: usize,
}

impl ClipSource {
    pub fn open(path: &str) -> Result<Self, String> {
        let mut f = File::open(path)
            .map_err(|e| format!("{}{path:?}: {e}", crate::obfstr_err!("cannot open the clip: ")))?;
        let mut hdr = [0u8; HEADER_LEN as usize];
        f.read_exact(&mut hdr)
            .map_err(|e| format!("{}{e}", crate::obfstr_err!("cannot read the clip header: ")))?;
        if &hdr[0..4] != MAGIC {
            return Err(crate::obfstr_err!("not a kerbside clip (bad magic)").to_string());
        }
        let rd_i32 = |o: usize| i32::from_le_bytes(hdr[o..o + 4].try_into().unwrap());
        let width = rd_i32(4);
        let height = rd_i32(8);
        let _fps = rd_i32(12);
        let frames = i64::from_le_bytes(hdr[16..24].try_into().unwrap());
        if width <= 0 || height <= 0 || frames < 0 {
            return Err(crate::obfstr_err!("clip header has invalid dimensions").to_string());
        }
        let frame_bytes = (width as usize) * (height as usize) * 3;
        Ok(Self {
            file: Mutex::new(BufReader::new(f)),
            width,
            height,
            frames,
            frame_bytes,
        })
    }
}

impl FrameSource for ClipSource {
    fn frame_count(&self) -> i64 {
        self.frames
    }

    fn frame(&self, id: i64) -> Result<Mat, String> {
        if id < 0 || id >= self.frames {
            return Err(format!(
                "{}{id}{}",
                crate::obfstr_err!("clip frame "),
                crate::obfstr_err!(" out of range")
            ));
        }
        let offset = HEADER_LEN + (id as u64) * (self.frame_bytes as u64);
        let mut buf = vec![0u8; self.frame_bytes];
        {
            let mut f = self.file.lock().unwrap_or_else(|e| e.into_inner());
            f.seek(SeekFrom::Start(offset))
                .map_err(|e| format!("{}{e}", crate::obfstr_err!("cannot seek the clip: ")))?;
            f.read_exact(&mut buf)
                .map_err(|e| format!("{}{e}", crate::obfstr_err!("cannot read a clip frame: ")))?;
        }
        let mut mat =
            Mat::new_rows_cols_with_default(self.height, self.width, CV_8UC3, Scalar::all(0.0))
                .map_err(|e| format!("{}{e}", crate::obfstr_err!("cannot allocate a clip frame: ")))?;
        mat.data_bytes_mut()
            .map_err(|e| format!("{}{e}", crate::obfstr_err!("cannot fill a clip frame: ")))?
            .copy_from_slice(&buf);
        Ok(mat)
    }
}
