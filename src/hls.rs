// HLS output built from the existing one-packet-per-page stream.
// Ogg is not an HLS segment format, so audio pages are muxed into MPEG-TS.

use crate::ogg::PRESKIP;
use bytes::Bytes;
use std::collections::VecDeque;
use std::sync::Mutex;

pub const SEGMENT_PAGES: usize = 250;
pub const SEGMENT_CAP: usize = 2;
const SEGMENT_SECONDS: u64 = 5;

const PAT_PID: u16 = 0x0000;
const PMT_PID: u16 = 0x1000;
const AUDIO_PID: u16 = 0x0100;
const PRIVATE_STREAM_1: u8 = 0xBD;
const TS_PACKET: usize = 188;

pub struct Hls {
    inner: Mutex<State>,
}

struct State {
    current: Vec<u8>,
    current_pages: usize,
    pes: Vec<u8>,
    segments: VecDeque<Segment>,
    next_segment: u64,
    pat_cc: u8,
    pmt_cc: u8,
    audio_cc: u8,
    #[cfg(test)]
    warned: bool,
}

struct Segment {
    seq: u64,
    data: Bytes,
}

impl State {
    fn new() -> Self {
        State {
            current: Vec::with_capacity(SEGMENT_PAGES * 1024),
            current_pages: 0,
            pes: Vec::with_capacity(2048),
            segments: VecDeque::with_capacity(SEGMENT_CAP),
            next_segment: 0,
            pat_cc: 0,
            pmt_cc: 0,
            audio_cc: 0,
            #[cfg(test)]
            warned: false,
        }
    }
}

impl Hls {
    pub fn new() -> Self {
        Hls {
            inner: Mutex::new(State::new()),
        }
    }

    pub fn reset(&self) {
        *self.inner.lock().unwrap_or_else(|error| error.into_inner()) = State::new();
    }

    /// Consume one audio page. Complete five-second segments are retained in a
    /// two-slot ring; a slow HLS client can only ask for a segment that has
    /// already fallen out of it.
    #[cfg(test)]
    pub fn push_page(&self, page: &Bytes) {
        let Some((granule, data)) = page_sample(page) else {
            let mut state = self.inner.lock().unwrap();
            if !state.warned {
                eprintln!("[hls] ignoring malformed Ogg page");
                state.warned = true;
            }
            return;
        };
        self.push_packet(granule, data);
    }

    /// Append one Opus packet without re-parsing the Ogg page. The station has
    /// the granule and payload already when it writes the page.
    pub fn push_packet(&self, granule: u64, data: Bytes) {
        let mut state = self.inner.lock().unwrap();
        if state.current_pages == 0 {
            begin_segment(&mut state);
        }
        let State {
            current,
            audio_cc,
            pes,
            ..
        } = &mut *state;
        pes_packet(current, audio_cc, pes, granule, &data);
        state.current_pages += 1;
        if state.current_pages == SEGMENT_PAGES {
            finish_segment(&mut state);
        }
    }

    /// None until the first complete segment exists. Segment URIs use the
    /// configured playlist prefix, so changing it takes effect immediately.
    pub fn playlist(&self, segment_prefix: &str) -> Option<Bytes> {
        let state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        render_playlist(&state.segments, segment_prefix)
    }

    pub fn segment(&self, seq: u64) -> Option<Bytes> {
        let state = self.inner.lock().unwrap();
        state
            .segments
            .iter()
            .find(|segment| segment.seq == seq)
            .map(|segment| segment.data.clone())
    }
}

#[cfg(test)]
fn page_sample(page: &Bytes) -> Option<(u64, Bytes)> {
    let bytes = page.as_ref();
    if bytes.len() < 27 || &bytes[..4] != b"OggS" || bytes[4] != 0 {
        return None;
    }
    let segments = bytes[26] as usize;
    let table_end = 27 + segments;
    if bytes.len() < table_end {
        return None;
    }
    let table = &bytes[27..table_end];
    if table.is_empty() || table.last().is_some_and(|&last| last == 255) {
        return None;
    }
    let payload_len: usize = table.iter().map(|&n| n as usize).sum();
    let payload_end = table_end + payload_len;
    if payload_end > bytes.len() {
        return None;
    }
    let granule = u64::from_le_bytes(bytes[6..14].try_into().ok()?);
    Some((granule, page.slice(table_end..payload_end)))
}

