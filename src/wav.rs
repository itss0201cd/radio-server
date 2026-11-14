use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

pub fn is_wav(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("wav"))
        .unwrap_or(false)
}

pub fn is_flac(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("flac"))
        .unwrap_or(false)
}

pub fn is_media(path: &Path) -> bool {
    is_wav(path) || is_flac(path)
}

#[derive(Clone, Copy, Debug)]
pub struct WavFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub is_float: bool,
    pub sample_bytes: usize,
    pub block_align: usize,
    pub data_offset: u64,
    pub data_len: u64,
}

fn le16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn read_four(f: &mut File) -> Result<[u8; 4], String> {
    let mut b = [0u8; 4];
    f.read_exact(&mut b).map_err(|e| e.to_string())?;
    Ok(b)
}

pub fn wav_format(path: &Path) -> Result<WavFormat, String> {
    let mut f = File::open(path).map_err(|e| format!("open: {e}"))?;
    let file_len = f.metadata().map_err(|e| e.to_string())?.len();

    if &read_four(&mut f)? != b"RIFF" {
        return Err("not RIFF".into());
    }
    let _ = read_four(&mut f)?;
    if &read_four(&mut f)? != b"WAVE" {
        return Err("not WAVE".into());
    }

    let mut parsed: Option<WavFormat> = None;
    let mut data_offset: Option<u64> = None;
    let mut data_len: u64 = 0;

    loop {
        let mut id = [0u8; 4];
        match f.read_exact(&mut id) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.to_string()),
        }

        let size = le32(&read_four(&mut f)?) as u64;
        let start = f.stream_position().map_err(|e| e.to_string())?;

        if &id == b"fmt " {
            if !(16..=4096).contains(&size) {
                return Err("bad fmt chunk".into());
            }

            let mut buf = vec![0u8; size as usize];
            f.read_exact(&mut buf).map_err(|e| e.to_string())?;

            let mut tag = le16(&buf[0..2]);
            let channels = le16(&buf[2..4]);
            let sample_rate = le32(&buf[4..8]);
            let block_align = le16(&buf[12..14]);
            let mut bits = le16(&buf[14..16]);
            let mut is_float = tag == 3;

            if tag == 0xfffe && buf.len() >= 40 {
                let valid_bits = le16(&buf[18..20]);
                if valid_bits != 0 {
                    bits = valid_bits;
                }

                // WAVEFORMATEXTENSIBLE SubFormat GUID starts at offset 24.
                tag = le16(&buf[24..26]);
                is_float = tag == 3;
            }

            if channels == 0 || sample_rate == 0 {
                return Err("bad wav format".into());
            }

            let mut sample_bytes = if block_align != 0 {
                block_align as usize / channels as usize
            } else {
                0
            };

            if sample_bytes == 0 {
                sample_bytes = match bits {
                    8 => 1,
                    16 => 2,
                    24 => 3,
                    32 => 4,
                    _ => 0,
                };
            }

            if sample_bytes == 0 || sample_bytes > 4 {
                return Err(format!(
                    "unsupported wav sample size: bits={bits} bytes={sample_bytes}"
                ));
            }

            if is_float {
                if sample_bytes != 4 {
                    return Err("only 32-bit float wav is supported".into());
                }
            } else {
                match bits {
                    8 | 16 | 24 | 32 => {}
                    _ => return Err(format!("unsupported wav bit depth: {bits}")),
                }
            }

            parsed = Some(WavFormat {
                sample_rate,
                channels,
                bits_per_sample: bits,
                is_float,
                sample_bytes,
                block_align: sample_bytes * channels as usize,
                data_offset: 0,
                data_len: 0,
            });
        } else if &id == b"data" {
            data_offset = Some(start);
            data_len = if size == 0 {
                file_len.saturating_sub(start)
            } else {
                size
            };
            break;
        }

        let next = start
            .checked_add(size)
            .and_then(|n| n.checked_add(size & 1))
            .ok_or("wav chunk overflow")?;

        f.seek(SeekFrom::Start(next)).map_err(|e| e.to_string())?;
    }

    let mut fmt = parsed.ok_or("no fmt chunk")?;
    let off = data_offset.ok_or("no data chunk")?;
    fmt.data_offset = off;
    fmt.data_len = data_len.min(file_len.saturating_sub(off));
    Ok(fmt)
}

pub fn wav_format_label(fmt: &WavFormat) -> String {
    let depth = if fmt.is_float {
        format!("{}-bit float", fmt.bits_per_sample)
    } else {
        format!("{}-bit", fmt.bits_per_sample)
    };

    format!(
        "WAV · {} kHz · {} · {} ch",
        fmt.sample_rate as f64 / 1000.0,
        depth,
        fmt.channels
    )
}

fn decode_sample(b: &[u8], fmt: &WavFormat) -> f32 {
    if fmt.is_float {
        if b.len() >= 4 {
            let x = f32::from_bits(le32(b));
            if x.is_finite() {
                x.clamp(-1.0, 1.0)
            } else {
                0.0
            }
        } else {
            0.0
        }
    } else if fmt.sample_bytes == 1 {
        (b[0] as f32 - 128.0) / 128.0
    } else if fmt.sample_bytes == 2 {
        i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0
    } else if fmt.sample_bytes == 3 {
        let v = (b[0] as i32) | ((b[1] as i32) << 8) | ((b[2] as i32) << 16);
        let v = if v & 0x0080_0000 != 0 {
            v | !0x00ff_ffff
        } else {
            v
        };
        v as f32 / 8_388_608.0
    } else if fmt.sample_bytes == 4 {
        i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0
    } else {
        0.0
    }
}

pub struct WavStream {
    f: File,
    fmt: WavFormat,
    remaining_frames: usize,
}

impl WavStream {
    pub fn open(path: &Path) -> Result<Self, String> {
        let fmt = wav_format(path)?;
        let mut f = File::open(path).map_err(|e| format!("open: {e}"))?;
        f.seek(SeekFrom::Start(fmt.data_offset))
            .map_err(|e| e.to_string())?;

        let remaining_frames = if fmt.block_align == 0 {
            0
        } else {
            (fmt.data_len / fmt.block_align as u64) as usize
        };

        Ok(WavStream {
            f,
            fmt,
            remaining_frames,
        })
    }

    pub fn format(&self) -> WavFormat {
        self.fmt
    }

    pub fn read_stereo_frames(&mut self, max_frames: usize) -> Result<Vec<f32>, String> {
        let frames = self.remaining_frames.min(max_frames);
        if frames == 0 {
            return Ok(Vec::new());
        }

        let need = frames * self.fmt.block_align;
        let mut buf = vec![0u8; need];
        let mut filled = 0usize;

        while filled < need {
            match self.f.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.to_string()),
            }
        }

        let complete = filled / self.fmt.block_align;
        if complete == 0 {
            return Err("unexpected end of wav data".into());
        }

        buf.truncate(complete * self.fmt.block_align);
        self.remaining_frames = self.remaining_frames.saturating_sub(complete);
        if filled < need {
            self.remaining_frames = 0;
        }

        let mut out = Vec::with_capacity(complete * 2);
        let sb = self.fmt.sample_bytes;
        let ch = self.fmt.channels as usize;

        for frame in buf.chunks(self.fmt.block_align) {
            let l = decode_sample(&frame[..sb], &self.fmt);
            let r = if ch > 1 {
                decode_sample(&frame[sb..2 * sb], &self.fmt)
            } else {
                l
            };
            out.push(l);
            out.push(r);
        }

        Ok(out)
    }
}
