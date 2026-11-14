use std::io::Read;

use super::bitreader::BitReader;
use super::error::{Result, error};

#[inline(always)]
fn rice_to_signed(folded: u32) -> i64 {
    let half = (folded >> 1) as i64;
    let sign = -((folded & 1) as i64);
    half ^ sign
}

pub(crate) fn decode<R: Read>(
    reader: &mut BitReader<'_, R>,
    block_size: usize,
    predictor_order: usize,
    out: &mut [i64],
) -> Result<()> {
    let method = reader.read_bits(2)?;
    let param_bits = match method {
        0 => 4,
        1 => 5,
        _ => return error("reserved FLAC residual coding method"),
    };
    let partition_order = reader.read_bits(4)? as usize;
    let partitions = 1usize << partition_order;
    if block_size & (partitions - 1) != 0 {
        return error("FLAC residual partition order does not divide the block");
    }
    let partition_size = block_size >> partition_order;
    if partition_size <= predictor_order {
        return error("FLAC residual partition is smaller than the predictor order");
    }
    let expected_len = block_size - predictor_order;
    if out.len() != expected_len {
        return error("internal FLAC residual buffer length mismatch");
    }

    let escape = (1u64 << param_bits) - 1;
    let mut start = 0usize;
    let mut partition_len = partition_size - predictor_order;
    for _ in 0..partitions {
        let parameter = reader.read_bits(param_bits as u32)?;
        let end = start + partition_len;
        let partition = &mut out[start..end];

        if parameter == escape {
            let raw_bits = reader.read_bits(5)? as u32;
            if raw_bits == 0 {
                partition.fill(0);
            } else {
                for sample in partition {
                    *sample = reader.read_signed(raw_bits)?;
                }
            }
        } else if parameter == 0 {
            for sample in partition {
                let folded = reader.read_unary()?;
                if folded >= u32::MAX as u64 {
                    return error("FLAC residual is out of range");
                }
                *sample = rice_to_signed(folded as u32);
            }
        } else {
            let parameter = parameter as u32;
            for sample in partition {
                let quotient = reader.read_unary()?;
                let remainder = reader.read_bits(parameter)?;
                let folded = (quotient << parameter) | remainder;
                if folded >= u32::MAX as u64 {
                    return error("FLAC residual is out of range");
                }
                *sample = rice_to_signed(folded as u32);
            }
        }

        start = end;
        partition_len = partition_size;
    }
    if start != out.len() {
        return error("FLAC residual did not fill the block");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::decode;
    use crate::flac::bitreader::{BitReader, ByteSource};

    fn unpack(bits: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut byte = 0u8;
        let mut count = 0u8;
        for bit in bits.bytes() {
            byte = (byte << 1) | (bit == b'1') as u8;
            count += 1;
            if count == 8 {
                bytes.push(byte);
                byte = 0;
                count = 0;
            }
        }
        if count > 0 {
            bytes.push(byte << (8 - count));
        }
        bytes
    }

    fn run(bits: &str, block_size: usize) -> Vec<i64> {
        let mut source = ByteSource::new(Cursor::new(unpack(bits)));
        source.start_frame();
        let mut reader = BitReader::new(&mut source);
        let mut out = vec![0; block_size];
        decode(&mut reader, block_size, 0, &mut out).unwrap();
        out
    }

    #[test]
    fn escaped_partition() {
        // method=00, order=0, escape=1111, raw_bits=00100, samples 0, -1, 2.
        let bits = "00 0000 1111 00100 0000 1111 0010";
        assert_eq!(run(&bits.replace(' ', ""), 3), vec![0, -1, 2]);
    }

    #[test]
    fn rice2_partition() {
        // method=01, order=0, parameter=00011; folded 0, 1, 2.
        let bits = "01 0000 00011 1 000 1 001 1 010";
        assert_eq!(run(&bits.replace(' ', ""), 3), vec![0, -1, 1]);
    }

    #[test]
    fn rice_partition() {
        // method=00, order=0, parameter=0011; folded 0, 1, 2.
        let bits = "00 0000 0011 1 000 1 001 1 010";
        assert_eq!(run(&bits.replace(' ', ""), 3), vec![0, -1, 1]);
    }
}