fn begin_segment(state: &mut State) {
    let pat = pat_section();
    let pmt = pmt_section();
    psi_packet(&mut state.current, PAT_PID, state.pat_cc, &pat);
    state.pat_cc = state.pat_cc.wrapping_add(1);
    psi_packet(&mut state.current, PMT_PID, state.pmt_cc, &pmt);
    state.pmt_cc = state.pmt_cc.wrapping_add(1);
}

fn finish_segment(state: &mut State) {
    debug_assert_eq!(state.current.len() % TS_PACKET, 0);
    state.next_segment += 1;
    let seq = state.next_segment - 1;
    let data = Bytes::from(std::mem::replace(
        &mut state.current,
        Vec::with_capacity(SEGMENT_PAGES * 1024),
    ));
    state.segments.push_back(Segment { seq, data });
    while state.segments.len() > SEGMENT_CAP {
        state.segments.pop_front();
    }
    state.current_pages = 0;
}

fn render_playlist(segments: &VecDeque<Segment>, segment_prefix: &str) -> Option<Bytes> {
    let first = segments.front()?.seq;
    let name = segment_prefix.rsplit('/').next().unwrap_or(segment_prefix);
    let mut out = String::with_capacity(128 + segments.len() * 40);
    out.push_str("#EXTM3U\n");
    out.push_str("#EXT-X-VERSION:3\n");
    out.push_str("#EXT-X-TARGETDURATION:5\n");
    out.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{first}\n"));
    for segment in segments {
        out.push_str(&format!(
            "#EXTINF:{SEGMENT_SECONDS}.000,\n{name}{}.ts\n",
            segment.seq
        ));
    }
    Some(Bytes::from(out))
}

fn pat_section() -> Vec<u8> {
    let mut section = Vec::with_capacity(16);
    section.push(0x00);
    section.push(0xB0);
    section.push(13);
    section.extend(1u16.to_be_bytes());
    section.push(0xC1);
    section.push(0);
    section.push(0);
    section.extend(1u16.to_be_bytes());
    section.extend((0xE000 | PMT_PID).to_be_bytes());
    let crc = crc32_mpeg(&section);
    section.extend(crc.to_be_bytes());
    section
}

fn pmt_section() -> Vec<u8> {
    let mut section = Vec::with_capacity(24);
    section.push(0x02);
    section.push(0xB0);
    section.push(24);
    section.extend(1u16.to_be_bytes());
    section.push(0xC1);
    section.push(0);
    section.push(0);
    section.extend((0xE000 | AUDIO_PID).to_be_bytes());
    section.extend(0u16.to_be_bytes()); // program_info_length
    section.push(0x06); // private PES data
    section.extend((0xE000 | AUDIO_PID).to_be_bytes());
    section.extend(6u16.to_be_bytes()); // ES_info_length
    section.extend([0x05, 4, b'O', b'p', b'u', b's']); // registration_descriptor
    let crc = crc32_mpeg(&section);
    section.extend(crc.to_be_bytes());
    section
}

fn psi_packet(out: &mut Vec<u8>, pid: u16, cc: u8, section: &[u8]) {
    let mut payload = Vec::with_capacity(1 + section.len());
    payload.push(0); // pointer_field
    payload.extend_from_slice(section);
    ts_packet(out, pid, cc, true, None, &payload);
}

fn pes_packet(out: &mut Vec<u8>, cc: &mut u8, pes: &mut Vec<u8>, granule: u64, data: &[u8]) {
    let pts = granule.saturating_sub(PRESKIP) * 90_000 / 48_000;
    pes.clear();
    pes.extend([0x00, 0x00, 0x01, PRIVATE_STREAM_1]);
    let pes_len = 8 + data.len();
    pes.extend((pes_len.min(u16::MAX as usize) as u16).to_be_bytes());
    pes.push(0x80); // '10' + no flags
    pes.push(0x80); // PTS present, no DTS
    pes.push(5);
    write_pts(0x2, pts, pes);
    pes.extend_from_slice(data);

    let mut offset = 0;
    let mut first = true;
    while offset < pes.len() {
        let pcr = first.then_some(pts);
        let capacity = if pcr.is_some() { 176 } else { 184 };
        let take = capacity.min(pes.len() - offset);
        ts_packet(
            out,
            AUDIO_PID,
            *cc,
            first,
            pcr,
            &pes[offset..offset + take],
        );
        *cc = cc.wrapping_add(1);
        first = false;
        offset += take;
    }
}

