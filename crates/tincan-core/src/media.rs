//! Media packet formats shared by tincan-audio, tincan-screen and tincan-net.
//! The host forwards these bytes verbatim. All integers big-endian.

use bytes::Bytes;

/// Track kinds (low 2 bits of a track id): voice.
pub const KIND_VOICE: u16 = 0;
/// Track kinds (low 2 bits of a track id): screen share.
pub const KIND_SCREEN: u16 = 1;
/// Track kinds (low 2 bits of a track id): soundboard audio. A member's clips go out on their
/// own Opus stream (`track_id(short_id, KIND_SOUND)`) so receivers can set its volume apart
/// from the voice. Same datagram format as voice.
pub const KIND_SOUND: u16 = 2;
/// Track kinds (low 2 bits of a track id): the audio of a screen share. It rides the same
/// audio datagrams as voice (`AUDIO_MAGIC`), Opus 48 kHz stereo, and is forwarded only to
/// those watching the matching `KIND_SCREEN` track. Builds from before this kind drop it:
/// their receivers and hosts accept only voice and soundboard datagrams.
pub const KIND_SHARE_AUDIO: u16 = 3;
/// Largest short id that fits in a track id (14 bits).
pub const MAX_SHORT_ID: u16 = 0x3FFF;

/// `short_id * 4 + kind`. Short ids above `MAX_SHORT_ID` wrap (the host never assigns them).
pub fn track_id(short_id: u16, kind: u16) -> u16 { short_id.wrapping_mul(4) | (kind & 3) }
/// The short id part of a track id.
pub fn track_short_id(track: u16) -> u16 { track >> 2 }
/// The kind part of a track id.
pub fn track_kind(track: u16) -> u16 { track & 3 }
/// True for the call audio kinds a member always sends to the whole room (voice and
/// soundboard). Share audio (`KIND_SHARE_AUDIO`) also rides audio datagrams but goes only
/// to watchers, so it is checked on its own.
pub fn is_audio_kind(kind: u16) -> bool { kind == KIND_VOICE || kind == KIND_SOUND }
/// The screen track whose watchers get the share-audio track `track` (same short id).
/// None if `track` is not a share-audio track.
pub fn screen_of_share_audio(track: u16) -> Option<u16> {
    (track_kind(track) == KIND_SHARE_AUDIO).then(|| track_id(track_short_id(track), KIND_SCREEN))
}
/// The share-audio track that goes with the screen track `track`. None if `track` is not a
/// screen track.
pub fn share_audio_of_screen(track: u16) -> Option<u16> {
    (track_kind(track) == KIND_SCREEN).then(|| track_id(track_short_id(track), KIND_SHARE_AUDIO))
}

// ---------------------------------------------------------------- audio datagrams

/// First byte of every audio datagram (audio v1).
pub const AUDIO_MAGIC: u8 = 0xA1;
/// Bytes before the Opus payload.
pub const AUDIO_HEADER_LEN: usize = 12;

/// Flag: the sender is speaking (post-APM energy, 250 ms hangover).
pub const AF_SPEAKING: u8 = 1 << 0;
/// Flag: first packet of a talkspurt.
pub const AF_TALKSPURT: u8 = 1 << 1;
/// Flag: the Opus packet carries in-band FEC.
pub const AF_FEC: u8 = 1 << 2;
/// Flag: the packet carries soundboard audio. Builds since the soundboard track send it on
/// `KIND_SOUND` packets only; older builds set it on voice packets with a clip mixed in.
pub const AF_SOUNDBOARD: u8 = 1 << 3;

/// Layout: `[0] magic, [1] flags, [2..4] track, [4..6] seq, [6..10] ts (48 kHz), [10] level, [11] epoch`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct AudioHeader {
    /// `AF_*` bits.
    pub flags: u8,
    /// Track id (`track_id(short_id, KIND_VOICE)`).
    pub track: u16,
    /// +1 per packet (DTX does not consume seq), wraps.
    pub seq: u16,
    /// 48 kHz sample clock, random origin, wraps.
    pub ts: u32,
    /// 0..127 = -dBov (0 loudest, 127 silence)
    pub level: u8,
    /// Low byte of the host epoch the sender saw.
    pub epoch: u8,
}

