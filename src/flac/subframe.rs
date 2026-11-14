use std::io::Read;

use super::bitreader::BitReader;
use super::error::{Result, error};
use super::residual;

#[derive(Clone, Copy, Debug)]
enum SubframeType {
    Constant,
    Verbatim,
    Fixed(usize),
    Lpc(usize),
}

fn header<R: Read>(reader: &mut BitReader<'_, R>, bps: u32) -> Result<(SubframeType, u32)> {
    if reader.read_bit()? {
        return error("FLAC subframe padding bit is set");
    }
    let kind = reader.read_bits(6)? as u8;
    let subframe_type = match kind {
        0 => SubframeType::Constant,
        1 => SubframeType::Verbatim,
        n if n & 0b111_000 == 0b001_000 => {
            let order = (n & 0b000_111) as usize;
            if order > 4 {
                return error("reserved FLAC fixed predictor order");
            }
            SubframeType::Fixed(order)
        }
        n if n & 0b111_110 == 0b000_010 || n & 0b111_100 == 0b000_100 => {
            return error("reserved FLAC subframe type");
        }
        n if n & 0b100_000 != 0 => SubframeType::Lpc((n & 0b011_111) as usize + 1),
        _ => return error("reserved FLAC subframe type"),
    };

    let wasted_bits = if reader.read_bit()? {
        reader.read_unary()? as u32 + 1
    } else {
        0
    };
    if wasted_bits >= bps {
        return error("FLAC subframe has no non-wasted bits");
    }
    Ok((subframe_type, wasted_bits))
}

fn decode_constant<R: Read>(
    reader: &mut BitReader<'_, R>,
    bps: u32,
    out: &mut [i64],
) -> Result<()> {
    let sample = reader.read_signed(bps)?;
    out.fill(sample);
    Ok(())
}

fn decode_verbatim<R: Read>(
    reader: &mut BitReader<'_, R>,
    bps: u32,
    out: &mut [i64],
) -> Result<()> {
    for sample in out {
        *sample = reader.read_signed(bps)?;
    }
    Ok(())
}

fn predict_fixed(order: usize, out: &mut [i64]) {
    let coefficients: &[i64] = match order {
        0 => &[],
        1 => &[1],
        2 => &[-1, 2],
        3 => &[1, -3, 3],
        4 => &[-1, 4, -6, 4],
        _ => unreachable!(),
    };
    for i in order..out.len() {
        let mut prediction = 0i64;
        for (j, coefficient) in coefficients.iter().enumerate() {
            prediction = prediction.wrapping_add(coefficient.wrapping_mul(out[i - order + j]));
        }
        out[i] = out[i].wrapping_add(prediction);
    }
}

fn decode_fixed<R: Read>(
    reader: &mut BitReader<'_, R>,
    bps: u32,
    order: usize,
    out: &mut [i64],
) -> Result<()> {
    if out.len() < order {
        return error("FLAC fixed predictor order exceeds the block size");
    }
    for sample in &mut out[..order] {
        *sample = reader.read_signed(bps)?;
    }
    residual::decode(reader, out.len(), order, &mut out[order..])?;
    predict_fixed(order, out);
    Ok(())
}

fn predict_lpc(order: usize, coefficients: &[i64], shift: u32, out: &mut [i64]) {
    let coefficients = &coefficients[..order];
    for i in order..out.len() {
        let prediction = {
            let window = &out[i - order..i];
            coefficients
                .iter()
                .zip(window)
                .fold(0i64, |sum, (&coefficient, &sample)| {
                    sum.wrapping_add(coefficient.wrapping_mul(sample))
                })
        };
        out[i] = out[i].wrapping_add(prediction >> shift);
    }
}

fn decode_lpc<R: Read>(
    reader: &mut BitReader<'_, R>,
    bps: u32,
    order: usize,
    out: &mut [i64],
) -> Result<()> {
    if out.len() < order {
        return error("FLAC LPC order exceeds the block size");
    }
    for sample in &mut out[..order] {
        *sample = reader.read_signed(bps)?;
    }

    let precision = reader.read_bits(4)? as u32 + 1;
    if precision == 16 {
        return error("reserved FLAC LPC coefficient precision");
    }
    let shift = reader.read_signed(5)?;
    if shift < 0 {
        return error("negative FLAC LPC shift is forbidden");
    }

    let mut coefficients = [0i64; 32];
    for coefficient in coefficients[..order].iter_mut().rev() {
        *coefficient = reader.read_signed(precision)?;
    }

    residual::decode(reader, out.len(), order, &mut out[order..])?;
    predict_lpc(order, &coefficients[..order], shift as u32, out);
    Ok(())
}

pub(crate) fn decode<R: Read>(
    reader: &mut BitReader<'_, R>,
    bps: u32,
    out: &mut [i64],
) -> Result<()> {
    let (subframe_type, wasted_bits) = header(reader, bps)?;
    let subframe_bits = bps - wasted_bits;

    match subframe_type {
        SubframeType::Constant => decode_constant(reader, subframe_bits, out)?,
        SubframeType::Verbatim => decode_verbatim(reader, subframe_bits, out)?,
        SubframeType::Fixed(order) => decode_fixed(reader, subframe_bits, order, out)?,
        SubframeType::Lpc(order) => decode_lpc(reader, subframe_bits, order, out)?,
    }

    if wasted_bits > 0 {
        for sample in out {
            *sample = sample.wrapping_shl(wasted_bits);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::predict_fixed;
    use super::predict_lpc;

    #[test]
    fn fixed_reconstruction() {
        let mut out = [
            -729, -722, -667, -19, -16, 17, -23, -7, 16, -16, -5, 3, -8, -13, -15, -1,
        ];
        predict_fixed(3, &mut out);
        assert_eq!(
            out,
            [
                -729, -722, -667, -583, -486, -359, -225, -91, 59, 209, 354, 497, 630, 740, 812,
                845
            ]
        );
    }

    #[test]
    fn fixed_orders() {
        let mut order0 = [1, 2, 3, 4];
        predict_fixed(0, &mut order0);
        assert_eq!(order0, [1, 2, 3, 4]);

        let mut order1 = [1, 2, 3, 4];
        predict_fixed(1, &mut order1);
        assert_eq!(order1, [1, 3, 6, 10]);

        let mut order2 = [1, 2, 3, 4];
        predict_fixed(2, &mut order2);
        assert_eq!(order2, [1, 2, 6, 14]);

        let mut order4 = [1, 2, 3, 4, 5];
        predict_fixed(4, &mut order4);
        assert_eq!(order4, [1, 2, 3, 4, 10]);
    }

    #[test]
    fn lpc_reconstruction() {
        let coefficients = [-75, 166, 121, -269, -75, -399, 1042];
        let mut out = [
            -796, -547, -285, -32, 199, 443, 670, -2, -23, 14, 6, 3, -4, 12, -2, 10,
        ];
        predict_lpc(7, &coefficients, 9, &mut out);
        assert_eq!(
            out,
            [
                -796, -547, -285, -32, 199, 443, 670, 875, 1046, 1208, 1343, 1454, 1541, 1616,
                1663, 1701
            ]
        );
    }
}