fn ts_packet(
    out: &mut Vec<u8>,
    pid: u16,
    cc: u8,
    pusi: bool,
    pcr: Option<u64>,
    payload: &[u8],
) {
    let capacity = if pcr.is_some() { 176 } else { 184 };
    debug_assert!(payload.len() <= capacity);
    out.push(0x47);
    out.push((if pusi { 0x40 } else { 0 }) | ((pid >> 8) as u8 & 0x1F));
    out.push(pid as u8);

    if let Some(pts) = pcr {
        out.push(0x30 | (cc & 0x0F));
        let adaptation_len = 183 - payload.len() as u8;
        out.push(adaptation_len);
        out.push(0x10); // PCR_flag
        write_pcr(pts, out);
        out.extend(std::iter::repeat_n(0xFF, adaptation_len as usize - 7));
    } else if payload.len() == 184 {
        out.push(0x10 | (cc & 0x0F));
        out.extend_from_slice(payload);
        return;
    } else {
        out.push(0x30 | (cc & 0x0F));
        let adaptation_len = 183 - payload.len() as u8;
        out.push(adaptation_len);
        if adaptation_len > 0 {
            out.push(0);
            out.extend(std::iter::repeat_n(0xFF, adaptation_len as usize - 1));
        }
    }
    out.extend_from_slice(payload);
}

fn write_pts(prefix: u8, pts: u64, out: &mut Vec<u8>) {
    let pts = pts & 0x1_FFFF_FFFF;
    out.push((prefix << 4) | (((pts >> 29) as u8) & 0x0E) | 1);
    out.push((pts >> 22) as u8);
    out.push((((pts >> 14) as u8) & 0xFE) | 1);
    out.push((pts >> 7) as u8);
    out.push((((pts << 1) as u8) & 0xFE) | 1);
}

fn write_pcr(pts: u64, out: &mut Vec<u8>) {
    let base = pts & 0x1_FFFF_FFFF;
    out.push((base >> 25) as u8);
    out.push((base >> 17) as u8);
    out.push((base >> 9) as u8);
    out.push((base >> 1) as u8);
    out.push((((base & 1) as u8) << 7) | 0x7E);
    out.push(0);
}

