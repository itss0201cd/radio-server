use crate::encoder::Encoder;
use crate::hls::Hls;
use crate::ogg::{AudioPage, PRESKIP, PageWriter};
use crate::pagecache;
use crate::resample::{Resampler, scaled_frames};
use crate::wav::{self, WavStream, is_wav, wav_format_label};
use bytes::Bytes;
use rand::Rng;
use rand::seq::{IndexedRandom, SliceRandom};
use serde_json::json;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, watch};

pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAME_SAMPLES: usize = 960;
pub const FRAME_FLOATS: usize = FRAME_SAMPLES * 2;
pub const BURST_SECONDS: u64 = 10;
pub const BURST_PAGES: u64 = BURST_SECONDS * 1000 / 20;
pub const BCAST_CAP: usize = BURST_PAGES as usize + 100;
const BITRATE_KBPS: i32 = 320;
const RING_BLOCKS: usize = 60 * 50;
pub const ARTIST_COOLDOWN: f64 = 3600.0;
const IDLE_WAIT: Duration = Duration::from_secs(5);
/// Keep a newly uploaded track visible at the queue head for one publish
/// cycle. The encoder's ten-second ring absorbs the delay.
const FRESH_HOLD: Duration = Duration::from_secs(1);
const SILENCE_DB: f32 = -60.0;
const RING_DB: f32 = -40.0;
const MAX_OVERLAP_SECS: f64 = 8.0;

pub type Page = (u64, Bytes);

pub struct NowPlaying {
    pub artist: String,
    pub title: String,
    pub file: String,
    pub format: String,
}

struct Block {
    pcm: Vec<f32>,
    /// Play id of the track this block starts, if it is the first block of one.
    play: Option<u64>,
}

/// One open stream socket. Entries live only in memory: they are added when
/// the connection is accepted and removed when its guard is dropped.
struct Listener {
    id: u64,
    addr: String,
    user_agent: String,
    connected_at: u64,
    disconnect: watch::Sender<bool>,
}

#[derive(Clone)]
pub struct Status {
    listeners: Vec<Arc<Listener>>,
    now: Option<Arc<NowPlaying>>,
    queue: Vec<Arc<NowPlaying>>,
    /// Admin SSE frame: includes the queue preview and total.
    pub frame: Bytes,
    /// Public SSE frame: only listeners + now playing, so the player works
    /// without logging in.
    pub public_frame: Bytes,
    json: Bytes,
}

impl Status {
    fn new() -> Self {
        let mut s = Status {
            listeners: Vec::new(),
            now: None,
            queue: Vec::new(),
            frame: Bytes::new(),
            public_frame: Bytes::new(),
            json: Bytes::new(),
        };
        s.render();
        s
    }

    fn render(&mut self) {
        let now = self.now.as_ref().map(|n| np_json(n));
        let count = self.listeners.len();
        let public_body = json!({
            "listeners": count,
            "now": now.clone(),
        })
        .to_string();
        self.public_frame = Bytes::from(format!("data: {public_body}\n\n"));

        let preview: Vec<_> = self.queue.iter().take(25).map(|n| np_json(n)).collect();
        let listener_list: Vec<_> = self.listeners.iter().map(|l| listener_json(l)).collect();
        let body = json!({
            "listeners": count,
            "now": now,
            "queue": preview,
            "queue_total": self.queue.len(),
            "listener_list": listener_list,
        })
        .to_string();

        self.json = Bytes::from(body.clone());
        self.frame = Bytes::from(format!("data: {body}\n\n"));
    }

    fn json(&self) -> Bytes {
        self.json.clone()
    }
}

