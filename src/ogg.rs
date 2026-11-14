// RFC 7845 muxer: CRC, page framing, OpusHead, OpusTags
// One Opus packet per page. pages are immutable Bytes so the fan-out can share them

use bytes::Bytes;

/// CELT encoder lookahead at 48 kHz (2.5 ms). Decoders discard this many
/// leading samples; granule positions are offset by it
pub const PRESKIP: u64 = 120;

/// Ogg CRC-32: polynomial 0x04c11db7, MSB-first, init 0, no final xor
fn ogg_crc(data: &[u8]) -> u32 {
    let mut crc: u32 = 0;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn opus_head() -> Vec<u8> {
    let mut b = Vec::with_capacity(19);
    b.extend_from_slice(b"OpusHead");
    b.push(1); // version
    b.push(2); // channels: stereo
    b.extend_from_slice(&(PRESKIP as u16).to_le_bytes());
    b.extend_from_slice(&48_000u32.to_le_bytes()); // original input sample rate
    b.extend_from_slice(&0i16.to_le_bytes()); // output gain
    b.push(0);
    b
}

fn opus_tags() -> Vec<u8> {
    let vendor = b"radio-station";
    let mut b = Vec::with_capacity(8 + 4 + vendor.len() + 4);
    b.extend_from_slice(b"OpusTags");
    b.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    b.extend_from_slice(vendor);
    b.extend_from_slice(&0u32.to_le_bytes());
    b
}

pub struct AudioPage {
    pub page: Bytes,
    pub granule: u64,
    pub payload: Bytes,
}

pub struct PageWriter {
    serial: u32,
    seq: u32,
}

impl PageWriter {
    pub fn new(serial: u32) -> Self {
        PageWriter { serial, seq: 0 }
    }

    fn page(&mut self, header_type: u8, granule: u64, payload: &[u8]) -> Bytes {
        // Segment (lacing) taable for one packet
        let mut segtab = Vec::with_capacity(payload.len() / 255 + 1);
        let mut n = payload.len();
        while n >= 255 {
            segtab.push(255u8);
            n -= 255;
        }
        segtab.push(n as u8);

        let mut buf = Vec::with_capacity(27 + segtab.len() + payload.len());
        buf.extend_from_slice(b"OggS");
        buf.push(0); // stream structure version
        buf.push(header_type);
        buf.extend_from_slice(&granule.to_le_bytes());
        buf.extend_from_slice(&self.serial.to_le_bytes());
        buf.extend_from_slice(&self.seq.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes()); // CRC placeholder
        buf.push(segtab.len() as u8);
        buf.extend_from_slice(&segtab);
        buf.extend_from_slice(payload);
        let crc = ogg_crc(&buf);
        buf[22..26].copy_from_slice(&crc.to_le_bytes());
        self.seq += 1;
        Bytes::from(buf)
    }

    /// sequence 0
    pub fn head(&mut self) -> Bytes {
        self.page(0x02, 0, &opus_head())
    }

    /// sequence 1
    pub fn tags(&mut self) -> Bytes {
        self.page(0x00, 0, &opus_tags())
    }

    #[cfg(test)]
    pub fn audio(&mut self, granule: u64, payload: &[u8]) -> Bytes {
        self.audio_page(granule, payload).page
    }

    pub fn audio_page(&mut self, granule: u64, payload: &[u8]) -> AudioPage {
        let page = self.page(0x00, granule, payload);
        let segments = page[26] as usize;
        let start = 27 + segments;
        let payload = page.slice(start..start + payload.len());
        AudioPage {
            page,
            granule,
            payload,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected_lacing(len: usize) -> Vec<u8> {
        let mut table = vec![255u8; len / 255];
        table.push((len % 255) as u8);
        table
    }

    fn check_lacing(len: usize) {
        let mut writer = PageWriter::new(0x1234_5678);
        let payload = vec![0u8; len];
        let page = writer.audio(0, &payload);
        let segments = page[26] as usize;
        let expected = expected_lacing(len);

        assert_eq!(segments, expected.len(), "segment count for {len}");
        assert_eq!(
            &page[27..27 + segments],
            expected.as_slice(),
            "lacing for {len}"
        );
        assert_eq!(page.len(), 27 + segments + len, "page length for {len}");
    }

    #[test]
    fn lacing_tables_match_the_spec_for_the_encoder_buffer() {
        for len in 0..=4000 {
            check_lacing(len);
        }
    }

    #[test]
    fn lacing_supports_255_segments() {
        check_lacing(255 * 255 - 1);
    }
}