fn crc32_mpeg(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ogg::PageWriter;

    fn push(hls: &Hls, writer: &mut PageWriter, from: u64, to: u64) {
        for granule in from..to {
            let page = writer.audio(granule, &[0xAA; 64]);
            hls.push_page(&page);
        }
    }

    fn ts_pid(packet: &[u8]) -> u16 {
        (((packet[1] & 0x1F) as u16) << 8) | packet[2] as u16
    }

    fn audio_ccs(segment: &[u8]) -> Vec<u8> {
        segment
            .chunks(TS_PACKET)
            .filter(|packet| ts_pid(packet) == AUDIO_PID)
            .map(|packet| packet[3] & 0x0F)
            .collect()
    }

    #[test]
    fn waits_for_a_full_segment() {
        let hls = Hls::new();
        let mut writer = PageWriter::new(1);
        push(&hls, &mut writer, 0, SEGMENT_PAGES as u64 - 1);
        assert!(hls.playlist("/radio-").is_none());
        assert!(hls.segment(0).is_none());

        let page = writer.audio(SEGMENT_PAGES as u64, &[0xAA; 64]);
        hls.push_page(&page);
        assert!(hls.playlist("/radio-").is_some());
        assert!(hls.segment(0).is_some());
    }

    #[test]
    fn playlist_tracks_the_two_segment_window() {
        let hls = Hls::new();
        let mut writer = PageWriter::new(1);
        push(&hls, &mut writer, 0, SEGMENT_PAGES as u64 * 4);

        let playlist = String::from_utf8(hls.playlist("/radio-").unwrap().to_vec()).unwrap();
        assert!(playlist.contains("#EXT-X-TARGETDURATION:5\n"));
        assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:2\n"));
        assert!(playlist.contains("radio-2.ts\n"));
        assert!(playlist.contains("radio-3.ts\n"));
        assert!(!playlist.contains("radio-0.ts\n"));
        assert!(!playlist.contains("radio-1.ts\n"));
        assert!(hls.segment(0).is_none());
        assert!(hls.segment(1).is_none());
        assert!(hls.segment(2).is_some());
        assert!(hls.segment(3).is_some());
    }

    #[test]
    fn direct_packet_feed_matches_page_feed() {
        let from_pages = Hls::new();
        let from_packets = Hls::new();
        let mut writer = PageWriter::new(1);
        for granule in 0..SEGMENT_PAGES as u64 * 2 {
            let audio = writer.audio_page(granule, &[0xAA; 64]);
            from_pages.push_page(&audio.page);
            from_packets.push_packet(audio.granule, audio.payload);
        }
        assert_eq!(
            from_pages.segment(0).unwrap(),
            from_packets.segment(0).unwrap()
        );
        assert_eq!(from_pages.playlist("/radio-"), from_packets.playlist("/radio-"));
    }

    #[test]
    fn segments_are_ts_packets_with_a_pat_and_pmt() {
        let hls = Hls::new();
        let mut writer = PageWriter::new(1);
        push(&hls, &mut writer, 0, SEGMENT_PAGES as u64);
        let segment = hls.segment(0).unwrap();

        assert_eq!(segment.len() % TS_PACKET, 0);
        assert_eq!(segment[0], 0x47);
        assert_eq!(ts_pid(&segment[..TS_PACKET]), PAT_PID);
        assert_eq!(ts_pid(&segment[TS_PACKET..TS_PACKET * 2]), PMT_PID);
        assert_eq!(ts_pid(&segment[TS_PACKET * 2..TS_PACKET * 3]), AUDIO_PID);
        assert!(segment.windows(4).any(|w| w == [0x00, 0x00, 0x01, 0xBD]));
    }

    #[test]
    fn continuity_counters_continue_across_segments() {
        let hls = Hls::new();
        let mut writer = PageWriter::new(1);
        push(&hls, &mut writer, 0, SEGMENT_PAGES as u64 * 2);
        let first = audio_ccs(&hls.segment(0).unwrap());
        let second = audio_ccs(&hls.segment(1).unwrap());
        assert_eq!(second[0], (first.last().unwrap() + 1) & 0x0F);
    }

    #[test]
    #[ignore]
    fn bench_segment_mux() {
        use std::time::Instant;

        const PAGES: u64 = 50_000;
        let mut writer = PageWriter::new(1);
        let pages: Vec<(Bytes, Bytes)> = (0..PAGES)
            .map(|granule| {
                let audio = writer.audio_page(granule, &[0xAA; 800]);
                (audio.page, audio.payload)
            })
            .collect();

        let from_pages = Hls::new();
        let start = Instant::now();
        for (page, _) in &pages {
            from_pages.push_page(page);
        }
        let page_elapsed = start.elapsed();

        let from_packets = Hls::new();
        let start = Instant::now();
        for (granule, (_, payload)) in pages.iter().enumerate() {
            from_packets.push_packet(granule as u64, payload.clone());
        }
        let packet_elapsed = start.elapsed();
        let segments = from_pages.inner.lock().unwrap().next_segment;
        eprintln!(
            "hls page path: {:.2} us/page, {:.3} ms/segment; packet path: {:.2} us/page, {:.3} ms/segment; {segments} segments",
            page_elapsed.as_secs_f64() * 1_000_000.0 / pages.len() as f64,
            page_elapsed.as_secs_f64() * 1000.0 / segments as f64,
            packet_elapsed.as_secs_f64() * 1_000_000.0 / pages.len() as f64,
            packet_elapsed.as_secs_f64() * 1000.0 / segments as f64,
        );
        assert_eq!(from_pages.playlist("/radio-"), from_packets.playlist("/radio-"));
        assert_eq!(segments, PAGES / SEGMENT_PAGES as u64);
    }
}
