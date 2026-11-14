use std::io::Read;

use super::StreamInfo;
use super::bitreader::ByteSource;
use super::crc::crc8;
use super::error::{Result, error};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockingStrategy {
    Fixed,
    Variable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelAssignment {
    Independent(u8),
    LeftSide,
    RightSide,
    MidSide,
}

impl ChannelAssignment {
    pub fn channels(self) -> u8 {
        match self {
            Self::Independent(count) => count,
            Self::LeftSide | Self::RightSide | Self::MidSide => 2,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FrameHeader {
    pub blocking: BlockingStrategy,
    pub block_size: usize,
    pub sample_rate: u32,
    pub bits_per_sample: u8,
    pub channel_assignment: ChannelAssignment,
    pub start_sample: u64,
}

fn read_coded_number<R: Read>(source: &mut ByteSource<R>) -> Result<u64> {
    let first = source.read_u8()?;
    let leading_ones = first.leading_ones();
    if leading_ones == 1 || leading_ones > 7 {
        return error("invalid FLAC coded number");
    }
    if leading_ones == 0 {
        return Ok((first & 0x7f) as u64);
    }

    let total_bytes = leading_ones as usize;
    let payload_bits = 7 - leading_ones;
    let mask = if payload_bits == 0 {
        0
    } else {
        (1u8 << payload_bits) - 1
    };
    let mut value = (first & mask) as u64;
    for _ in 1..total_bytes {
        let byte = source.read_u8()?;
        if byte & 0xc0 != 0x80 {
            return error("invalid FLAC coded number");
        }
        value = (value << 6) | (byte & 0x3f) as u64;
    }
    Ok(value)
}

pub(crate) fn parse_header<R: Read>(
    source: &mut ByteSource<R>,
    info: &StreamInfo,
) -> Result<FrameHeader> {
    let first = source.read_be_u16()?;
    if first & 0xfffc != 0xfff8 {
        return error("FLAC frame sync code missing");
    }
    if first & 0x0002 != 0 {
        return error("reserved FLAC frame header bit is set");
    }
    let blocking = if first & 1 == 0 {
        BlockingStrategy::Fixed
    } else {
        BlockingStrategy::Variable
    };

    let block_size_and_rate = source.read_u8()?;
    let block_code = block_size_and_rate >> 4;
    let rate_code = block_size_and_rate & 0x0f;
    if block_code == 0 || rate_code == 0x0f {
        return error("reserved FLAC frame header value");
    }

    let channels_and_depth = source.read_u8()?;
    let assignment = match channels_and_depth >> 4 {
        n @ 0..=7 => ChannelAssignment::Independent(n + 1),
        8 => ChannelAssignment::LeftSide,
        9 => ChannelAssignment::RightSide,
        10 => ChannelAssignment::MidSide,
        _ => return error("reserved FLAC channel assignment"),
    };
    let bits_code = (channels_and_depth >> 1) & 0x07;
    if channels_and_depth & 1 != 0 {
        return error("reserved FLAC frame header bit is set");
    }

    let coded_number = read_coded_number(source)?;

    let block_size = match block_code {
        1 => 192,
        2..=5 => 576usize << (block_code - 2),
        6 => source.read_u8()? as usize + 1,
        7 => {
            let value = source.read_be_u16()? as usize;
            if value == 0xffff {
                return error("FLAC block size exceeds 65535");
            }
            value + 1
        }
        8..=15 => 256usize << (block_code - 8),
        _ => unreachable!(),
    };
    if !(1..=65535).contains(&block_size) {
        return error("invalid FLAC block size");
    }

    let sample_rate = match rate_code {
        0 => info.sample_rate,
        1 => 88_200,
        2 => 176_400,
        3 => 192_000,
        4 => 8_000,
        5 => 16_000,
        6 => 22_050,
        7 => 24_000,
        8 => 32_000,
        9 => 44_100,
        10 => 48_000,
        11 => 96_000,
        12 => source.read_u8()? as u32 * 1_000,
        13 => source.read_be_u16()? as u32,
        14 => source.read_be_u16()? as u32 * 10,
        _ => unreachable!(),
    };

    let bits_per_sample = match bits_code {
        0 => info.bits_per_sample,
        1 => 8,
        2 => 12,
        4 => 16,
        5 => 20,
        6 => 24,
        7 => 32,
        _ => return error("reserved FLAC bit depth"),
    };
    if !(1..=32).contains(&bits_per_sample) {
        return error("invalid FLAC bit depth");
    }

    let start_sample = match blocking {
        BlockingStrategy::Variable => coded_number,
        BlockingStrategy::Fixed => {
            if coded_number > 0x7fff_ffff {
                return error("FLAC frame number is too large");
            }
            // Last frame may be short; frame numbers count nominal blocks.
            let nominal_block_size = if info.min_block_size == info.max_block_size {
                info.max_block_size as u64
            } else {
                block_size as u64
            };
            coded_number
                .checked_mul(nominal_block_size)
                .ok_or_else(|| super::error::Error::new("FLAC sample position overflow"))?
        }
    };

    let header_end = source.position();
    let expected_crc = crc8(source.crc_bytes(header_end));
    let stored_crc = source.read_u8()?;
    if expected_crc != stored_crc {
        return error("FLAC frame header CRC mismatch");
    }

    Ok(FrameHeader {
        blocking,
        block_size,
        sample_rate,
        bits_per_sample,
        channel_assignment: assignment,
        start_sample,
    })
}