fn listener_json(l: &Listener) -> serde_json::Value {
    json!({
        "id": l.id,
        "addr": l.addr,
        "user_agent": l.user_agent,
        "since": l.connected_at,
    })
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn np_json(n: &NowPlaying) -> serde_json::Value {
    json!({
        "artist": n.artist,
        "title": n.title,
        "file": n.file,
        "format": n.format,
    })
}

/// A change the UI makes that the prefetch thread needs to fold into its
/// in-memory library/playlist. The station no longer rescans the directory on
/// every track change; it only reacts to these notifications.
pub enum StationCmd {
    AddTrack(PathBuf),
    AddJingle(PathBuf),
    Remove(PathBuf),
    Rename { from: PathBuf, to: PathBuf },
}

pub struct Shared {
    pub head: Bytes,
    pub dir: PathBuf,
    pub hls: Hls,
    hls_enabled: AtomicBool,
    ring: Mutex<VecDeque<Page>>,
    tx: broadcast::Sender<Page>,
    status: watch::Sender<Status>,
    cmd: Mutex<mpsc::Sender<StationCmd>>,
    next_listener_id: AtomicU64,
    /// Unix milliseconds at audio-timeline zero, set when pacing starts.
    /// Zero until the station has primed its ten-second ring.
    audio_epoch_ms: AtomicU64,
    /// Tracks already handed to the encoder but not yet announced as playing.
    /// They are still upcoming from the listener's point of view.
    inflight: Mutex<VecDeque<(u64, Arc<NowPlaying>)>>,
    /// The prefetch queue's display form.
    remaining: Mutex<Vec<Arc<NowPlaying>>>,
}

/// RAII handle for one registered listener. Dropping it removes the entry, so
/// a closed socket cannot leave a ghost in the admin list.
pub struct ListenerGuard {
    shared: Arc<Shared>,
    listener: Arc<Listener>,
}

impl ListenerGuard {
    /// Receives `true` when an admin asks this connection to stop.
    pub fn disconnect_signal(&self) -> watch::Receiver<bool> {
        self.listener.disconnect.subscribe()
    }
}

impl Drop for ListenerGuard {
    fn drop(&mut self) {
        let id = self.listener.id;
        self.shared.update(|s| s.listeners.retain(|l| l.id != id));
    }
}

impl Shared {
    /// Toggling resets the segmenter so a half-built segment from the other
    /// mode cannot leak into the playlist.
    pub fn set_hls_enabled(&self, enabled: bool) {
        if self.hls_enabled.swap(enabled, Ordering::Relaxed) != enabled {
            self.hls.reset();
        }
    }

    fn set_audio_epoch(&self, unix_ms: u64) {
        self.audio_epoch_ms.store(unix_ms, Ordering::Relaxed);
    }

    /// Wall-clock seconds at audio-timeline zero, once pacing has started.
    fn audio_epoch_secs(&self) -> Option<f64> {
        let ms = self.audio_epoch_ms.load(Ordering::Relaxed);
        (ms != 0).then_some(ms as f64 / 1000.0)
    }

    fn set_remaining(&self, remaining: Vec<Arc<NowPlaying>>) {
        *self.remaining.lock().unwrap() = remaining;
        self.refresh_queue();
    }

    fn add_inflight(&self, id: u64, np: Arc<NowPlaying>) {
        self.inflight.lock().unwrap().push_back((id, np));
        self.refresh_queue();
    }

    fn remove_inflight(&self, id: u64) {
        self.inflight.lock().unwrap().retain(|(other, _)| *other != id);
        self.refresh_queue();
    }

    /// Remove a track from the upcoming view. Returns its current display
    /// entry, if it was still waiting to be announced.
    fn take_inflight(&self, id: u64) -> Option<Arc<NowPlaying>> {
        let removed = {
            let mut inflight = self.inflight.lock().unwrap();
            let pos = inflight.iter().position(|(other, _)| *other == id)?;
            inflight.remove(pos).map(|(_, np)| np)
        };
        self.refresh_queue();
        removed
    }

    /// In-flight tracks are still upcoming from the listener's point of view.
    /// They must stay in the published queue until `announce_due` makes them
    /// now-playing, otherwise a prefetch that runs up to sixty seconds ahead
    /// hides them from the UI and from rename/delete updates.
    fn refresh_queue(&self) {
        let queue = {
            let inflight = self.inflight.lock().unwrap();
            let remaining = self.remaining.lock().unwrap();
            inflight
                .iter()
                .map(|(_, np)| Arc::clone(np))
                .chain(remaining.iter().cloned())
                .collect()
        };
        self.set_queue(queue);
    }

    pub fn forget_file(&self, path: &Path) {
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            return;
        };
        self.inflight.lock().unwrap().retain(|(_, np)| np.file != name);
        self.remaining.lock().unwrap().retain(|np| np.file != name);
        self.update(|s| {
            if s.now.as_ref().is_some_and(|np| np.file == name) {
                s.now = None;
            }
        });
        self.refresh_queue();
    }

    fn push(&self, seq: u64, page: Bytes, hls: Option<(u64, Bytes)>) {
        {
            let mut ring = self.ring.lock().unwrap();
            if ring.len() >= BURST_PAGES as usize {
                ring.pop_front();
            }
            ring.push_back((seq, page.clone()));
        }
        if self.hls_enabled.load(Ordering::Relaxed)
            && let Some((granule, payload)) = hls
        {
            self.hls.push_packet(granule, payload);
        }
        let _ = self.tx.send((seq, page));
    }

    fn update(&self, f: impl FnOnce(&mut Status)) {
        self.status.send_if_modified(|s| {
            let before = (s.frame.clone(), s.public_frame.clone());
            f(s);
            s.render();
            s.frame != before.0 || s.public_frame != before.1
        });
    }

    pub fn register_listener(self: &Arc<Self>, addr: String, user_agent: String) -> ListenerGuard {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        let listener = Arc::new(Listener {
            id,
            addr,
            user_agent,
            connected_at: unix_millis(),
            disconnect: watch::channel(false).0,
        });
        self.update(|s| s.listeners.push(Arc::clone(&listener)));
        ListenerGuard {
            shared: Arc::clone(self),
            listener,
        }
    }

    /// Ask one stream to stop. Returns false when the listener already left.
    pub fn disconnect_listener(&self, id: u64) -> bool {
        let mut found = false;
        self.update(|s| {
            for listener in s.listeners.iter().filter(|l| l.id == id) {
                listener.disconnect.send_replace(true);
                found = true;
            }
        });
        found
    }

    fn set_now_playing(&self, np: Arc<NowPlaying>) {
        self.update(|s| s.now = Some(np));
    }

    fn clear_now_playing(&self) {
        self.update(|s| s.now = None);
    }

    fn set_queue(&self, queue: Vec<Arc<NowPlaying>>) {
        self.update(|s| s.queue = queue);
    }

    /// The current status as an admin JSON object (without the SSE `data:`
    /// wrapper). Public callers should use the public SSE frame instead.
    pub fn status_json(&self) -> Bytes {
        self.status.subscribe().borrow().json()
    }

    /// Full-ish queue snapshot for the paginated admin queue endpoint.
    pub fn queue_snapshot(&self) -> Vec<Arc<NowPlaying>> {
        self.status.subscribe().borrow().queue.clone()
    }

    /// Tell the prefetch thread about a file the UI added or removed.
    pub fn notify(&self, cmd: StationCmd) {
        let _ = self.cmd.lock().unwrap().send(cmd);
    }

    /// Reflect a completed on-disk rename in the published queue right away,
    /// then let the prefetch thread update its in-memory library.
    pub fn file_renamed(&self, from: &Path, to: &Path) {
        let old_name = match from.file_name() {
            Some(name) => name.to_string_lossy().into_owned(),
            None => return,
        };
        let jingle = to.starts_with(jingles_dir(&self.dir));
        let renamed = now_playing_for(to, jingle);
        {
            let mut inflight = self.inflight.lock().unwrap();
            for (_, np) in inflight.iter_mut() {
                if np.file == old_name {
                    *np = Arc::clone(&renamed);
                }
            }
        }
        {
            let mut remaining = self.remaining.lock().unwrap();
            for np in remaining.iter_mut() {
                if np.file == old_name {
                    *np = Arc::clone(&renamed);
                }
            }
        }
        self.update(|s| {
            if s.now.as_ref().is_some_and(|n| n.file == old_name) {
                s.now = Some(Arc::clone(&renamed));
            }
        });
        self.refresh_queue();
        self.notify(StationCmd::Rename {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        });
    }

    pub fn watch_status(&self) -> watch::Receiver<Status> {
        self.status.subscribe()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Page> {
        self.tx.subscribe()
    }

    pub fn burst(&self) -> Vec<Page> {
        self.ring.lock().unwrap().iter().cloned().collect()
    }
}

enum Feed {
    RadioGone,
    Skip(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TrackKind {
    Normal,
    Jingle,
}

#[derive(Clone)]
struct Track {
    path: PathBuf,
    artist: String,
    kind: TrackKind,
    /// Full-file duration in seconds, read from the container header. Trimmed
    /// playback can be shorter; the scheduler treats this as a projection and
    /// repairs from the mixer clock if it drifts.
    duration: f64,
    /// Newly uploaded and not yet shown to the UI at the head of the queue.
    fresh: bool,
}

/// Duration used when a container does not report one. FLAC STREAMINFO and
/// WAV headers normally do; this keeps scheduling total for damaged files.
const UNKNOWN_DURATION: f64 = 180.0;

/// Read duration from the container header without decoding any audio.
fn probe_duration(path: &Path) -> Option<f64> {
    if is_wav(path) {
        let fmt = wav::wav_format(path).ok()?;
        let denom = fmt.block_align as f64 * fmt.sample_rate as f64;
        if denom <= 0.0 || fmt.data_len == 0 {
            return None;
        }
        Some(fmt.data_len as f64 / denom)
    } else if wav::is_flac(path) {
        let decoder = crate::flac::Decoder::new(File::open(path).ok()?).ok()?;
        let info = decoder.stream_info();
        if info.sample_rate == 0 {
            None
        } else {
            Some(info.total_samples? as f64 / info.sample_rate as f64)
        }
    } else {
        None
    }
}

fn track_duration(path: &Path) -> f64 {
    probe_duration(path).unwrap_or(UNKNOWN_DURATION)
}

fn parse_artist(path: &Path) -> Option<String> {
    split_stem(path).map(|(artist, _)| normalize_artist(&artist))
}

fn split_stem(path: &Path) -> Option<(String, String)> {
    let stem = path.file_stem()?.to_string_lossy();
    let mut s = stem.trim().to_string();
    let digits = s.bytes().take_while(|b| b.is_ascii_digit()).count();
    if (1..=3).contains(&digits) && s.as_bytes().get(digits) == Some(&b'.') {
        s = s[digits + 1..].trim_start_matches([' ', '_']).to_string();
    }
    let cut = match (s.find("_-_"), s.find(" - ")) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return None,
    };
    Some((s[..cut].to_string(), s[cut + 3..].to_string()))
}

fn tidy(s: &str) -> String {
    s.replace('_', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize_artist(s: &str) -> String {
    tidy(s).to_lowercase()
}

fn display_names(track: &Track) -> (String, String) {
    match track.kind {
        TrackKind::Jingle => {
            let stem = track
                .path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            ("Jingle".to_string(), tidy(&stem))
        }
        TrackKind::Normal => match split_stem(&track.path) {
            Some((artist, title)) => (tidy(&artist), tidy(&title)),
            None => {
                let stem = track
                    .path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (String::new(), tidy(&stem))
            }
        },
    }
}

fn build_track(path: PathBuf) -> Track {
    let artist = parse_artist(&path).unwrap_or_else(|| {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        normalize_artist(&stem)
    });

    let duration = track_duration(&path);
    Track {
        path,
        artist,
        kind: TrackKind::Normal,
        duration,
        fresh: false,
    }
}

fn build_jingle(path: PathBuf) -> Track {
    let duration = track_duration(&path);
    Track {
        path,
        artist: "jingle".to_string(),
        kind: TrackKind::Jingle,
        duration,
        fresh: false,
    }
}

/// Track shaped only for display, without touching the file. Rename and
/// now-playing paths need names, not durations.
fn display_track(path: &Path, jingle: bool) -> Track {
    Track {
        path: path.to_path_buf(),
        artist: String::new(),
        kind: if jingle {
            TrackKind::Jingle
        } else {
            TrackKind::Normal
        },
        duration: 0.0,
        fresh: false,
    }
}

fn scan_media(dir: &Path) -> Vec<PathBuf> {
    fn rec(d: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let p = e.path();
            if ft.is_dir() {
                rec(&p, out);
            } else if ft.is_file() && wav::is_media(&p) {
                out.push(p);
            }
        }
    }

    let mut v = Vec::new();
    rec(dir, &mut v);
    v.sort();
    v
}

fn jingles_dir(dir: &Path) -> PathBuf {
    dir.join("jingles")
}

fn warn_missing_artist(track: &Track) {
    if track.kind == TrackKind::Normal && parse_artist(&track.path).is_none() {
        eprintln!(
            "[station] no artist separator in {} — treating the whole name as the artist",
            track.path.display()
        );
    }
}

fn scan_library(dir: &Path) -> Vec<Track> {
    let jd = jingles_dir(dir);

    scan_media(dir)
        .into_iter()
        .filter(|p| !p.starts_with(&jd))
        .map(build_track)
        .collect()
}

fn scan_jingles(dir: &Path) -> Vec<Track> {
    let jd = jingles_dir(dir);
    if !jd.is_dir() {
        return Vec::new();
    }

    scan_media(&jd).into_iter().map(build_jingle).collect()
}

/// Shuffle the library into a fresh preference order.
fn shuffled_tracks(library: &[Track], rng: &mut impl Rng) -> Vec<Track> {
    let mut v = library.to_vec();
    v.shuffle(rng);
    v
}

const COOLDOWN_MS: u64 = 3_600_000;

fn seconds_to_ms(secs: f64) -> u64 {
    if !secs.is_finite() {
        return 0;
    }
    (secs.max(0.0) * 1000.0).round() as u64
}

/// Order `tracks` against a playout clock so every artist sits out its
/// one-hour cooldown, while keeping the shuffled preference order wherever
/// possible.
///
/// Each artist is a queue of tracks in preference order. A min-heap of
/// eligible artists is keyed by the preference rank of their next track, so
/// when several artists are eligible the earliest-shuffled one wins. Artists
/// still in cooldown wait in a second heap keyed by the time they become
/// eligible. If no artist is eligible, the one that becomes eligible soonest
/// is scheduled anyway: silence is not an option, and the alternative would
/// be to stall the station. The returned count is the number of forced
/// (cooldown-violating) slots.
fn schedule_tracks(
    tracks: Vec<Track>,
    clock: f64,
    last_end: &HashMap<String, f64>,
) -> (VecDeque<Track>, usize) {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    let mut slots: Vec<Option<Track>> = tracks.into_iter().map(Some).collect();
    let mut artists: Vec<(String, VecDeque<(usize, usize)>)> = Vec::new();
    let mut artist_ids: HashMap<String, usize> = HashMap::new();

    for (idx, slot) in slots.iter().enumerate() {
        let artist = slot.as_ref().expect("just built").artist.clone();
        let aid = *artist_ids.entry(artist.clone()).or_insert_with(|| {
            artists.push((artist, VecDeque::new()));
            artists.len() - 1
        });
        artists[aid].1.push_back((idx, idx));
    }

    // (preference rank, artist id)
    let mut ready: BinaryHeap<Reverse<(usize, usize)>> = BinaryHeap::new();
    // (ready time in ms, preference rank, artist id)
    let mut waiting: BinaryHeap<Reverse<(u64, usize, usize)>> = BinaryHeap::new();
    let start_ms = seconds_to_ms(clock);
    let mut now = start_ms;

    for (aid, (artist, queue)) in artists.iter().enumerate() {
        let (_, rank) = queue[0];
        let ready_ms = last_end
            .get(artist)
            .map_or(0, |end| seconds_to_ms(*end).saturating_add(COOLDOWN_MS));
        if ready_ms <= start_ms {
            ready.push(Reverse((rank, aid)));
        } else {
            waiting.push(Reverse((ready_ms, rank, aid)));
        }
    }

    let mut out = VecDeque::with_capacity(slots.len());
    let mut forced = 0usize;
    while out.len() < slots.len() {
        while let Some(&Reverse((ready_ms, _, _))) = waiting.peek() {
            if ready_ms > now {
                break;
            }
            let Reverse((_, rank, aid)) = waiting.pop().expect("peeked");
            ready.push(Reverse((rank, aid)));
        }

        let aid = match ready.pop() {
            Some(Reverse((_, aid))) => aid,
            None => match waiting.pop() {
                Some(Reverse((_, _, aid))) => {
                    forced += 1;
                    aid
                }
                None => break,
            },
        };

        let (idx, _) = artists[aid]
            .1
            .pop_front()
            .expect("heap entry always has a track");
        let track = slots[idx].take().expect("each track scheduled once");
        now = now.saturating_add(seconds_to_ms(track.duration));
        out.push_back(track);

        if let Some(&(_, rank)) = artists[aid].1.front() {
            waiting.push(Reverse((now.saturating_add(COOLDOWN_MS), rank, aid)));
        }
    }

    (out, forced)
}

/// Replace `playlist` with a schedule over either the current order (repair)
/// or a fresh shuffle of the library (startup, wrap-around, reset).
fn note_forced(forced: usize, warned: &mut bool) {
    if forced == 0 {
        *warned = false;
    } else if !*warned {
        eprintln!(
            "[prefetch] cooldown could not be met for {forced} slot(s) — too few artists to fill the hour"
        );
        *warned = true;
    }
}

fn reschedule(
    playlist: &mut VecDeque<Track>,
    library: &[Track],
    rng: &mut impl Rng,
    clock: f64,
    last_end: &HashMap<String, f64>,
    fresh: bool,
) -> usize {
    let order: Vec<Track> = if fresh || playlist.is_empty() {
        shuffled_tracks(library, rng)
    } else {
        playlist.drain(..).collect()
    };
    let (scheduled, forced) = schedule_tracks(order, clock, last_end);
    *playlist = scheduled;
    forced
}

fn insert_random(playlist: &mut VecDeque<Track>, mut new_tracks: Vec<Track>, rng: &mut impl Rng) {
    let new_count = new_tracks.len();
    if new_count == 0 {
        return;
    }

    new_tracks.shuffle(rng);

    let old_len = playlist.len();
    let final_len = old_len + new_count;

    let mut positions: Vec<usize> = (0..final_len).collect();
    positions.shuffle(rng);
    positions.truncate(new_count);
    positions.sort_unstable();

    let rebuilt = {
        let mut old_iter = playlist.drain(..);
        let mut new_iter = new_tracks.into_iter();
        let mut rebuilt = VecDeque::with_capacity(final_len);
        let mut next_new = 0usize;

        for pos in 0..final_len {
            if next_new < new_count && positions[next_new] == pos {
                rebuilt.push_back(new_iter.next().expect("new track"));
                next_new += 1;
            } else {
                rebuilt.push_back(old_iter.next().expect("old track"));
            }
        }

        rebuilt
    };

    *playlist = rebuilt;
}

fn cooled(track: &Track, clock: f64, last_end: &HashMap<String, f64>) -> bool {
    if track.kind == TrackKind::Jingle {
        return true;
    }

    match last_end.get(&track.artist) {
        None => true,
        Some(&end) => clock - end >= ARTIST_COOLDOWN,
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn next_hour_after(now: u64) -> u64 {
    (now / 3600 + 1) * 3600
}

/// A jingle is due at the first playout boundary at or after its hour.
/// `clock` is the projected start of the next slot on the audio timeline.
fn jingle_due(clock: f64, epoch: Option<f64>, next_jingle: u64) -> bool {
    epoch.is_some_and(|epoch| clock >= next_jingle as f64 - epoch)
}

fn choose_jingle(jingles: &[Track], last: Option<&PathBuf>, rng: &mut impl Rng) -> Option<Track> {
    if jingles.is_empty() {
        return None;
    }

    if jingles.len() == 1 {
        return Some(jingles[0].clone());
    }

    let mut candidates: Vec<&Track> = jingles.iter().filter(|t| Some(&t.path) != last).collect();

    if candidates.is_empty() {
        candidates = jingles.iter().collect();
    }

    Some((**candidates.choose(rng).expect("non-empty")).clone())
}

fn db(x: f32) -> f32 {
    10f32.powf(x / 20.0)
}

#[cfg(test)]
struct Trim {
    start: usize,
    end: usize,
    ring: usize,
}


#[cfg(test)]
fn analyze(path: &Path) -> Result<Trim, String> {
    if is_wav(path) {
        analyze_wav(path)
    } else {
        analyze_flac(path)
    }
}

#[cfg(test)]
fn analyze_flac(path: &Path) -> Result<Trim, String> {
    let file = File::open(path).map_err(|e| format!("open: {e}"))?;
    let mut decoder = crate::flac::Decoder::new(file).map_err(|e| format!("open: {e}"))?;
    let fs = decoder.stream_info().sample_rate as usize;
    if fs == 0 {
        return Err("flac sample rate 0".into());
    }

    let (sil, ring_thr) = (db(SILENCE_DB), db(RING_DB));
    let win = (fs / 50).max(1);
    let mut peaks: Vec<f32> = vec![0.0];
    let (mut first, mut last) = (None::<usize>, 0usize);
    let mut pos = 0usize;
    let mut frame = crate::flac::Frame::empty();
    let mut window = 0usize;
    let mut window_end = win;
    loop {
        if !decoder
            .read_frame(&mut frame)
            .map_err(|e| format!("decode: {e}"))?
        {
            break;
        }
        if frame.sample_rate as usize != fs {
            return Err("flac sample rate changes mid-stream".into());
        }
        let bits = frame.bits_per_sample;
        if bits == 0 || bits > 32 {
            return Err(format!("unsupported bit depth {bits}"));
        }
        let scale = 1.0f32 / (1u64 << (bits - 1)) as f32;
        let channels = frame.channels() as usize;
        let left = frame.channel(0);
        let right = if channels > 1 {
            frame.channel(1)
        } else {
            left
        };
        for i in 0..frame.block_size {
            let l = normalize(left[i], scale).abs();
            let r = normalize(right[i], scale).abs();
            let p = l.max(r);
            let f = pos + i;
            if p > sil {
                first.get_or_insert(f);
                last = f;
            }
            if f >= window_end {
                window = f / win;
                window_end = (window + 1) * win;
                if window >= peaks.len() {
                    peaks.resize(window + 1, 0.0);
                }
            }
            peaks[window] = peaks[window].max(p);
        }
        pos += frame.block_size;
    }
    let start = first.ok_or("file is entirely silent")?;
    let end = last + 1;
    let mut ring_start = end;
    for w in (0..peaks.len()).rev() {
        if peaks[w] >= ring_thr {
            ring_start = ((w + 1) * win).min(end);
            break;
        }
    }
    let cap = (MAX_OVERLAP_SECS * fs as f64) as usize;
    let ring = (end - ring_start).min(cap).min((end - start) / 4);
    Ok(Trim { start, end, ring })
}

#[cfg(test)]
fn analyze_wav(path: &Path) -> Result<Trim, String> {
    let mut stream = WavStream::open(path)?;
    let fs = stream.format().sample_rate as usize;
    if fs == 0 {
        return Err("wav sample rate 0".into());
    }

    let (sil, ring_thr) = (db(SILENCE_DB), db(RING_DB));
    let win = (fs / 50).max(1);

    let mut peaks: Vec<f32> = vec![0.0];
    let mut first: Option<usize> = None;
    let mut last = 0usize;
    let mut pos = 0usize;
    let mut window = 0usize;
    let mut window_end = win;

    loop {
        let stereo = stream.read_stereo_frames(4096)?;
        if stereo.is_empty() {
            break;
        }
        let frames = stereo.len() / 2;
        for i in 0..frames {
            let l = stereo[i * 2].abs();
            let r = stereo[i * 2 + 1].abs();
            let p = l.max(r);
            let f = pos + i;

            if p > sil {
                first.get_or_insert(f);
                last = f;
            }

            if f >= window_end {
                window = f / win;
                window_end = (window + 1) * win;
                if window >= peaks.len() {
                    peaks.resize(window + 1, 0.0);
                }
            }
            peaks[window] = peaks[window].max(p);
        }

        pos += frames;
    }

    let start = first.ok_or("file is entirely silent")?;
    let end = last + 1;

    let mut ring_start = end;
    for w in (0..peaks.len()).rev() {
        if peaks[w] >= ring_thr {
            ring_start = ((w + 1) * win).min(end);
            break;
        }
    }

    let cap = (MAX_OVERLAP_SECS * fs as f64) as usize;
    let ring = (end - ring_start).min(cap).min((end - start) / 4);

    Ok(Trim { start, end, ring })
}

struct Mixer {
    tx: mpsc::SyncSender<Block>,
    buf: Vec<f32>,
    /// Samples at the front of `buf` already handed to the encoder. They are
    /// reclaimed in bulk, so `commit` does not have to move the held tail.
    head: usize,
    cursor: usize,
    hold: usize,
    pushed: usize,
    pending: Option<u64>,
}

impl Mixer {
    fn new(tx: mpsc::SyncSender<Block>) -> Self {
        Mixer {
            tx,
            buf: Vec::new(),
            head: 0,
            cursor: 0,
            hold: 0,
            pushed: 0,
            pending: None,
        }
    }

    fn len(&self) -> usize {
        self.buf.len() - self.head
    }

    fn begin_track(&mut self, ring_frames: usize, len_frames: usize, play_id: u64) {
        let h = self
            .hold
            .min(self.len())
            .min(len_frames.saturating_mul(2));
        self.cursor = self.len() - h;
        self.hold = ring_frames.saturating_mul(2);
        self.pending = Some(play_id);
    }

    fn set_hold(&mut self, ring_frames: usize) {
        self.hold = ring_frames.saturating_mul(2);
    }

    fn abort_track(&mut self) {
        self.hold = 0;
        self.cursor = self.len();
    }

    fn add(&mut self, s: &[f32]) -> Result<(), Feed> {
        let ov = (self.len() - self.cursor).min(s.len());
        let start = self.head + self.cursor;
        for (d, x) in self.buf[start..start + ov].iter_mut().zip(&s[..ov]) {
            *d += *x;
        }
        self.buf.extend_from_slice(&s[ov..]);
        self.cursor += s.len();
        self.commit()
    }

    fn commit(&mut self) -> Result<(), Feed> {
        let limit = self.cursor.min(self.len().saturating_sub(self.hold));
        let n = limit / FRAME_FLOATS;
        let emitted = n * FRAME_FLOATS;
        for chunk in self.buf[self.head..self.head + emitted].chunks(FRAME_FLOATS) {
            self.tx
                .send(Block {
                    pcm: chunk.to_vec(),
                    play: self.pending.take(),
                })
                .map_err(|_| Feed::RadioGone)?;
        }
        self.head += emitted;
        self.cursor -= emitted;
        self.pushed += n;
        // Reclaim consumed samples once they reach half the buffer; the tail
        // then moves at most once per two samples of emitted audio.
        if self.head * 2 >= self.buf.len() {
            self.buf.drain(..self.head);
            self.head = 0;
        }
        Ok(())
    }

    fn end_secs(&self) -> f64 {
        ((self.pushed * FRAME_FLOATS + self.len()) / 2) as f64 / SAMPLE_RATE as f64
    }

    fn next_start_secs(&self) -> f64 {
        self.end_secs() - (self.hold.min(self.len()) / 2) as f64 / SAMPLE_RATE as f64
    }
}

const STREAM_TAIL_SECS: usize = 15;
const STREAM_TAIL_FRAMES: usize = STREAM_TAIL_SECS * SAMPLE_RATE as usize;
const STREAM_TAIL_FLOATS: usize = STREAM_TAIL_FRAMES * 2;

struct OutputTail {
    data: Vec<f32>,
    head: usize,
    total_frames: usize,
}

impl OutputTail {
    fn new() -> Self {
        OutputTail {
            data: Vec::with_capacity(STREAM_TAIL_FLOATS),
            head: 0,
            total_frames: 0,
        }
    }

    fn push(&mut self, samples: &[f32], mixer: &mut Mixer) -> Result<(), Feed> {
        self.data.extend_from_slice(samples);
        self.total_frames += samples.len() / 2;

        let live = self.data.len() - self.head;
        if live > STREAM_TAIL_FLOATS {
            let excess = live - STREAM_TAIL_FLOATS;
            mixer.add(&self.data[self.head..self.head + excess])?;
            self.head += excess;
        }
        if self.head * 2 >= self.data.len() {
            self.data.drain(..self.head);
            self.head = 0;
        }
        Ok(())
    }

    fn frames(&self) -> usize {
        (self.data.len() - self.head) / 2
    }

    fn finish(
        self,
        fs: usize,
        start: usize,
        end: usize,
        ring: usize,
        mixer: &mut Mixer,
    ) -> Result<(), Feed> {
        let end_output = scaled_frames(fs, end.saturating_sub(start));
        let tail_start = self.total_frames.saturating_sub(self.frames());
        let keep = end_output
            .min(self.total_frames)
            .saturating_sub(tail_start);
        let ring_output = scaled_frames(fs, ring).min(keep);

        mixer.set_hold(ring_output);
        if keep > 0 {
            let begin = self.head;
            mixer.add(&self.data[begin..begin + keep * 2])?;
        }
        Ok(())
    }
}

fn emit_output(
    staged: &[f32],
    resampler: &mut Option<Resampler>,
    out: &mut Vec<f32>,
    tail: &mut OutputTail,
    mixer: &mut Mixer,
) -> Result<(), Feed> {
    if staged.is_empty() {
        return Ok(());
    }
    match resampler {
        None => tail.push(staged, mixer),
        Some(r) => {
            out.clear();
            r.resample(staged, out);
            tail.push(out, mixer)
        }
    }
}

fn flush_output(
    resampler: &mut Option<Resampler>,
    out: &mut Vec<f32>,
    tail: &mut OutputTail,
    mixer: &mut Mixer,
) -> Result<(), Feed> {
    if let Some(r) = resampler {
        out.clear();
        r.flush(out);
        tail.push(out, mixer)?;
    }
    Ok(())
}

fn trim_from_peaks(
    peaks: &[f32],
    win: usize,
    first: Option<usize>,
    last: usize,
    fs: usize,
) -> Result<(usize, usize, usize), String> {
    let start = first.ok_or("file is entirely silent")?;
    let end = last + 1;
    let mut ring_start = end;
    for w in (0..peaks.len()).rev() {
        if peaks[w] >= db(RING_DB) {
            ring_start = ((w + 1) * win).min(end);
            break;
        }
    }
    let cap = (MAX_OVERLAP_SECS * fs as f64) as usize;
    let ring = (end - ring_start).min(cap).min((end - start) / 4);
    Ok((start, end, ring))
}

fn stream_file(track: &Track, mixer: &mut Mixer, play_id: u64) -> Result<(), Feed> {
    let result = if is_wav(&track.path) {
        stream_wav(track, mixer, play_id)
    } else {
        stream_flac(track, mixer, play_id)
    };

    pagecache::forget(&track.path);
    result
}

fn stream_flac(track: &Track, mixer: &mut Mixer, play_id: u64) -> Result<(), Feed> {
    let file = File::open(&track.path).map_err(|e| Feed::Skip(format!("open: {e}")))?;
    let mut decoder =
        crate::flac::Decoder::new(file).map_err(|e| Feed::Skip(format!("open: {e}")))?;
    let info = decoder.stream_info();
    let fs = info.sample_rate as usize;
    let bps = info.bits_per_sample;
    let chs = info.channels as usize;

    if fs == 0 {
        return Err(Feed::Skip("flac sample rate 0".into()));
    }
    if bps == 0 || bps > 32 {
        return Err(Feed::Skip(format!("unsupported bit depth {bps}")));
    }

    let format = format!(
        "FLAC · {} kHz · {}-bit · {} ch",
        fs as f64 / 1000.0,
        bps,
        chs
    );

    eprintln!(
        "[prefetch] now playing: {} [{}] ({})",
        track.path.display(),
        track.artist,
        format
    );

    let mut resampler = if fs == SAMPLE_RATE as usize {
        None
    } else {
        Some(
            Resampler::new(fs as u32)
                .ok_or_else(|| Feed::Skip(format!("unsupported sample rate {fs}")))?,
        )
    };

    let sil = db(SILENCE_DB);
    let win = (fs / 50).max(1);
    let mut peaks: Vec<f32> = vec![0.0];
    let mut first: Option<usize> = None;
    let mut last = 0usize;
    let mut pos = 0usize;
    let mut window = 0usize;
    let mut window_end = win;

    let mut frame = crate::flac::Frame::empty();
    let mut staged: Vec<f32> = Vec::new();
    let mut out: Vec<f32> = Vec::new();
    let mut tail = OutputTail::new();
    let mut started = false;

    loop {
        if !decoder
            .read_frame(&mut frame)
            .map_err(|e| Feed::Skip(format!("decode: {e}")))?
        {
            break;
        }
        if frame.sample_rate as usize != fs {
            return Err(Feed::Skip("flac sample rate changes mid-stream".into()));
        }
        let bits = frame.bits_per_sample;
        if bits == 0 || bits > 32 {
            return Err(Feed::Skip(format!("unsupported bit depth {bits}")));
        }
        let scale = 1.0f32 / (1u64 << (bits - 1)) as f32;
        let channels = frame.channels() as usize;
        let bsz = frame.block_size;
        let left = frame.channel(0);
        let right = if channels > 1 {
            frame.channel(1)
        } else {
            left
        };

        let mut first_in_block = None;
        for i in 0..bsz {
            let p = normalize(left[i], scale)
                .abs()
                .max(normalize(right[i], scale).abs());
            let frame_pos = pos + i;
            if p > sil {
                first.get_or_insert(frame_pos);
                last = frame_pos;
                first_in_block.get_or_insert(i);
            }
            if frame_pos >= window_end {
                window = frame_pos / win;
                window_end = (window + 1) * win;
                if window >= peaks.len() {
                    peaks.resize(window + 1, 0.0);
                }
            }
            peaks[window] = peaks[window].max(p);
        }

        let feed_from = if started {
            0
        } else if let Some(first_frame) = first_in_block {
            mixer.begin_track(0, usize::MAX / 2, play_id);
            started = true;
            first_frame
        } else {
            pos += bsz;
            continue;
        };

        staged.clear();
        staged.reserve((bsz - feed_from) * 2);
        for i in feed_from..bsz {
            staged.push(normalize(left[i], scale));
            staged.push(normalize(right[i], scale));
        }
        pos += bsz;
        emit_output(&staged, &mut resampler, &mut out, &mut tail, mixer)?;
    }

    if !started {
        return Err(Feed::Skip("file is entirely silent".into()));
    }

    flush_output(&mut resampler, &mut out, &mut tail, mixer)?;
    let (start, end, ring) = trim_from_peaks(&peaks, win, first, last, fs)
        .map_err(|e| Feed::Skip(e.to_string()))?;
    tail.finish(fs, start, end, ring, mixer)
}

fn stream_wav(track: &Track, mixer: &mut Mixer, play_id: u64) -> Result<(), Feed> {
    let mut s = WavStream::open(&track.path).map_err(Feed::Skip)?;
    let fmt = s.format();
    let fs = fmt.sample_rate as usize;

    if fs == 0 {
        return Err(Feed::Skip("wav sample rate 0".into()));
    }

    let format = wav_format_label(&fmt);

    eprintln!(
        "[prefetch] now playing: {} [{}] ({})",
        track.path.display(),
        track.artist,
        format
    );

    let mut resampler = if fs == SAMPLE_RATE as usize {
        None
    } else {
        Some(
            Resampler::new(fs as u32)
                .ok_or_else(|| Feed::Skip(format!("unsupported sample rate {fs}")))?,
        )
    };

    let sil = db(SILENCE_DB);
    let win = (fs / 50).max(1);
    let mut peaks: Vec<f32> = vec![0.0];
    let mut first: Option<usize> = None;
    let mut last = 0usize;
    let mut pos = 0usize;
    let mut window = 0usize;
    let mut window_end = win;
    let mut staged: Vec<f32> = Vec::new();
    let mut out: Vec<f32> = Vec::new();
    let mut tail = OutputTail::new();
    let mut started = false;

    loop {
        let stereo = s.read_stereo_frames(4096).map_err(Feed::Skip)?;
        if stereo.is_empty() {
            break;
        }

        let frames = stereo.len() / 2;
        let mut first_in_block = None;
        for i in 0..frames {
            let p = stereo[i * 2].abs().max(stereo[i * 2 + 1].abs());
            let frame_pos = pos + i;
            if p > sil {
                first.get_or_insert(frame_pos);
                last = frame_pos;
                first_in_block.get_or_insert(i);
            }
            if frame_pos >= window_end {
                window = frame_pos / win;
                window_end = (window + 1) * win;
                if window >= peaks.len() {
                    peaks.resize(window + 1, 0.0);
                }
            }
            peaks[window] = peaks[window].max(p);
        }

        let feed_from = if started {
            0
        } else if let Some(first_frame) = first_in_block {
            mixer.begin_track(0, usize::MAX / 2, play_id);
            started = true;
            first_frame
        } else {
            pos += frames;
            continue;
        };

        staged.clear();
        staged.reserve((frames - feed_from) * 2);
        for i in feed_from..frames {
            staged.push(stereo[i * 2]);
            staged.push(stereo[i * 2 + 1]);
        }
        pos += frames;
        emit_output(&staged, &mut resampler, &mut out, &mut tail, mixer)?;
    }

    if !started {
        return Err(Feed::Skip("file is entirely silent".into()));
    }

    flush_output(&mut resampler, &mut out, &mut tail, mixer)?;
    let (start, end, ring) = trim_from_peaks(&peaks, win, first, last, fs)
        .map_err(|e| Feed::Skip(e.to_string()))?;
    tail.finish(fs, start, end, ring, mixer)
}

fn normalize(s: i32, scale: f32) -> f32 {
    s as f32 * scale
}

fn io_err(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

fn build_encoder() -> io::Result<Encoder> {
    Ok(Encoder::new())
}

fn encode_one(
    enc: &mut Encoder,
    pw: &mut PageWriter,
    frames_done: &mut u64,
    pkt: &mut [u8],
    block: &[f32],
) -> io::Result<AudioPage> {
    let n = enc
        .encode(block, pkt)
        .ok_or_else(|| io_err("celt encode failed"))?;
    *frames_done += 1;
    let granule = PRESKIP + *frames_done * FRAME_SAMPLES as u64;
    Ok(pw.audio_page(granule, &pkt[..n]))
}

fn announce_due(
    shared: &Shared,
    upcoming: &mut VecDeque<(u64, u64)>,
    frames_done: u64,
) {
    while upcoming.front().is_some_and(|(due, _)| *due <= frames_done) {
        if let Some((_, play_id)) = upcoming.pop_front() {
            // A deleted track is no longer in flight, so it is never announced.
            if let Some(np) = shared.take_inflight(play_id) {
                shared.set_now_playing(np);
            }
        }
    }
}

fn now_playing_for(path: &Path, jingle: bool) -> Arc<NowPlaying> {
    track_now_playing(&display_track(path, jingle))
}

fn track_now_playing(t: &Track) -> Arc<NowPlaying> {
    let (artist, title) = display_names(t);
    let format = t
        .path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_uppercase())
        .unwrap_or_default();
    Arc::new(NowPlaying {
        artist,
        title,
        file: t
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        format,
    })
}

fn publish_queue(shared: &Shared, playlist: &VecDeque<Track>) {
    // Keep the snapshot bounded so a huge library does not make every queue
    // update expensive. The UI paginates this list. Tracks already in flight
    // are prepended by `Shared::refresh_queue`.
    let remaining: Vec<Arc<NowPlaying>> =
        playlist.iter().take(1000).map(track_now_playing).collect();
    shared.set_remaining(remaining);
}

fn add_jingle(path: PathBuf, jingles: &mut Vec<Track>) -> bool {
    if jingles.iter().any(|t| t.path == path) {
        return false;
    }
    eprintln!("[prefetch] added jingle: {}", path.display());
    jingles.push(build_jingle(path));
    true
}

fn apply_cmd(
    cmd: StationCmd,
    dir: &Path,
    library: &mut Vec<Track>,
    jingles: &mut Vec<Track>,
    playlist: &mut VecDeque<Track>,
    rng: &mut impl Rng,
) -> bool {
    match cmd {
        StationCmd::AddTrack(path) => {
            if path.starts_with(jingles_dir(dir)) {
                return add_jingle(path, jingles);
            }
            if library.iter().any(|t| t.path == path) {
                return false;
            }
            let mut track = build_track(path.clone());
            track.fresh = true;
            warn_missing_artist(&track);
            eprintln!("[prefetch] added: {}", path.display());
            library.push(track.clone());
            insert_random(playlist, vec![track], rng);
            true
        }
        StationCmd::AddJingle(path) => add_jingle(path, jingles),
        StationCmd::Rename { from, to } => {
            let mut changed = false;
            for track in library.iter_mut().chain(jingles.iter_mut()) {
                if track.path == from {
                    *track = match track.kind {
                        TrackKind::Jingle => build_jingle(to.clone()),
                        TrackKind::Normal => build_track(to.clone()),
                    };
                    changed = true;
                }
            }
            for track in playlist.iter_mut() {
                if track.path == from {
                    *track = match track.kind {
                        TrackKind::Jingle => build_jingle(to.clone()),
                        TrackKind::Normal => build_track(to.clone()),
                    };
                    changed = true;
                }
            }
            eprintln!("[prefetch] renamed: {} -> {}", from.display(), to.display());
            changed
        }
        StationCmd::Remove(path) => {
            let before = library.len() + jingles.len() + playlist.len();
            library.retain(|t| t.path != path);
            jingles.retain(|t| t.path != path);
            playlist.retain(|t| t.path != path);
            eprintln!("[prefetch] removed: {}", path.display());
            before != library.len() + jingles.len() + playlist.len()
        }
    }
}

struct CommandCtx<'a, R: Rng> {
    rx: &'a mpsc::Receiver<StationCmd>,
    dir: &'a Path,
    library: &'a mut Vec<Track>,
    jingles: &'a mut Vec<Track>,
    playlist: &'a mut VecDeque<Track>,
    rng: &'a mut R,
}

/// Returns `None` when the command channel closed, otherwise whether the
/// library or playlist changed and needs rescheduling.
fn drain_commands<R: Rng>(c: CommandCtx<'_, R>) -> Option<bool> {
    let mut changed = false;
    loop {
        match c.rx.try_recv() {
            Ok(cmd) => {
                changed |= apply_cmd(
                    cmd,
                    c.dir,
                    &mut *c.library,
                    &mut *c.jingles,
                    &mut *c.playlist,
                    &mut *c.rng,
                );
            }
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => return None,
        }
    }
    Some(changed)
}

fn wait_for_command<R: Rng>(c: CommandCtx<'_, R>) -> Option<bool> {
    match c.rx.recv_timeout(IDLE_WAIT) {
        Ok(cmd) => Some(apply_cmd(
            cmd,
            c.dir,
            &mut *c.library,
            &mut *c.jingles,
            &mut *c.playlist,
            &mut *c.rng,
        )),
        Err(mpsc::RecvTimeoutError::Timeout) => Some(false),
        Err(mpsc::RecvTimeoutError::Disconnected) => None,
    }
}

fn prefetch_loop(
    dir: PathBuf,
    tx: mpsc::SyncSender<Block>,
    shared: Arc<Shared>,
    cmd_rx: mpsc::Receiver<StationCmd>,
) {
    let mut library = scan_library(&dir);
    let mut jingles = scan_jingles(&dir);
    let artists: HashSet<&str> = library.iter().map(|t| t.artist.as_str()).collect();
    eprintln!(
        "[station] {} media files, {} distinct artists (parsed from filenames); library changes are driven by UI notifications for {}",
        library.len(),
        artists.len(),
        dir.display()
    );
    if library.is_empty() {
        eprintln!(
            "[prefetch] library is empty — waiting for uploads in {}",
            dir.display()
        );
    }

    for t in &library {
        warn_missing_artist(t);
    }

    let mut rng = rand::rng();
    let mut mixer = Mixer::new(tx);
    let mut playlist: VecDeque<Track> = VecDeque::new();
    let mut last_end: HashMap<String, f64> = HashMap::new();
    let mut failures = 0usize;
    let mut last_jingle: Option<PathBuf> = None;
    let mut next_jingle = next_hour_after(unix_secs());
    let mut next_play_id: u64 = 1;
    let mut schedule_dirty = true;
    let mut forced_warned = false;

    loop {
        match drain_commands(CommandCtx {
            rx: &cmd_rx,
            dir: &dir,
            library: &mut library,
            jingles: &mut jingles,
            playlist: &mut playlist,
            rng: &mut rng,
        }) {
            None => return,
            Some(changed) => schedule_dirty |= changed,
        }

        if library.is_empty() {
            playlist.clear();
            last_end.clear();
            failures = 0;
            schedule_dirty = false;
            shared.clear_now_playing();
            publish_queue(&shared, &playlist);
        }

        let clock = mixer.next_start_secs();
        let epoch = shared.audio_epoch_secs();
        if jingle_due(clock, epoch, next_jingle) {
            let epoch = epoch.expect("jingle_due requires an epoch");
            match choose_jingle(&jingles, last_jingle.as_ref(), &mut rng) {
                Some(jingle) => {
                    let play_id = next_play_id;
                    next_play_id = next_play_id.wrapping_add(1);
                    shared.add_inflight(play_id, track_now_playing(&jingle));
                    match stream_file(&jingle, &mut mixer, play_id) {
                        Ok(()) => {
                            last_jingle = Some(jingle.path.clone());
                        }
                        Err(Feed::RadioGone) => return,
                        Err(Feed::Skip(e)) => {
                            shared.remove_inflight(play_id);
                            mixer.abort_track();
                            eprintln!(
                                "[prefetch] jingle skipped {}: {e}",
                                jingle.path.display()
                            );
                        }
                    }
                }
                None => {
                    if jingles.is_empty() {
                        eprintln!(
                            "[prefetch] jingle hour, but no jingles found in {}",
                            jingles_dir(&dir).display()
                        );
                    }
                }
            }

            let wall = (epoch + clock).floor().max(0.0) as u64;
            next_jingle = next_hour_after(wall);
            continue;
        }

        if library.is_empty() {
            match wait_for_command(CommandCtx {
                rx: &cmd_rx,
                dir: &dir,
                library: &mut library,
                jingles: &mut jingles,
                playlist: &mut playlist,
                rng: &mut rng,
            }) {
                None => return,
                Some(changed) => schedule_dirty |= changed,
            }
            continue;
        }

        if failures >= library.len() {
            eprintln!("[prefetch] no playable files right now — waiting for a library change");
            playlist.clear();
            last_end.clear();
            failures = 0;
            schedule_dirty = false;
            publish_queue(&shared, &playlist);

            match wait_for_command(CommandCtx {
                rx: &cmd_rx,
                dir: &dir,
                library: &mut library,
                jingles: &mut jingles,
                playlist: &mut playlist,
                rng: &mut rng,
            }) {
                None => return,
                Some(changed) => schedule_dirty |= changed,
            }
            continue;
        }

        if schedule_dirty || playlist.is_empty() {
            let clock = mixer.next_start_secs();
            let fresh = playlist.is_empty();
            let forced = reschedule(
                &mut playlist,
                &library,
                &mut rng,
                clock,
                &last_end,
                fresh,
            );
            schedule_dirty = false;
            note_forced(forced, &mut forced_warned);
            publish_queue(&shared, &playlist);
        }

        // The queue is ordered with projected durations. Re-check the head
        // against the mixer's actual clock; if the projection drifted, rebuild
        // the remaining order once instead of rotating individual tracks.
        let clock = mixer.next_start_secs();
        if let Some(head) = playlist.front() {
            if !cooled(head, clock, &last_end) {
                let forced = reschedule(
                    &mut playlist,
                    &library,
                    &mut rng,
                    clock,
                    &last_end,
                    false,
                );
                note_forced(forced, &mut forced_warned);
                publish_queue(&shared, &playlist);
            }
        }

        // A newly uploaded track gets one publish cycle at the head before it
        // is played, so the UI always sees it. The encoder buffer hides the
        // one-second delay.
        if playlist.front().is_some_and(|t| t.fresh) {
            publish_queue(&shared, &playlist);
            std::thread::sleep(FRESH_HOLD);
            let fresh_path = playlist.front().map(|t| t.path.clone());
            if let Some(head) = playlist.front_mut() {
                head.fresh = false;
            }
            if let Some(path) = fresh_path {
                if let Some(t) = library.iter_mut().find(|t| t.path == path) {
                    t.fresh = false;
                }
            }
            continue;
        }

        let track = match playlist.pop_front() {
            Some(t) => t,
            None => {
                match wait_for_command(CommandCtx {
                    rx: &cmd_rx,
                    dir: &dir,
                    library: &mut library,
                    jingles: &mut jingles,
                    playlist: &mut playlist,
                    rng: &mut rng,
                }) {
                    None => return,
                    Some(changed) => schedule_dirty |= changed,
                }
                continue;
            }
        };

        let play_id = next_play_id;
        next_play_id = next_play_id.wrapping_add(1);
        shared.add_inflight(play_id, track_now_playing(&track));
        publish_queue(&shared, &playlist);

        match stream_file(&track, &mut mixer, play_id) {
            Ok(()) => {
                if track.kind == TrackKind::Normal {
                    last_end.insert(track.artist, mixer.end_secs());
                }
                failures = 0;
            }
            Err(Feed::RadioGone) => return,
            Err(Feed::Skip(e)) => {
                shared.remove_inflight(play_id);
                mixer.abort_track();
                failures += 1;
                eprintln!("[prefetch] skipping {}: {e}", track.path.display());
            }
        }
    }
}

pub fn start(dir: PathBuf, hls_enabled: bool) -> io::Result<Arc<Shared>> {
    let serial = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos()
        | 1;

    let mut pw = PageWriter::new(serial);
    let mut head = pw.head().to_vec();
    head.extend_from_slice(&pw.tags());
    let head = Bytes::from(head);

    let (tx, _) = broadcast::channel(BCAST_CAP);
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let shared = Arc::new(Shared {
        head,
        dir: dir.clone(),
        hls: Hls::new(),
        hls_enabled: AtomicBool::new(hls_enabled),
        ring: Mutex::new(VecDeque::with_capacity(BURST_PAGES as usize)),
        tx,
        status: watch::channel(Status::new()).0,
        cmd: Mutex::new(cmd_tx),
        next_listener_id: AtomicU64::new(1),
        audio_epoch_ms: AtomicU64::new(0),
        inflight: Mutex::new(VecDeque::new()),
        remaining: Mutex::new(Vec::new()),
    });

    let (blk_tx, blk_rx) = mpsc::sync_channel::<Block>(RING_BLOCKS);

    let prefetch_shared = shared.clone();
    std::thread::Builder::new()
        .name("prefetch".into())
        .spawn(move || prefetch_loop(dir, blk_tx, prefetch_shared, cmd_rx))
        .map_err(|e| io_err(e.to_string()))?;

    let station_shared = shared.clone();
    std::thread::Builder::new()
        .name("station".into())
        .spawn(move || run_station(pw, blk_rx, station_shared))
        .map_err(|e| io_err(e.to_string()))?;

    // The HTTP server starts immediately, even if the library is still empty;
    // the first UI upload can then prime the stream.
    Ok(shared)
}

fn run_station(mut pw: PageWriter, blk_rx: mpsc::Receiver<Block>, shared: Arc<Shared>) {
    let mut enc = match build_encoder() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[station] fatal: {e}");
            return;
        }
    };

    let mut pkt = vec![0u8; 4000];
    let mut frames_done: u64 = 0;
    let mut seq: u64 = 0;
    let mut upcoming: VecDeque<(u64, u64)> = VecDeque::new();
    let mut primed = 0usize;
    let mut warned = false;

    while primed < BURST_PAGES as usize {
        let block = match blk_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(b) => b,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !warned {
                    eprintln!(
                        "[station] waiting for audio — upload media or drop files into the music directory"
                    );
                    warned = true;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                eprintln!("[station] prefetch ended before the 10 s cache was built");
                return;
            }
        };

        if let Some(play_id) = block.play {
            upcoming.push_back((frames_done + BURST_PAGES, play_id));
        }

        let audio = match encode_one(&mut enc, &mut pw, &mut frames_done, &mut pkt, &block.pcm) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[station] fatal: {e}");
                return;
            }
        };

        seq += 1;
        primed += 1;
        shared.push(
            seq,
            audio.page,
            Some((audio.granule, audio.payload)),
        );
        announce_due(&shared, &mut upcoming, frames_done);
    }

    eprintln!(
        "[station] cache primed: {} pages = exactly {} s of audio; clock running ({} kbps VBR, f32/CELT)",
        BURST_PAGES, BURST_SECONDS, BITRATE_KBPS
    );

    let pace_start = std::time::Instant::now();
    // Audio-timeline zero is the first block; from here on, playout wall time
    // is `epoch + clock`, which lets the prefetch scheduler place jingles.
    shared.set_audio_epoch(unix_millis());

    loop {
        let block = match blk_rx.recv() {
            Ok(b) => b,
            Err(_) => {
                eprintln!("[station] prefetch ended (library exhausted or unreadable)");
                return;
            }
        };

        if let Some(play_id) = block.play {
            upcoming.push_back((frames_done + BURST_PAGES, play_id));
        }

        let audio = match encode_one(&mut enc, &mut pw, &mut frames_done, &mut pkt, &block.pcm) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[station] fatal: {e}");
                return;
            }
        };

        seq += 1;
        shared.push(
            seq,
            audio.page,
            Some((audio.granule, audio.payload)),
        );
        announce_due(&shared, &mut upcoming, frames_done);

        let deadline = pace_start + Duration::from_nanos((frames_done - BURST_PAGES) * 20_000_000);
        let now = std::time::Instant::now();
        if deadline > now {
            std::thread::sleep(deadline - now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    const TEST_FRAMES: usize = FRAME_SAMPLES * 20;

    #[derive(Clone, Copy)]
    struct WavSpec {
        bits: u16,
        is_float: bool,
        signal_bits: u16,
    }

    fn temp_dir(label: &str) -> PathBuf {
        let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("radio-audio-{}-{label}-{id}", std::process::id()));
        fs::create_dir_all(&dir).expect("create the test directory");
        dir
    }

    fn test_samples(frames: usize, signal_bits: u16) -> Vec<i32> {
        let half = 1i32 << (signal_bits - 1) as u32;
        let floor = half / 2;
        let span = half - floor;
        let mut state: u32 = 0x9e37_79b9;
        let mut out = Vec::with_capacity(frames * 2);
        for _ in 0..frames * 2 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let magnitude = floor + ((state >> 1) as i32 % span);
            out.push(if state & 1 == 0 {
                magnitude
            } else {
                -magnitude
            });
        }
        out
    }

    fn expected_sample(value: i32, signal_bits: u16) -> f32 {
        value as f32 / (1u32 << (signal_bits - 1)) as f32
    }

    fn source_data(spec: WavSpec, samples: &[i32]) -> Vec<u8> {
        let mut out = Vec::with_capacity(samples.len() * (spec.bits as usize / 8));
        for &sample in samples {
            match (spec.is_float, spec.bits) {
                (true, 32) => {
                    let value = expected_sample(sample, spec.signal_bits);
                    out.extend_from_slice(&value.to_bits().to_le_bytes());
                }
                (false, 8) => out.push((sample + 128) as u8),
                (false, 16) => out.extend_from_slice(&(sample as i16).to_le_bytes()),
                (false, 24) => out.extend_from_slice(&sample.to_le_bytes()[..3]),
                _ => unreachable!("unsupported test wav format"),
            }
        }
        out
    }

    fn captured_data(spec: WavSpec, pcm: &[f32]) -> Vec<u8> {
        let scale = (1u32 << (spec.signal_bits - 1)) as f32;
        let mut out = Vec::with_capacity(pcm.len() * (spec.bits as usize / 8));
        for &sample in pcm {
            match (spec.is_float, spec.bits) {
                (true, 32) => out.extend_from_slice(&sample.to_bits().to_le_bytes()),
                (false, 8) => {
                    let value = (sample * scale).round() as i32 + 128;
                    out.push(value.clamp(0, 255) as u8);
                }
                (false, 16) => {
                    let value = (sample * scale).round() as i32;
                    let value = value.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                    out.extend_from_slice(&value.to_le_bytes());
                }
                (false, 24) => {
                    let value = (sample * scale).round() as i32;
                    let value = value.clamp(-8_388_608, 8_388_607);
                    out.extend_from_slice(&value.to_le_bytes()[..3]);
                }
                _ => unreachable!("unsupported test wav format"),
            }
        }
        out
    }

    fn wav_file(spec: WavSpec, data: &[u8]) -> Vec<u8> {
        let channels: u16 = 2;
        let block_align = channels * spec.bits / 8;
        let byte_rate = SAMPLE_RATE * block_align as u32;
        let tag: u16 = if spec.is_float { 3 } else { 1 };
        let extensible = spec.bits > 16 && !spec.is_float;
        let fmt_len = if extensible { 40 } else { 16 };
        let mut out = Vec::with_capacity(12 + 8 + fmt_len + 8 + data.len());
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&((4 + 8 + fmt_len + 8 + data.len()) as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&(fmt_len as u32).to_le_bytes());
        if extensible {
            out.extend_from_slice(&0xfffeu16.to_le_bytes());
            out.extend_from_slice(&channels.to_le_bytes());
            out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
            out.extend_from_slice(&byte_rate.to_le_bytes());
            out.extend_from_slice(&block_align.to_le_bytes());
            out.extend_from_slice(&spec.bits.to_le_bytes());
            out.extend_from_slice(&22u16.to_le_bytes());
            out.extend_from_slice(&spec.bits.to_le_bytes());
            out.extend_from_slice(&0x3u32.to_le_bytes());
            out.extend_from_slice(&[
                0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00,
                0x38, 0x9b, 0x71,
            ]);
        } else {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&channels.to_le_bytes());
            out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
            out.extend_from_slice(&byte_rate.to_le_bytes());
            out.extend_from_slice(&block_align.to_le_bytes());
            out.extend_from_slice(&spec.bits.to_le_bytes());
        }
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        out
    }

    fn write_source(path: &Path, spec: WavSpec, samples: &[i32]) {
        fs::write(path, wav_file(spec, &source_data(spec, samples))).expect("write the source wav");
    }

    // The mixer receiver stands in for the Opus encoder, so the captured
    // samples are exactly what the station would hand to libopus.
    fn capture(path: &Path) -> Vec<f32> {
        let track = build_track(path.to_path_buf());
        let trim = analyze(path).expect("analyze the source");
        assert_eq!(trim.start, 0, "unexpected leading trim for {path:?}");
        assert_eq!(
            trim.end, TEST_FRAMES,
            "unexpected trailing trim for {path:?}"
        );
        assert_eq!(trim.ring, 0, "unexpected overlap for {path:?}");

        let (tx, rx) = mpsc::sync_channel(TEST_FRAMES / FRAME_SAMPLES + 8);
        let mut mixer = Mixer::new(tx);
        match stream_file(&track, &mut mixer, 0) {
            Ok(()) => {}
            Err(Feed::Skip(e)) => panic!("stream failed for {path:?}: {e}"),
            Err(Feed::RadioGone) => panic!("mixer receiver disappeared for {path:?}"),
        }
        drop(mixer);

        let mut pcm = Vec::with_capacity(TEST_FRAMES * 2);
        for block in rx.iter() {
            pcm.extend_from_slice(&block.pcm);
        }
        pcm
    }

    fn assert_identity(path: &Path, samples: &[i32], signal_bits: u16) {
        let pcm = capture(path);
        assert_eq!(
            pcm.len(),
            samples.len(),
            "sample count changed for {path:?}"
        );
        let expected: Vec<f32> = samples
            .iter()
            .map(|&value| expected_sample(value, signal_bits))
            .collect();
        for (index, (got, want)) in pcm.iter().zip(&expected).enumerate() {
            assert_eq!(
                got.to_bits(),
                want.to_bits(),
                "sample {index} changed for {path:?}"
            );
        }
    }

    #[test]
    fn one_pass_releases_the_long_track_tail_exactly() {
        const LONG_FRAMES: usize = SAMPLE_RATE as usize * 20;
        let dir = temp_dir("long-tail");
        let path = dir.join("long.wav");
        let spec = WavSpec {
            bits: 16,
            is_float: false,
            signal_bits: 16,
        };
        let samples = test_samples(LONG_FRAMES, 16);
        write_source(&path, spec, &samples);

        let track = build_track(path.clone());
        let trim = analyze(&path).expect("reference trim");
        let (tx, rx) = mpsc::sync_channel(LONG_FRAMES / FRAME_SAMPLES + 8);
        let mut mixer = Mixer::new(tx);
        stream_file(&track, &mut mixer, 0)
            .unwrap_or_else(|_| panic!("stream the long track"));
        assert_eq!(
            mixer.hold,
            scaled_frames(SAMPLE_RATE as usize, trim.ring) * 2,
            "one-pass ring differs from analysis"
        );
        assert!(
            (mixer.end_secs() - trim.end as f64 / SAMPLE_RATE as f64).abs() < 1e-9,
            "one-pass end differs from analysis"
        );
        mixer.abort_track();
        mixer
            .commit()
            .unwrap_or_else(|_| panic!("release the held tail"));
        drop(mixer);

        let mut pcm = Vec::with_capacity(LONG_FRAMES * 2);
        for block in rx.iter() {
            pcm.extend_from_slice(&block.pcm);
        }
        assert_eq!(pcm.len(), samples.len(), "long track length changed");
        for (index, (got, want)) in pcm.iter().zip(&samples).enumerate() {
            assert_eq!(
                got.to_bits(),
                expected_sample(*want, 16).to_bits(),
                "sample {index} changed"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wav_decoding_is_bit_identical_to_the_generated_source() {
        let dir = temp_dir("wav");
        let specs = [
            WavSpec {
                bits: 8,
                is_float: false,
                signal_bits: 8,
            },
            WavSpec {
                bits: 16,
                is_float: false,
                signal_bits: 16,
            },
            WavSpec {
                bits: 24,
                is_float: false,
                signal_bits: 24,
            },
            WavSpec {
                bits: 32,
                is_float: true,
                signal_bits: 16,
            },
        ];

        for spec in specs {
            let samples = test_samples(TEST_FRAMES, spec.signal_bits);
            let path = dir.join(format!("source-{}-{}.wav", spec.bits, spec.is_float));
            write_source(&path, spec, &samples);

            let pcm = capture(&path);
            assert_eq!(
                pcm.len(),
                samples.len(),
                "sample count changed for {path:?}"
            );
            let expected: Vec<f32> = samples
                .iter()
                .map(|&value| expected_sample(value, spec.signal_bits))
                .collect();
            for (index, (got, want)) in pcm.iter().zip(&expected).enumerate() {
                assert_eq!(
                    got.to_bits(),
                    want.to_bits(),
                    "sample {index} changed for {path:?}"
                );
            }

            let source = fs::read(&path).expect("read the source wav");
            let processed = wav_file(spec, &captured_data(spec, &pcm));
            assert_eq!(source, processed, "processed wav differs for {path:?}");
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn flac_decoding_is_bit_identical_to_the_generated_source() {
        let dir = temp_dir("flac");
        for signal_bits in [16u16, 24u16] {
            let samples = test_samples(TEST_FRAMES, signal_bits);
            let spec = WavSpec {
                bits: signal_bits,
                is_float: false,
                signal_bits,
            };
            let wav_path = dir.join(format!("source-{signal_bits}.wav"));
            write_source(&wav_path, spec, &samples);

            let flac_path = dir.join(format!("source-{signal_bits}.flac"));
            let status = Command::new("flac")
                .arg("--silent")
                .arg("--force")
                .arg("-o")
                .arg(&flac_path)
                .arg(&wav_path)
                .status()
                .unwrap_or_else(|e| panic!("could not run the flac encoder: {e}"));
            assert!(status.success(), "flac failed for {signal_bits}-bit source");

            assert_identity(&flac_path, &samples, signal_bits);
        }

        let _ = fs::remove_dir_all(&dir);
    }

    fn sched_track(artist: &str, duration: f64) -> Track {
        Track {
            path: PathBuf::from(format!("{artist}-{duration}.wav")),
            artist: artist.to_string(),
            kind: TrackKind::Normal,
            duration,
            fresh: false,
        }
    }

    fn starts(order: &VecDeque<Track>, clock: f64) -> Vec<(String, f64, f64)> {
        let mut t = clock;
        order
            .iter()
            .map(|track| {
                let start = t;
                t += track.duration;
                (track.artist.clone(), start, t)
            })
            .collect()
    }

    #[test]
    fn scheduler_preserves_preference_when_all_artists_are_ready() {
        let tracks = vec![
            sched_track("a", 100.0),
            sched_track("b", 100.0),
            sched_track("c", 100.0),
            sched_track("d", 100.0),
        ];
        let (order, forced) = schedule_tracks(tracks.clone(), 0.0, &HashMap::new());
        assert_eq!(forced, 0);
        let got: Vec<_> = order.iter().map(|t| t.path.clone()).collect();
        let want: Vec<_> = tracks.iter().map(|t| t.path.clone()).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn scheduler_keeps_same_artist_one_hour_apart() {
        let tracks = vec![
            sched_track("a", 100.0),
            sched_track("b", 1_000.0),
            sched_track("a", 100.0),
            sched_track("c", 1_000.0),
            sched_track("d", 1_000.0),
            sched_track("e", 1_000.0),
        ];
        let (order, forced) = schedule_tracks(tracks, 0.0, &HashMap::new());
        assert_eq!(forced, 0);
        let seq = starts(&order, 0.0);
        let mut last_end: HashMap<String, f64> = HashMap::new();
        for (artist, start, end) in &seq {
            if let Some(prev_end) = last_end.get(artist) {
                assert!(
                    start - prev_end >= 3_600.0,
                    "{artist} started at {start} but previous end was {prev_end}: {seq:?}"
                );
            }
            last_end.insert(artist.clone(), *end);
        }
        // The second "a" cannot play until 3700, so the later artists fill
        // the cooldown and it lands after them.
        assert_eq!(seq.last().unwrap().0, "a");
    }

    #[test]
    fn scheduler_forces_only_when_the_library_cannot_fill_the_hour() {
        let tracks = vec![
            sched_track("a", 100.0),
            sched_track("a", 100.0),
            sched_track("a", 100.0),
        ];
        let (order, forced) = schedule_tracks(tracks.clone(), 0.0, &HashMap::new());
        assert_eq!(forced, 2);
        let got: Vec<_> = order.iter().map(|t| t.path.clone()).collect();
        let want: Vec<_> = tracks.iter().map(|t| t.path.clone()).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn scheduler_respects_an_actual_previous_end() {
        let mut last_end = HashMap::new();
        last_end.insert("a".to_string(), 5_000.0);
        let mut tracks = vec![sched_track("a", 100.0)];
        for i in 0..10 {
            tracks.push(sched_track(&format!("filler-{i}"), 1_000.0));
        }
        let (order, forced) = schedule_tracks(tracks, 0.0, &last_end);
        assert_eq!(forced, 0);
        let seq = starts(&order, 0.0);
        let (_, start, _) = seq.iter().find(|(artist, _, _)| artist == "a").unwrap();
        assert!(*start >= 8_600.0, "a started at {start}: {seq:?}");
    }

    #[test]
    fn jingle_deadline_follows_the_playout_clock() {
        assert!(!jingle_due(3_599.9, Some(0.0), 3_600));
        assert!(jingle_due(3_600.0, Some(0.0), 3_600));
        assert!(!jingle_due(3_600.0, None, 3_600));

        // A song starting just before the hour delays the jingle until after
        // it; the next boundary crosses the deadline.
        let song_start = 3_599.0;
        assert!(!jingle_due(song_start, Some(0.0), 3_600));
        assert!(jingle_due(song_start + 300.0, Some(0.0), 3_600));

        // A non-zero epoch shifts the deadline on the audio timeline.
        assert!(!jingle_due(3_599.0, Some(1_000.0), 4_600));
        assert!(jingle_due(3_600.0, Some(1_000.0), 4_600));
    }

    #[test]
    fn probe_duration_reads_wav_and_flac_headers() {
        let dir = temp_dir("probe-duration");
        let frames = SAMPLE_RATE as usize * 2;
        let wav = dir.join("tone.wav");
        fs::write(&wav, wav_tone(SAMPLE_RATE, frames, 440.0, 0.5)).expect("write the tone");
        let wav_duration = probe_duration(&wav).expect("wav header duration");
        assert!(
            (wav_duration - 2.0).abs() < 1e-6,
            "wav duration {wav_duration}"
        );
        assert!((build_track(wav).duration - 2.0).abs() < 1e-6);

        let flac = dir.join("tone.flac");
        let status = Command::new("flac")
            .arg("--silent")
            .arg("--force")
            .arg("-o")
            .arg(&flac)
            .arg(&dir.join("tone.wav"))
            .status()
            .expect("run the flac encoder");
        assert!(status.success(), "flac failed");
        let flac_duration = probe_duration(&flac).expect("flac header duration");
        assert!(
            (flac_duration - 2.0).abs() < 1e-6,
            "flac duration {flac_duration}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Compares the one-off scheduling cost with the old per-selection
    /// rotation, for a library with many artists and for a dense one where
    /// rotations actually happen.
    #[test]
    #[ignore]
    fn bench_schedule() {
        use std::time::Instant;

        fn run(label: &str, tracks: Vec<Track>) {
            let n = tracks.len();
            let empty = HashMap::new();
            let _ = schedule_tracks(tracks.clone(), 0.0, &empty);
            let start = Instant::now();
            let mut forced = 0usize;
            for _ in 0..20 {
                let (_, f) = schedule_tracks(tracks.clone(), 0.0, &empty);
                forced = f;
            }
            let schedule_ms = start.elapsed().as_secs_f64() * 1000.0 / 20.0;

            // Old behaviour: scan and rotate until an eligible head appears.
            let start = Instant::now();
            let mut queue: VecDeque<Track> = tracks.into();
            let mut last_end: HashMap<String, f64> = HashMap::new();
            let mut clock = 0.0f64;
            let mut scanned_total = 0usize;
            for _ in 0..n {
                let mut scanned = 0usize;
                let mut pick = None;
                while scanned < queue.len() {
                    let track = queue.pop_front().expect("non-empty");
                    scanned += 1;
                    if cooled(&track, clock, &last_end) {
                        pick = Some(track);
                        break;
                    }
                    queue.push_back(track);
                }
                let track = match pick {
                    Some(track) => track,
                    None => {
                        last_end.clear();
                        queue.pop_front().expect("non-empty")
                    }
                };
                scanned_total += scanned;
                clock += track.duration;
                last_end.insert(track.artist.clone(), clock);
            }
            let rotation_ms = start.elapsed().as_secs_f64() * 1000.0;

            println!(
                "{label}: {n} tracks, schedule {:.3} ms (forced {forced}), old rotation {:.3} ms ({:.1} head checks/track)",
                schedule_ms,
                rotation_ms,
                scanned_total as f64 / n as f64
            );
        }

        let sparse: Vec<Track> = (0..5_000)
            .map(|i| {
                sched_track(
                    &format!("artist-{}", i % 200),
                    150.0 + (i % 11) as f64 * 5.0,
                )
            })
            .collect();
        run("sparse", sparse);

        let dense: Vec<Track> = (0..3_000)
            .map(|i| sched_track(&format!("artist-{}", i % 20), 150.0))
            .collect();
        run("dense", dense);
    }

    /// Per-file cost of the new header-only duration probe.
    #[test]
    #[ignore]
    fn bench_probe() {
        use std::time::Instant;
        let dir = temp_dir("bench-probe");
        let wav = dir.join("tone.wav");
        let frames = SAMPLE_RATE as usize * 2;
        fs::write(&wav, wav_tone(SAMPLE_RATE, frames, 440.0, 0.5)).expect("write the tone");
        let flac = dir.join("tone.flac");
        let status = Command::new("flac")
            .arg("--silent")
            .arg("--force")
            .arg("-o")
            .arg(&flac)
            .arg(&wav)
            .status()
            .expect("run the flac encoder");
        assert!(status.success(), "flac failed");

        for path in [&wav, &flac] {
            probe_duration(path).expect("warm probe");
            let start = Instant::now();
            for _ in 0..1_000 {
                probe_duration(path).expect("probe");
            }
            let us = start.elapsed().as_secs_f64() * 1e6 / 1_000.0;
            println!(
                "{}: {us:.2} µs per header probe",
                path.file_name().unwrap().to_string_lossy()
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mixer_crossfade_mixes_tail_and_emits_whole_blocks() {
        let (tx, rx) = mpsc::sync_channel(64);
        let mut mixer = Mixer::new(tx);
        let first = vec![0.25f32; FRAME_FLOATS * 4];
        mixer.begin_track(FRAME_SAMPLES, FRAME_SAMPLES * 4, 0);
        assert!(mixer.add(&first).is_ok());

        assert!((mixer.end_secs() - 0.08).abs() < 1e-9);
        assert!((mixer.next_start_secs() - 0.06).abs() < 1e-9);

        let second = vec![0.5f32; FRAME_FLOATS * 4];
        mixer.begin_track(0, FRAME_SAMPLES * 4, 0);
        assert!(mixer.add(&second).is_ok());
        drop(mixer);

        let blocks: Vec<_> = rx.iter().collect();
        assert_eq!(blocks.len(), 7);
        assert!(blocks.iter().all(|block| block.pcm.len() == FRAME_FLOATS));

        let pcm: Vec<f32> = blocks
            .iter()
            .flat_map(|block| block.pcm.iter().copied())
            .collect();
        assert!(pcm[..FRAME_FLOATS].iter().all(|&s| s == 0.25));
        assert!(
            pcm[3 * FRAME_FLOATS..4 * FRAME_FLOATS]
                .iter()
                .all(|&s| s == 0.75)
        );
        assert!(pcm[4 * FRAME_FLOATS..].iter().all(|&s| s == 0.5));
    }

    /// Measures the mixer's buffering cost with an eight-second crossfade
    /// hold and 4096-frame feeding chunks, i.e. the shape of the live path.
    #[test]
    #[ignore]
    fn bench_mixer() {
        use std::time::Instant;
        const SECONDS: usize = 60;
        const CHUNK_FRAMES: usize = 4096;
        let frames = SECONDS * SAMPLE_RATE as usize;
        let hold = 8 * SAMPLE_RATE as usize;

        let (tx, rx) = mpsc::sync_channel(4);
        let drain = std::thread::spawn(move || rx.iter().map(|b: Block| b.pcm.len()).sum::<usize>());
        let mut mixer = Mixer::new(tx);
        mixer.begin_track(hold, frames, 0);

        let chunk = vec![0.125f32; CHUNK_FRAMES * 2];
        let start = Instant::now();
        for _ in 0..frames / CHUNK_FRAMES {
            assert!(mixer.add(&chunk).is_ok(), "mixer receiver disappeared");
        }
        let secs = start.elapsed().as_secs_f64();
        drop(mixer);
        let emitted = drain.join().expect("drain thread");

        println!(
            "{:.3} ms per second of audio ({:.1}x realtime), {} samples emitted",
            secs * 1000.0 / SECONDS as f64,
            SECONDS as f64 / secs,
            emitted
        );
    }

    /// Measures the analysis pass on WAV and its FLAC equivalent.
    #[test]
    #[ignore]
    fn bench_analyze() {
        use std::time::Instant;
        const SECONDS: usize = 60;
        let dir = temp_dir("bench-analyze");
        for rate in [48_000u32, 44_100] {
            let frames = rate as usize * SECONDS;
            let wav = dir.join(format!("tone-{rate}.wav"));
            fs::write(&wav, wav_tone(rate, frames, 440.0, 0.5)).expect("write the tone");
            let flac = dir.join(format!("tone-{rate}.flac"));
            let status = Command::new("flac")
                .arg("--silent")
                .arg("--force")
                .arg("-o")
                .arg(&flac)
                .arg(&wav)
                .status()
                .expect("run the flac encoder");
            assert!(status.success(), "flac failed for {rate} Hz");

            for path in [&wav, &flac] {
                analyze(path).expect("warm analysis");
                let start = Instant::now();
                for _ in 0..3 {
                    analyze(path).expect("analysis");
                }
                let secs = start.elapsed().as_secs_f64() / 3.0;
                println!(
                    "{}: {:.3} ms per second of audio ({:.1}x realtime)",
                    path.file_name().unwrap().to_string_lossy(),
                    secs * 1000.0 / SECONDS as f64,
                    SECONDS as f64 / secs
                );
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// Measures the full per-track prefetch cost: analysis, decode, optional
    /// resampling, and mixing into encoder blocks.
    #[test]
    #[ignore]
    fn bench_stream() {
        use std::time::Instant;
        const SECONDS: usize = 60;
        let dir = temp_dir("bench-stream");
        for rate in [48_000u32, 44_100] {
            let frames = rate as usize * SECONDS;
            let path = dir.join(format!("tone-{rate}.wav"));
            fs::write(&path, wav_tone(rate, frames, 440.0, 0.5)).expect("write the tone");
            let track = build_track(path.clone());
            let mut best = f64::MAX;
            for _ in 0..3 {
                let (tx, rx) = mpsc::sync_channel(4);
                let drain =
                    std::thread::spawn(move || rx.iter().map(|b: Block| b.pcm.len()).sum::<usize>());
                let mut mixer = Mixer::new(tx);
                let start = Instant::now();
                match stream_file(&track, &mut mixer, 0) {
                    Ok(()) => {}
                    Err(Feed::Skip(e)) => panic!("stream failed for {rate} Hz: {e}"),
                    Err(Feed::RadioGone) => panic!("mixer receiver disappeared for {rate} Hz"),
                }
                let secs = start.elapsed().as_secs_f64();
                drop(mixer);
                let emitted = drain.join().expect("drain thread");
                assert!(emitted > 0);
                best = best.min(secs);
            }
            println!(
                "{rate} Hz -> encoder: {:.3} ms per second of audio ({:.1}x realtime)",
                best * 1000.0 / SECONDS as f64,
                SECONDS as f64 / best
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// 16-bit stereo tone, at an arbitrary sample rate.
    fn wav_tone(rate: u32, frames: usize, hz: f64, amp: f64) -> Vec<u8> {
        let channels: u16 = 2;
        let bits: u16 = 16;
        let block_align = channels * bits / 8;
        let byte_rate = rate * block_align as u32;
        let mut data = Vec::with_capacity(frames * block_align as usize);
        for i in 0..frames {
            let v = (std::f64::consts::TAU * hz * i as f64 / rate as f64).sin() * amp;
            let sample = (v * 32767.0).round() as i16;
            for _ in 0..channels {
                data.extend_from_slice(&sample.to_le_bytes());
            }
        }

        let mut out = Vec::with_capacity(44 + data.len());
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&((36 + data.len()) as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&byte_rate.to_le_bytes());
        out.extend_from_slice(&block_align.to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    /// Magnitude of `f` Hz in the left channel, as a sine amplitude.
    fn amplitude_at(pcm: &[f32], rate: f64, f: f64) -> f64 {
        let frames = pcm.len() / 2;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, pair) in pcm.chunks_exact(2).enumerate() {
            let ph = std::f64::consts::TAU * f * i as f64 / rate;
            re += pair[0] as f64 * ph.cos();
            im += pair[0] as f64 * ph.sin();
        }
        2.0 * (re * re + im * im).sqrt() / frames as f64
    }

    /// A non-48 kHz file must reach the mixer at 48 kHz, at the same pitch, and
    /// with the exact rational length. This covers the `station.rs` wiring, not
    /// just `resample.rs`: a swapped `L`/`M` would fail every assertion here.
    #[test]
    fn resampled_wav_keeps_pitch_and_exact_length() {
        let cases = [
            (44_100u32, 160usize, 147usize),
            (96_000, 1, 2),
            (192_000, 1, 4),
        ];

        for (rate, l, m) in cases {
            let dir = temp_dir(&format!("resample-{rate}"));
            let frames = rate as usize / 2;
            let path = dir.join(format!("tone-{rate}.wav"));
            fs::write(&path, wav_tone(rate, frames, 1_000.0, 0.5)).expect("write the tone");

            let track = build_track(path.clone());
            let trim = analyze(&path).expect("analyze the tone");
            let fed = trim.end - trim.start;
            let expect = (fed * l + m - 1) / m;

            let (tx, rx) = mpsc::sync_channel(expect / FRAME_SAMPLES + 8);
            let mut mixer = Mixer::new(tx);
            match stream_file(&track, &mut mixer, 0) {
                Ok(()) => {}
                Err(Feed::Skip(e)) => panic!("stream failed for {rate} Hz: {e}"),
                Err(Feed::RadioGone) => panic!("mixer receiver disappeared for {rate} Hz"),
            }
            drop(mixer);

            let mut pcm = Vec::new();
            for block in rx.iter() {
                assert_eq!(block.pcm.len(), FRAME_FLOATS);
                pcm.extend_from_slice(&block.pcm);
            }

            let got = pcm.len() / 2;
            assert_eq!(got % FRAME_SAMPLES, 0, "whole blocks for {rate} Hz");
            assert!(
                got.abs_diff(expect) < FRAME_SAMPLES,
                "{rate} Hz: {got} frames, expected about {expect}"
            );
            assert!(
                pcm.iter().all(|s| s.is_finite()),
                "{rate} Hz produced non-finite output"
            );

            let rate_out = SAMPLE_RATE as f64;
            let tone = amplitude_at(&pcm, rate_out, 1_000.0);
            let other = amplitude_at(&pcm, rate_out, 1_500.0);
            assert!((tone - 0.5).abs() < 0.02, "{rate} Hz tone amplitude {tone}");
            assert!(other < 0.01, "{rate} Hz leaked {other} at 1.5 kHz");

            let _ = fs::remove_dir_all(&dir);
        }
    }

    fn test_shared() -> Arc<Shared> {
        let (tx, _) = broadcast::channel(4);
        let (cmd, _cmd_rx) = mpsc::channel();
        Arc::new(Shared {
            head: Bytes::new(),
            dir: PathBuf::from("."),
            hls: Hls::new(),
            hls_enabled: AtomicBool::new(true),
            ring: Mutex::new(VecDeque::new()),
            tx,
            status: watch::channel(Status::new()).0,
            cmd: Mutex::new(cmd),
            next_listener_id: AtomicU64::new(1),
            audio_epoch_ms: AtomicU64::new(0),
            inflight: Mutex::new(VecDeque::new()),
            remaining: Mutex::new(Vec::new()),
        })
    }

    fn status_of(shared: &Arc<Shared>) -> Status {
        shared.watch_status().borrow().clone()
    }

    #[test]
    fn inflight_tracks_remain_in_the_published_queue() {
        let shared = test_shared();
        shared.add_inflight(
            7,
            Arc::new(NowPlaying {
                artist: "Artist".into(),
                title: "Track".into(),
                file: "track.wav".into(),
                format: "WAV".into(),
            }),
        );
        assert_eq!(
            shared
                .queue_snapshot()
                .iter()
                .map(|np| np.file.as_str())
                .collect::<Vec<_>>(),
            vec!["track.wav"]
        );
        assert!(shared.take_inflight(7).is_some());
        assert!(shared.queue_snapshot().is_empty());
    }

    /// The admin frame must carry the live connection list; the public frame
    /// must not leak IPs or user agents to unauthenticated players. Dropping
    /// the guard must remove the entry immediately.
    #[test]
    fn listener_registry_is_live_and_private() {
        let shared = test_shared();
        let guard = shared.register_listener("203.0.113.7".into(), "TestAgent/1.0".into());

        let status = status_of(&shared);
        let admin = String::from_utf8(status.frame.to_vec()).expect("admin frame is UTF-8");
        assert!(admin.contains("\"listener_list\""), "{admin}");
        assert!(admin.contains("203.0.113.7"), "{admin}");
        assert!(admin.contains("TestAgent/1.0"), "{admin}");
        assert!(admin.contains("\"listeners\":1"), "{admin}");

        let public =
            String::from_utf8(status.public_frame.to_vec()).expect("public frame is UTF-8");
        assert!(
            !public.contains("203.0.113.7"),
            "the public frame must not leak client IPs: {public}"
        );
        assert!(
            !public.contains("TestAgent/1.0"),
            "the public frame must not leak user agents: {public}"
        );
        assert!(public.contains("\"listeners\":1"), "{public}");

        assert!(shared.disconnect_listener(1));
        assert!(
            *guard.disconnect_signal().borrow(),
            "the stream must observe the disconnect request"
        );
        assert!(
            !shared.disconnect_listener(99),
            "unknown ids must not match"
        );

        drop(guard);
        let status = status_of(&shared);
        let admin = String::from_utf8(status.frame.to_vec()).expect("admin frame is UTF-8");
        assert!(admin.contains("\"listener_list\":[]"), "{admin}");
        assert!(admin.contains("\"listeners\":0"), "{admin}");
    }
}
