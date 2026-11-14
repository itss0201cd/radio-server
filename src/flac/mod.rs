#![deny(unsafe_code)]

mod bitreader;
mod crc;
mod error;
mod frame;
mod residual;
mod stereo;
mod subframe;

use std::io::Read;

pub use error::{Error, Result};
pub use frame::ChannelAssignment;

use bitreader::{BitReader, ByteSource};
use crc::crc16;

#[derive(Clone, Debug)]
pub struct StreamInfo {
    pub min_block_size: u16,
    pub max_block_size: u16,
    pub sample_rate: u32,
    pub channels: u8,
    pub bits_per_sample: u8,
    pub total_samples: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct Frame {
    pub start_sample: u64,
    pub block_size: usize,
    pub sample_rate: u32,
    pub bits_per_sample: u8,
    pub channel_assignment: ChannelAssignment,
    /// Channel-major decoded samples.
    pub samples: Vec<i32>,
}

impl Frame {
    pub fn empty() -> Self {
        Self {
            start_sample: 0,
            block_size: 0,
            sample_rate: 0,
            bits_per_sample: 0,
            channel_assignment: ChannelAssignment::Independent(0),
            samples: Vec::new(),
        }
    }

    pub fn channels(&self) -> u8 {
        self.channel_assignment.channels()
    }

    pub fn channel(&self, channel: usize) -> &[i32] {
        let start = channel * self.block_size;
        &self.samples[start..start + self.block_size]
    }
}

pub struct Decoder<R: Read> {
    source: ByteSource<R>,
    info: StreamInfo,
    channel_scratch: Vec<Vec<i64>>,
    blocking: Option<frame::BlockingStrategy>,
    next_sample: u64,
    eof: bool,
}

impl<R: Read> Decoder<R> {
    pub fn new(reader: R) -> Result<Self> {
        let mut source = ByteSource::new(reader);
        let mut magic = [0u8; 4];
        source.read_into(&mut magic)?;
        if &magic != b"fLaC" {
            return error::error("FLAC stream signature missing");
        }

        let mut info = None;
        let mut first_block = true;
        loop {
            let header = source.read_u8()?;
            let last = header & 0x80 != 0;
            let block_type = header & 0x7f;
            if block_type == 127 {
                return error::error("forbidden FLAC metadata block type");
            }
            let length = source.read_be_u24()? as usize;
            if first_block && block_type != 0 {
                return error::error("FLAC stream does not start with STREAMINFO");
            }
            if block_type == 0 {
                if info.is_some() {
                    return error::error("duplicate FLAC STREAMINFO block");
                }
                if length != 34 {
                    return error::error("FLAC STREAMINFO has the wrong length");
                }
                let mut data = [0u8; 34];
                source.read_into(&mut data)?;
                info = Some(parse_streaminfo(&data)?);
            } else {
                source.skip(length)?;
            }
            first_block = false;
            if last {
                break;
            }
        }

        let info = info.ok_or_else(|| Error::new("FLAC stream has no STREAMINFO"))?;
        Ok(Self {
            source,
            info,
            channel_scratch: Vec::new(),
            blocking: None,
            next_sample: 0,
            eof: false,
        })
    }

    pub fn stream_info(&self) -> &StreamInfo {
        &self.info
    }