impl AudioHeader {
    /// Write the 12 header bytes.
    pub fn write(&self, out: &mut [u8; AUDIO_HEADER_LEN]) {
        out[0] = AUDIO_MAGIC;
        out[1] = self.flags;
        out[2..4].copy_from_slice(&self.track.to_be_bytes());
        out[4..6].copy_from_slice(&self.seq.to_be_bytes());
        out[6..10].copy_from_slice(&self.ts.to_be_bytes());
        out[10] = self.level;
        out[11] = self.epoch;
    }
    /// Returns the header and the Opus payload. None if too short or wrong magic.
    pub fn parse(pkt: &[u8]) -> Option<(AudioHeader, &[u8])> {
        if pkt.len() < AUDIO_HEADER_LEN || pkt[0] != AUDIO_MAGIC {
            return None;
        }
        let h = AudioHeader {
            flags: pkt[1],
            track: u16::from_be_bytes([pkt[2], pkt[3]]),
            seq: u16::from_be_bytes([pkt[4], pkt[5]]),
            ts: u32::from_be_bytes([pkt[6], pkt[7], pkt[8], pkt[9]]),
            level: pkt[10],
            epoch: pkt[11],
        };
        Some((h, &pkt[AUDIO_HEADER_LEN..]))
    }
    /// Header followed by payload, ready for `send_datagram`.
    pub fn packet(&self, opus: &[u8]) -> Bytes {
        let mut h = [0u8; AUDIO_HEADER_LEN];
        self.write(&mut h);
        let mut v = Vec::with_capacity(AUDIO_HEADER_LEN + opus.len());
        v.extend_from_slice(&h);
        v.extend_from_slice(opus);
        Bytes::from(v)
    }
    /// True if `flag` (an `AF_*` constant) is set.
    pub fn has(&self, flag: u8) -> bool { self.flags & flag != 0 }
}

/// Track id of an audio datagram without parsing the rest (for host forwarding).
/// None if too short or wrong magic.
pub fn audio_track_of(pkt: &[u8]) -> Option<u16> {
    if pkt.len() >= AUDIO_HEADER_LEN && pkt[0] == AUDIO_MAGIC { Some(u16::from_be_bytes([pkt[2], pkt[3]])) } else { None }
}

// ---------------------------------------------------------------- video streams

/// First byte of every video GOP stream.
pub const VIDEO_MAGIC: u8 = 0xB1;
/// Bytes of the GOP stream header.
pub const VIDEO_STREAM_HEADER_LEN: usize = 9;
/// Codec byte: H.264 Annex-B.
pub const CODEC_H264_ANNEXB: u8 = 1;
/// Frame flag: keyframe (IDR).
pub const VF_KEYFRAME: u8 = 1 << 0;

/// One QUIC uni stream per GOP starts with this header:
/// `[0] magic, [1..3] track, [3..7] group_seq, [7] codec, [8] flags`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoStreamHeader {
    /// Track id (`track_id(short_id, KIND_SCREEN)`).
    pub track: u16,
    /// GOP number, +1 per GOP (also the stream priority).
    pub group_seq: u32,
    /// `CODEC_*`.
    pub codec: u8,
    /// Reserved, 0.
    pub flags: u8,
}

impl VideoStreamHeader {
    /// The 9 header bytes.
    pub fn write(&self) -> [u8; VIDEO_STREAM_HEADER_LEN] {
        let mut o = [0u8; VIDEO_STREAM_HEADER_LEN];
        o[0] = VIDEO_MAGIC;
        o[1..3].copy_from_slice(&self.track.to_be_bytes());
        o[3..7].copy_from_slice(&self.group_seq.to_be_bytes());
        o[7] = self.codec;
        o[8] = self.flags;
        o
    }
    /// Parse the first 9 bytes of a stream. None if too short or wrong magic.
    pub fn parse(b: &[u8]) -> Option<VideoStreamHeader> {
        if b.len() < VIDEO_STREAM_HEADER_LEN || b[0] != VIDEO_MAGIC {
            return None;
        }
        Some(VideoStreamHeader {
            track: u16::from_be_bytes([b[1], b[2]]),
            group_seq: u32::from_be_bytes([b[3], b[4], b[5], b[6]]),
            codec: b[7],
            flags: b[8],
        })
    }
}

/// Largest value a QUIC varint can hold.
const VARINT_MAX: u64 = (1 << 62) - 1;

/// QUIC variable-length integer (RFC 9000 section 16), up to 2^62-1 (larger values saturate).
fn put_varint(out: &mut Vec<u8>, v: u64) {
    let v = v.min(VARINT_MAX);
    if v < 1 << 6 {
        out.push(v as u8);
    } else if v < 1 << 14 {
        out.extend_from_slice(&((v as u16) | 0x4000).to_be_bytes());
    } else if v < 1 << 30 {
        out.extend_from_slice(&((v as u32) | 0x8000_0000).to_be_bytes());
    } else {
        out.extend_from_slice(&(v | 0xC000_0000_0000_0000).to_be_bytes());
    }
}

/// Returns (value, bytes used), or None if `b` is too short.
fn get_varint(b: &[u8]) -> Option<(u64, usize)> {
    let first = *b.first()?;
    let len = 1usize << (first >> 6);
    if b.len() < len {
        return None;
    }
    let mut v = (first & 0x3f) as u64;
    for x in &b[1..len] {
        v = (v << 8) | *x as u64;
    }
    Some((v, len))
}

/// One encoded access unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoFrame {
    /// 90 kHz timestamp.
    pub ts: u32,
    /// Contains an IDR.
    pub keyframe: bool,
    /// Annex-B access unit.
    pub data: Bytes,
}

/// Frame on the GOP stream: QUIC-style varint length of (5 + data.len()) | ts u32 | fflags u8 | data.
pub fn encode_video_frame(f: &VideoFrame) -> Bytes {
    let mut out = Vec::with_capacity(f.data.len() + 13);
    put_varint(&mut out, 5 + f.data.len() as u64);
    out.extend_from_slice(&f.ts.to_be_bytes());
    out.push(if f.keyframe { VF_KEYFRAME } else { 0 });
    out.extend_from_slice(&f.data);
    Bytes::from(out)
}

/// Largest accepted frame on a video stream. Encoder input is at most 1920 wide, and the
/// GOP cache holds at most 2 MiB anyway, so a bigger frame is never legitimate.
pub const MAX_VIDEO_FRAME: usize = 2 * 1024 * 1024;

/// Incremental reader: push stream bytes, pop frames. Consumed bytes are dropped lazily
/// (a read offset, compacted on push), so many small frames cost linear time.
#[derive(Debug, Default)]
pub struct VideoFrameReader {
    buf: Vec<u8>,
    pos: usize,
}

impl VideoFrameReader {
    /// Empty reader.
    pub fn new() -> Self { Self::default() }
    /// Append bytes read from the stream.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.pos > 0 && self.pos >= self.buf.len() / 2 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        self.buf.extend_from_slice(bytes);
    }
    /// Bytes buffered but not yet returned as frames.
    pub fn buffered(&self) -> usize { self.buf.len() - self.pos }
    /// True while part of a frame is buffered (a frame has started but is not complete).
    pub fn mid_frame(&self) -> bool { self.buffered() > 0 }
    /// Next complete frame. Returns Err(()) on a malformed or oversized frame (reset the stream).
    #[allow(clippy::should_implement_trait, clippy::result_unit_err)]
    pub fn next(&mut self) -> Result<Option<VideoFrame>, ()> {
        let avail = &self.buf[self.pos..];
        let Some((len, used)) = get_varint(avail) else { return Ok(None) };
        let Ok(len) = usize::try_from(len) else { return Err(()) };
        if !(5..=MAX_VIDEO_FRAME).contains(&len) {
            return Err(());
        }
        if avail.len() < used + len {
            return Ok(None);
        }
        let body = &avail[used..used + len];
        let ts = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
        let keyframe = body[4] & VF_KEYFRAME != 0;
        let data = Bytes::copy_from_slice(&body[5..]);
        self.pos += used + len;
        if self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        }
        Ok(Some(VideoFrame { ts, keyframe, data }))
    }
}

/// Iterate Annex-B NAL unit payloads (without start codes or trailing zero bytes).
pub fn annexb_nals(au: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, s) in starts.iter().enumerate() {
        let mut end = if k + 1 < starts.len() { starts[k + 1] - 3 } else { au.len() };
        // a 4-byte start code (or trailing_zero_8bits) leaves zeros on the previous NAL
        while end > *s && au[end - 1] == 0 {
            end -= 1;
        }
        if end > *s {
            out.push(&au[*s..end]);
        }
    }
    out.into_iter()
}