    /// Decodes the next frame. Returns false at the end of the stream.
    pub fn read_frame(&mut self, frame: &mut Frame) -> Result<bool> {
        if self.eof {
            return Ok(false);
        }
        if let Some(total) = self.info.total_samples
            && self.next_sample >= total
        {
            self.eof = true;
            return Ok(false);
        }

        self.source.start_frame();
        if !self.source.ensure_optional(2)? {
            self.eof = true;
            return Ok(false);
        }
        let header = frame::parse_header(&mut self.source, &self.info)?;
        if let Some(blocking) = self.blocking {
            if blocking != header.blocking {
                return error::error("FLAC blocking strategy changes mid-stream");
            }
        } else {
            self.blocking = Some(header.blocking);
        }
        let channels = header.channel_assignment.channels() as usize;
        let block_size = header.block_size;

        if self.channel_scratch.len() < channels {
            self.channel_scratch.resize_with(channels, Vec::new);
        }
        for samples in &mut self.channel_scratch[..channels] {
            samples.resize(block_size, 0);
        }
        {
            let mut bits = BitReader::new(&mut self.source);
            for (channel, samples) in self.channel_scratch[..channels].iter_mut().enumerate() {
                let side = match header.channel_assignment {
                    ChannelAssignment::LeftSide => channel == 1,
                    ChannelAssignment::RightSide => channel == 0,
                    ChannelAssignment::MidSide => channel == 1,
                    ChannelAssignment::Independent(_) => false,
                };
                let bps = header.bits_per_sample as u32 + u32::from(side);
                subframe::decode(&mut bits, bps, samples)?;
            }
            bits.align_to_byte()?;
        }

        let crc_end = self.source.position();
        let expected_crc = crc16(self.source.crc_bytes(crc_end));
        let stored_crc = self.source.read_be_u16()?;
        if expected_crc != stored_crc {
            return error::error("FLAC frame CRC mismatch");
        }

        stereo::restore(
            header.channel_assignment,
            &mut self.channel_scratch[..channels],
        )?;

        let needed = channels * block_size;
        frame.samples.clear();
        frame.samples.reserve(needed);
        for channel in &self.channel_scratch[..channels] {
            for &sample in channel {
                let sample = i32::try_from(sample)
                    .map_err(|_| Error::new("decoded FLAC sample exceeds 32 bits"))?;
                frame.samples.push(sample);
            }
        }
        frame.start_sample = header.start_sample;
        frame.block_size = block_size;
        frame.sample_rate = header.sample_rate;
        frame.bits_per_sample = header.bits_per_sample;
        frame.channel_assignment = header.channel_assignment;

        let next_sample = header
            .start_sample
            .checked_add(block_size as u64)
            .ok_or_else(|| Error::new("FLAC sample position overflow"))?;
        if let Some(total) = self.info.total_samples
            && next_sample > total
        {
            return error::error("FLAC frame extends past STREAMINFO total samples");
        }
        self.next_sample = next_sample;
        Ok(true)
    }
}

fn parse_streaminfo(data: &[u8; 34]) -> Result<StreamInfo> {
    let min_block_size = u16::from_be_bytes([data[0], data[1]]);
    let max_block_size = u16::from_be_bytes([data[2], data[3]]);
    let packed = u32::from_be_bytes([data[10], data[11], data[12], data[13]]);
    let sample_rate = packed >> 12;
    let channels = ((packed >> 9) & 0x07) as u8 + 1;
    let bits_per_sample = ((packed >> 4) & 0x1f) as u8 + 1;
    let total_high = (packed & 0x0f) as u64;
    let total_low = u32::from_be_bytes([data[14], data[15], data[16], data[17]]) as u64;
    let total_samples = (total_high << 32) | total_low;

    if min_block_size < 16 || max_block_size < 16 || min_block_size > max_block_size {
        return error::error("invalid FLAC STREAMINFO block sizes");
    }
    if !(1..=8).contains(&channels) {
        return error::error("invalid FLAC channel count");
    }
    if !(4..=32).contains(&bits_per_sample) {
        return error::error("invalid FLAC bit depth");
    }

    Ok(StreamInfo {
        min_block_size,
        max_block_size,
        sample_rate,
        channels,
        bits_per_sample,
        total_samples: (total_samples != 0).then_some(total_samples),
    })
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{Decoder, Frame};

    fn temp_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("radio-flac-{name}-{unique}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_wav(path: &Path, rate: u32, bits: u16, channels: u16, frames: usize, mask: i32) {
        let block_align = channels * bits / 8;
        let byte_rate = rate * block_align as u32;
        let mut data = Vec::with_capacity(frames * block_align as usize);
        for i in 0..frames {
            let t = i as f64 / rate as f64;
            let left = (t * 440.0 * std::f64::consts::TAU).sin() * 0.45;
            let right = (t * 660.0 * std::f64::consts::TAU).sin() * 0.35;
            for channel in 0..channels {
                let sample = if channel == 0 { left } else { right };
                match bits {
                    8 => data.extend_from_slice(&[((sample * 127.0 + 128.0) as u8)]),
                    16 => data.extend_from_slice(
                        &(((sample * 32767.0) as i16 as i32 & mask) as i16).to_le_bytes(),
                    ),
                    24 => {
                        let value = (sample * 8_388_607.0) as i32 & mask;
                        data.extend_from_slice(&value.to_le_bytes()[..3]);
                    }
                    32 => {
                        let value = (sample * 2_147_483_647.0) as i32 & mask;
                        data.extend_from_slice(&value.to_le_bytes());
                    }
                    _ => unreachable!(),
                }
            }
        }
        let mut fmt = Vec::new();
        if channels > 2 || bits > 16 {
            fmt.extend_from_slice(&0xfffeu16.to_le_bytes());
            fmt.extend_from_slice(&channels.to_le_bytes());
            fmt.extend_from_slice(&rate.to_le_bytes());
            fmt.extend_from_slice(&byte_rate.to_le_bytes());
            fmt.extend_from_slice(&block_align.to_le_bytes());
            fmt.extend_from_slice(&bits.to_le_bytes());
            fmt.extend_from_slice(&22u16.to_le_bytes());
            fmt.extend_from_slice(&bits.to_le_bytes());
            let channel_mask = match channels {
                1 => 0x4u32,
                2 => 0x3,
                3 => 0x7u32,
                4 => 0x33,
                5 => 0x37,
                6 => 0x3f,
                7 => 0x70f,
                8 => 0x63f,
                _ => (1u32 << channels) - 1,
            };
            fmt.extend_from_slice(&channel_mask.to_le_bytes());
            fmt.extend_from_slice(&[
                0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38,
                0x9b, 0x71,
            ]);
        } else {
            fmt.extend_from_slice(&1u16.to_le_bytes());
            fmt.extend_from_slice(&channels.to_le_bytes());
            fmt.extend_from_slice(&rate.to_le_bytes());
            fmt.extend_from_slice(&byte_rate.to_le_bytes());
            fmt.extend_from_slice(&block_align.to_le_bytes());
            fmt.extend_from_slice(&bits.to_le_bytes());
        }
        let mut wav = Vec::with_capacity(12 + fmt.len() + 8 + data.len());
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&((4 + 8 + fmt.len() + 8 + data.len()) as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        wav.extend_from_slice(&fmt);
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        fs::write(path, wav).unwrap();
    }

    fn encode(wav: &Path, flac: &Path, options: &[&str]) {
        let status = Command::new("flac")
            .arg("--silent")
            .arg("--force")
            .args(options)
            .arg("-o")
            .arg(flac)
            .arg(wav)
            .status()
            .expect("run the flac encoder");
        assert!(status.success(), "flac failed");
    }

    fn read_raw_sample(bytes: &[u8], bits: u16) -> i32 {
        match bits {
            8 => bytes[0] as i8 as i32,
            16 => i16::from_le_bytes([bytes[0], bytes[1]]) as i32,
            24 => {
                let value = bytes[0] as i32 | (bytes[1] as i32) << 8 | (bytes[2] as i32) << 16;
                (value << 8) >> 8
            }
            32 => i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            _ => unreachable!(),
        }
    }

    fn compare_with_libflac(path: &Path, rate: u32, bits: u16, channels: u16) {
        let raw_path = path.with_extension("raw");
        let status = Command::new("flac")
            .args([
                "--silent",
                "--force",
                "--decode",
                "--force-raw-format",
                "--endian=little",
                "--sign=signed",
            ])
            .arg("-o")
            .arg(&raw_path)
            .arg(path)
            .status()
            .expect("run the flac decoder");
        assert!(status.success(), "flac decode failed");

        let raw = fs::read(&raw_path).unwrap();
        let bytes_per_sample = bits as usize / 8;
        assert_eq!(raw.len() % bytes_per_sample, 0);
        let expected_samples = raw.len() / bytes_per_sample;

        let mut decoder = Decoder::new(File::open(path).unwrap()).unwrap();
        let mut frame = Frame::empty();
        let mut sample_index = 0usize;
        while decoder.read_frame(&mut frame).unwrap() {
            assert_eq!(frame.channels(), channels as u8);
            assert_eq!(frame.sample_rate, rate);
            assert_eq!(frame.bits_per_sample as u16, bits);
            for i in 0..frame.block_size {
                for channel in 0..channels as usize {
                    let offset = (sample_index * channels as usize + channel) * bytes_per_sample;
                    let expected = read_raw_sample(&raw[offset..offset + bytes_per_sample], bits);
                    assert_eq!(
                        frame.channel(channel)[i], expected,
                        "mismatch at sample {sample_index}, channel {channel}"
                    );
                }
                sample_index += 1;
            }
        }
        assert_eq!(sample_index * channels as usize, expected_samples);
        let _ = fs::remove_file(&raw_path);
    }

    #[test]
    #[ignore]
    fn bench_decode() {
        use std::time::Instant;

        let dir = temp_dir("bench");
        let wav = dir.join("tone.wav");
        let flac = wav.with_extension("flac");
        write_wav(&wav, 48_000, 16, 2, 48_000 * 60, -1);
        encode(&wav, &flac, &["-8"]);

        for _ in 0..2 {
            let mut decoder = Decoder::new(File::open(&flac).unwrap()).unwrap();
            let mut frame = Frame::empty();
            while decoder.read_frame(&mut frame).unwrap() {}
        }
        let start = Instant::now();
        for _ in 0..5 {
            let mut decoder = Decoder::new(File::open(&flac).unwrap()).unwrap();
            let mut frame = Frame::empty();
            while decoder.read_frame(&mut frame).unwrap() {}
        }
        let seconds = start.elapsed().as_secs_f64() / 5.0;
        println!(
            "ompi-flac: {:.3} ms per second of audio ({:.1}x realtime)",
            seconds * 1000.0 / 60.0,
            60.0 / seconds
        );

        let _ = fs::remove_dir_all(&dir);
    }

    fn write_noise_wav(path: &Path, frames: usize) {
        let mut data = Vec::with_capacity(frames * 4);
        let mut state = 0x1234_5678u32;
        for _ in 0..frames * 2 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let sample = (state >> 16) as i16;
            data.extend_from_slice(&sample.to_le_bytes());
        }
        let mut wav = Vec::with_capacity(44 + data.len());
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&((36 + data.len()) as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&48_000u32.to_le_bytes());
        wav.extend_from_slice(&(48_000u32 * 4).to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        fs::write(path, wav).unwrap();
    }

    #[test]
    fn decodes_verbatim_noise() {
        let dir = temp_dir("noise");
        let wav = dir.join("noise.wav");
        let flac = wav.with_extension("flac");
        write_noise_wav(&wav, 48_000);
        encode(&wav, &flac, &["-0"]);
        compare_with_libflac(&flac, 48_000, 16, 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn decodes_wasted_bits() {
        let dir = temp_dir("wasted");
        let wav = dir.join("wasted.wav");
        let flac = wav.with_extension("flac");
        write_wav(&wav, 48_000, 16, 2, 12_000, 0xfff0);
        encode(&wav, &flac, &["-8"]);
        compare_with_libflac(&flac, 48_000, 16, 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn decodes_generated_flac_identically_to_libflac() {
        let dir = temp_dir("decode");
        for (rate, bits, channels, options) in [
            (48_000u32, 16u16, 2u16, &["-8"][..]),
            (44_100, 16, 1, &["-0"][..]),
            (96_000, 24, 2, &["-8"][..]),
            (48_000, 8, 1, &["-5"][..]),
            (12_345, 16, 2, &["-5"][..]),
            (12_000, 16, 2, &["-5"][..]),
            (48_000, 16, 2, &["-8", "-b", "1000"][..]),
            (48_000, 16, 2, &["-8", "-l", "32", "--lax"][..]),
            (48_000, 16, 2, &["-8", "-r", "8"][..]),
            (48_000, 16, 3, &["-8"][..]),
            (48_000, 16, 4, &["-8"][..]),
            (48_000, 16, 5, &["-8"][..]),
            (48_000, 16, 6, &["-8"][..]),
            (48_000, 16, 7, &["-8"][..]),
            (48_000, 16, 8, &["-8"][..]),
        ] {
            let wav = dir.join(format!("tone-{rate}-{bits}-{channels}.wav"));
            let flac = wav.with_extension("flac");
            write_wav(&wav, rate, bits, channels, rate as usize / 4, -1);
            encode(&wav, &flac, options);
            compare_with_libflac(&flac, rate, bits, channels);
        }
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod test_32_bit {
    use std::fs::{self, File};
    use std::path::Path;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{Decoder, Frame};

    fn temp_dir() -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("radio-flac-32-{unique}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_wav(path: &Path, rate: u32, frames: usize) -> Vec<i32> {
        let channels = 2u16;
        let mut data = Vec::with_capacity(frames * 8);
        let mut expected = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let t = i as f64 / rate as f64;
            let left = ((t * 440.0 * std::f64::consts::TAU).sin() * 0.3 * 2_147_483_647.0) as i32;
            let right = ((t * 660.0 * std::f64::consts::TAU).sin() * 0.3 * 2_147_483_647.0) as i32;
            for sample in [left, right] {
                data.extend_from_slice(&sample.to_le_bytes());
                expected.push(sample);
            }
        }
        let mut fmt = Vec::with_capacity(40);
        fmt.extend_from_slice(&0xfffeu16.to_le_bytes());
        fmt.extend_from_slice(&channels.to_le_bytes());
        fmt.extend_from_slice(&rate.to_le_bytes());
        fmt.extend_from_slice(&(rate * 8).to_le_bytes());
        fmt.extend_from_slice(&8u16.to_le_bytes());
        fmt.extend_from_slice(&32u16.to_le_bytes());
        fmt.extend_from_slice(&22u16.to_le_bytes());
        fmt.extend_from_slice(&32u16.to_le_bytes());
        fmt.extend_from_slice(&0x3u32.to_le_bytes());
        fmt.extend_from_slice(&[
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38,
            0x9b, 0x71,
        ]);

        let mut wav = Vec::with_capacity(12 + fmt.len() + 8 + data.len());
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&((4 + 8 + fmt.len() + 8 + data.len()) as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        wav.extend_from_slice(&fmt);
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        fs::write(path, wav).unwrap();
        expected
    }

    #[test]
    fn decodes_32_bit_stereo_side_channel() {
        let dir = temp_dir();
        let wav = dir.join("tone32.wav");
        let flac = dir.join("tone32.flac");
        let expected = write_wav(&wav, 48_000, 12_000);
        let status = Command::new("flac")
            .args(["--silent", "--force", "--lax", "-8", "-o"])
            .arg(&flac)
            .arg(&wav)
            .status()
            .unwrap();
        assert!(status.success());

        let mut decoder = Decoder::new(File::open(&flac).unwrap()).unwrap();
        let mut frame = Frame::empty();
        let mut position = 0usize;
        while decoder.read_frame(&mut frame).unwrap() {
            for channel in 0..2 {
                for sample in 0..frame.block_size {
                    assert_eq!(
                        frame.channel(channel)[sample],
                        expected[(position + sample) * 2 + channel]
                    );
                }
            }
            position += frame.block_size;
        }
        assert_eq!(position * 2, expected.len());
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod test_stream {
    use std::io::Cursor;

    use super::{Decoder, Frame, crc};

    fn streaminfo(sample_rate: u32, channels: u8, bits: u8, total_samples: u64) -> [u8; 34] {
        let mut data = [0u8; 34];
        data[0..2].copy_from_slice(&16u16.to_be_bytes());
        data[2..4].copy_from_slice(&16u16.to_be_bytes());
        let packed = (sample_rate << 12)
            | (((channels - 1) as u32) << 9)
            | (((bits - 1) as u32) << 4)
            | ((total_samples >> 32) as u32 & 0x0f);
        data[10..14].copy_from_slice(&packed.to_be_bytes());
        data[14..18].copy_from_slice(&(total_samples as u32).to_be_bytes());
        data
    }

    fn variable_frame(sample: i16) -> Vec<u8> {
        let mut header = vec![0xff, 0xf9, 0x6a, 0x08, 0x00, 0x0f];
        let crc = crc::crc8(&header);
        header.push(crc);
        let mut frame = header;
        frame.push(0x00);
        frame.extend_from_slice(&sample.to_be_bytes());
        let crc = crc::crc16(&frame);
        frame.extend_from_slice(&crc.to_be_bytes());
        frame
    }

    #[test]
    fn variable_block_constant_frame() {
        let mut stream = b"fLaC".to_vec();
        stream.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]);
        stream.extend_from_slice(&streaminfo(48_000, 1, 16, 16));
        stream.extend_from_slice(&variable_frame(0x1234));

        let mut decoder = Decoder::new(Cursor::new(stream)).unwrap();
        let mut frame = Frame::empty();
        assert!(decoder.read_frame(&mut frame).unwrap());
        assert_eq!(frame.start_sample, 0);
        assert_eq!(frame.block_size, 16);
        assert_eq!(frame.sample_rate, 48_000);
        assert_eq!(frame.bits_per_sample, 16);
        assert_eq!(frame.channel(0), &[0x1234; 16]);
        assert!(!decoder.read_frame(&mut frame).unwrap());
    }

    #[test]
    fn truncated_and_corrupt_frames_error() {
        let mut stream = b"fLaC".to_vec();
        stream.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]);
        stream.extend_from_slice(&streaminfo(48_000, 1, 16, 16));
        let frame = variable_frame(1);
        stream.extend_from_slice(&frame);

        let mut truncated = stream.clone();
        truncated.truncate(truncated.len() - 1);
        let mut decoder = Decoder::new(Cursor::new(truncated)).unwrap();
        assert!(decoder.read_frame(&mut Frame::empty()).is_err());

        let mut corrupt = stream;
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0x55;
        let mut decoder = Decoder::new(Cursor::new(corrupt)).unwrap();
        assert!(decoder.read_frame(&mut Frame::empty()).is_err());
    }

    #[test]
    fn random_input_never_panics() {
        let mut state = 0x1234_5678u32;
        for length in 0..256 {
            let mut data = Vec::with_capacity(length);
            for _ in 0..length {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                data.push((state >> 24) as u8);
            }
            if let Ok(mut decoder) = Decoder::new(Cursor::new(data)) {
                let _ = decoder.read_frame(&mut Frame::empty());
            }
        }
    }
}