/// Scan Annex-B NAL units: true if any NAL has type 5 (IDR).
pub fn h264_is_keyframe(au: &[u8]) -> bool {
    annexb_nals(au).any(|n| n.first().map(|b| b & 0x1f == 5).unwrap_or(false))
}

/// "avc1.PPCCLL" codec string from the first SPS (NAL type 7) in an Annex-B access unit.
pub fn h264_codec_string(au: &[u8]) -> Option<String> {
    let sps = annexb_nals(au).find(|n| n.first().map(|b| b & 0x1f == 7).unwrap_or(false))?;
    if sps.len() < 4 {
        return None;
    }
    Some(format!("avc1.{:02x}{:02x}{:02x}", sps[1], sps[2], sps[3]))
}

/// Raw message layout from Rust to the webview over the video Tauri Channel:
/// `[0] flags (bit0 keyframe) | [1..3] track u16 BE | [3..7] ts u32 BE (90 kHz) | [7..] access unit`.
pub fn ui_video_message(track: u16, f: &VideoFrame) -> Vec<u8> {
    let mut v = Vec::with_capacity(7 + f.data.len());
    v.push(if f.keyframe { 1 } else { 0 });
    v.extend_from_slice(&track.to_be_bytes());
    v.extend_from_slice(&f.ts.to_be_bytes());
    v.extend_from_slice(&f.data);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_ids() {
        assert_eq!(track_id(0, KIND_VOICE), 0);
        assert_eq!(track_id(5, KIND_SCREEN), 21);
        assert_eq!(track_short_id(21), 5);
        assert_eq!(track_kind(21), KIND_SCREEN);
        let t = track_id(MAX_SHORT_ID, KIND_SCREEN);
        assert_eq!(track_short_id(t), MAX_SHORT_ID);
        assert_eq!(track_kind(t), KIND_SCREEN);
        // kind is masked to 2 bits
        assert_eq!(track_id(1, 7), 7);
        assert_eq!(track_kind(track_id(3, KIND_SOUND)), KIND_SOUND);
        assert!(is_audio_kind(KIND_VOICE) && is_audio_kind(KIND_SOUND) && !is_audio_kind(KIND_SCREEN));
    }

    #[test]
    fn share_audio_tracks_pair_with_screen_tracks() {
        // The kinds stay distinct and in their wire positions (never renumbered).
        assert_eq!((KIND_VOICE, KIND_SCREEN, KIND_SHARE_AUDIO), (0, 1, 3));
        let audio = track_id(5, KIND_SHARE_AUDIO);
        assert_eq!(audio, 23);
        assert_eq!(track_short_id(audio), 5);
        assert_eq!(track_kind(audio), KIND_SHARE_AUDIO);
        assert_eq!(screen_of_share_audio(audio), Some(track_id(5, KIND_SCREEN)));
        assert_eq!(share_audio_of_screen(track_id(5, KIND_SCREEN)), Some(audio));
        assert_eq!(screen_of_share_audio(track_id(5, KIND_VOICE)), None);
        assert_eq!(screen_of_share_audio(track_id(5, KIND_SCREEN)), None);
        assert_eq!(share_audio_of_screen(track_id(5, KIND_VOICE)), None);
        assert_eq!(share_audio_of_screen(audio), None);
        let top = track_id(MAX_SHORT_ID, KIND_SHARE_AUDIO);
        assert_eq!(screen_of_share_audio(top), Some(track_id(MAX_SHORT_ID, KIND_SCREEN)));
        // A share-audio datagram is an ordinary audio datagram: same header, other kind.
        let pkt = AudioHeader { track: audio, seq: 1, ts: 960, ..AudioHeader::default() }.packet(b"stereo");
        let (h, payload) = AudioHeader::parse(&pkt).unwrap();
        assert_eq!(track_kind(h.track), KIND_SHARE_AUDIO);
        assert_eq!(payload, b"stereo");
        assert_eq!(audio_track_of(&pkt), Some(audio));
    }

    #[test]
    fn audio_header_round_trip() {
        let h = AudioHeader { flags: AF_SPEAKING | AF_FEC, track: 0xBEEF, seq: 0xFFFF, ts: 0xDEAD_BEEF, level: 127, epoch: 9 };
        let pkt = h.packet(b"opus!");
        assert_eq!(pkt.len(), AUDIO_HEADER_LEN + 5);
        assert_eq!(pkt[0], AUDIO_MAGIC);
        assert_eq!(&pkt[2..4], &[0xBE, 0xEF]); // big-endian
        let (back, payload) = AudioHeader::parse(&pkt).unwrap();
        assert_eq!(back, h);
        assert_eq!(payload, b"opus!");
        assert!(back.has(AF_SPEAKING) && back.has(AF_FEC));
        assert!(!back.has(AF_TALKSPURT) && !back.has(AF_SOUNDBOARD));
        assert_eq!(audio_track_of(&pkt), Some(0xBEEF));
        // header only, empty payload
        let bare = h.packet(&[]);
        let (_, empty) = AudioHeader::parse(&bare).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn audio_header_rejects_bad_input() {
        let h = AudioHeader::default();
        let pkt = h.packet(b"x");
        assert!(AudioHeader::parse(&pkt[..AUDIO_HEADER_LEN - 1]).is_none());
        assert!(AudioHeader::parse(&[]).is_none());
        let mut wrong = pkt.to_vec();
        wrong[0] = 0xA2;
        assert!(AudioHeader::parse(&wrong).is_none());
        assert_eq!(audio_track_of(&wrong), None);
        assert_eq!(audio_track_of(&pkt[..3]), None);
    }

    #[test]
    fn video_stream_header_round_trip() {
        let h = VideoStreamHeader { track: 0x1234, group_seq: 0x0102_0304, codec: CODEC_H264_ANNEXB, flags: 0 };
        let b = h.write();
        assert_eq!(b, [0xB1, 0x12, 0x34, 1, 2, 3, 4, 1, 0]);
        assert_eq!(VideoStreamHeader::parse(&b), Some(h));
        assert_eq!(VideoStreamHeader::parse(&b[..8]), None);
        let mut bad = b;
        bad[0] = 0;
        assert_eq!(VideoStreamHeader::parse(&bad), None);
    }

    #[test]
    fn varint_boundaries() {
        let cases: [(u64, usize); 9] = [
            (0, 1),
            (63, 1),
            (64, 2),
            (16_383, 2),
            (16_384, 4),
            ((1 << 30) - 1, 4),
            (1 << 30, 8),
            (VARINT_MAX, 8),
            (u64::MAX, 8),
        ];
        for (v, n) in cases {
            let mut out = Vec::new();
            put_varint(&mut out, v);
            assert_eq!(out.len(), n, "length for {v}");
            let (back, used) = get_varint(&out).unwrap();
            assert_eq!(used, n);
            assert_eq!(back, v.min(VARINT_MAX));
            // truncated input is "need more", never a panic
            assert!(get_varint(&out[..n - 1]).is_none() || n == 1);
        }
        assert!(get_varint(&[]).is_none());
        // RFC 9000 examples
        assert_eq!(get_varint(&[0x9d, 0x7f, 0x3e, 0x7d]), Some((494_878_333, 4)));
        assert_eq!(get_varint(&[0x7b, 0xbd]), Some((15_293, 2)));
        assert_eq!(get_varint(&[0x25]), Some((37, 1)));
    }

    #[test]
    fn video_frames_round_trip_with_split_pushes() {
        let frames: Vec<VideoFrame> = vec![
            VideoFrame { ts: 1, keyframe: true, data: Bytes::from(vec![0, 0, 0, 1, 0x65, 1, 2, 3]) },
            VideoFrame { ts: 3000, keyframe: false, data: Bytes::from(vec![7u8; 100]) },
            VideoFrame { ts: 6000, keyframe: false, data: Bytes::new() },
            VideoFrame { ts: u32::MAX, keyframe: false, data: Bytes::from(vec![9u8; 20_000]) },
        ];
        let mut stream = Vec::new();
        for f in &frames {
            stream.extend_from_slice(&encode_video_frame(f));
        }
        for chunk in [1usize, 2, 3, 7, 64, 4096, stream.len()] {
            let mut r = VideoFrameReader::new();
            let mut got = Vec::new();
            for piece in stream.chunks(chunk) {
                r.push(piece);
                while let Some(f) = r.next().unwrap() {
                    got.push(f);
                }
            }
            assert_eq!(got, frames, "chunk size {chunk}");
            assert_eq!(r.buffered(), 0);
        }
    }

    #[test]
    fn video_reader_rejects_malformed() {
        // declared length below the 5-byte minimum
        let mut r = VideoFrameReader::new();
        r.push(&[4, 0, 0, 0, 0]);
        assert!(r.next().is_err());
        // declared length above the maximum
        let mut r = VideoFrameReader::new();
        let mut b = Vec::new();
        put_varint(&mut b, MAX_VIDEO_FRAME as u64 + 1);
        r.push(&b);
        assert!(r.next().is_err());
        // incomplete frame is not an error
        let mut r = VideoFrameReader::new();
        let full = encode_video_frame(&VideoFrame { ts: 0, keyframe: true, data: Bytes::from_static(b"abc") });
        r.push(&full[..full.len() - 1]);
        assert_eq!(r.next(), Ok(None));
        r.push(&full[full.len() - 1..]);
        assert!(r.next().unwrap().unwrap().keyframe);
        // a 3 MiB frame (allowed before) is now refused
        let mut r = VideoFrameReader::new();
        let mut b = Vec::new();
        put_varint(&mut b, 3 * 1024 * 1024);
        r.push(&b);
        assert!(r.next().is_err());
    }

    #[test]
    fn video_reader_is_linear_in_many_tiny_frames() {
        // A buffer full of minimal frames used to cost O(n^2) (a drain per frame): 220k
        // frames took minutes.
        let one = encode_video_frame(&VideoFrame { ts: 1, keyframe: false, data: Bytes::new() });
        let all: Vec<u8> = one.iter().copied().cycle().take(one.len() * 220_000).collect();
        let mut r = VideoFrameReader::new();
        let t0 = std::time::Instant::now();
        let mut n = 0usize;
        r.push(&all);
        while r.next().unwrap().is_some() {
            n += 1;
        }
        assert_eq!(n, 220_000);
        assert!(!r.mid_frame());
        assert!(t0.elapsed() < std::time::Duration::from_secs(2), "took {:?}", t0.elapsed());
    }

    #[test]
    fn annexb_parsing() {
        // 4-byte start code SPS, 3-byte start code PPS, 4-byte IDR, trailing zero
        let au = [0, 0, 0, 1, 0x67, 0x64, 0x00, 0x1f, 0xAA, 0, 0, 1, 0x68, 0xBB, 0, 0, 0, 1, 0x65, 0x88, 0x84, 0];
        let nals: Vec<&[u8]> = annexb_nals(&au).collect();
        assert_eq!(nals, vec![&[0x67, 0x64, 0x00, 0x1f, 0xAA][..], &[0x68, 0xBB][..], &[0x65, 0x88, 0x84][..]]);
        assert!(h264_is_keyframe(&au));
        assert_eq!(h264_codec_string(&au).as_deref(), Some("avc1.64001f"));

        let p_frame = [0, 0, 0, 1, 0x41, 0x9a, 0x01];
        assert!(!h264_is_keyframe(&p_frame));
        assert_eq!(h264_codec_string(&p_frame), None);

        // no start code, empty input, start code with no payload, truncated SPS
        assert_eq!(annexb_nals(&[0x65, 1, 2]).count(), 0);
        assert_eq!(annexb_nals(&[]).count(), 0);
        assert_eq!(annexb_nals(&[0, 0, 1]).count(), 0);
        assert!(!h264_is_keyframe(&[0, 0, 1]));
        assert_eq!(h264_codec_string(&[0, 0, 1, 0x67, 0x42]), None);
    }

    #[test]
    fn ui_video_message_layout() {
        let f = VideoFrame { ts: 0x0102_0304, keyframe: true, data: Bytes::from_static(b"AU") };
        assert_eq!(ui_video_message(0x0506, &f), vec![1, 5, 6, 1, 2, 3, 4, b'A', b'U']);
        let f = VideoFrame { ts: 0, keyframe: false, data: Bytes::new() };
        assert_eq!(ui_video_message(1, &f), vec![0, 0, 1, 0, 0, 0, 0]);
    }
}
