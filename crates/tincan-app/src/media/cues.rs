//! Join and leave sounds of voice rooms. Everyone in a room hears the join sound when
//! someone enters (the one entering too) and the leave sound when someone leaves (and when
//! they leave themselves). They play locally through the audio engine, so they follow the
//! chosen speaker and stay silent while deafened; nothing is sent to the call.

use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};

use tincan_audio::{AudioEngine, ClipPcm};

static JOIN_WAV: &[u8] = include_bytes!("../../assets/voice-join.wav");
static LEAVE_WAV: &[u8] = include_bytes!("../../assets/voice-leave.wav");

/// Playback gain of the cues: they sit under voices and soundboard clips.
const CUE_GAIN: f32 = 0.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Cue {
    Join,
    Leave,
}

/// The voice room as last seen: its conversation and the other people in it.
pub(super) type Seen = Option<(String, BTreeSet<String>)>;

/// The cue a change of room membership calls for, if any.
pub(super) fn cue_for(prev: &Seen, next: &Seen) -> Option<Cue> {
    match (prev, next) {
        (None, None) => None,
        (None, Some(_)) => Some(Cue::Join),
        (Some(_), None) => Some(Cue::Leave),
        (Some((a, was)), Some((b, now))) => {
            if a != b || now.difference(was).next().is_some() {
                Some(Cue::Join)
            } else if was.difference(now).next().is_some() {
                Some(Cue::Leave)
            } else {
                None
            }
        }
    }
}

/// The decoded cue, decoded once per process (None if the bundled file fails to decode).
pub(super) fn pcm(cue: Cue) -> Option<Arc<ClipPcm>> {
    static JOIN: OnceLock<Option<Arc<ClipPcm>>> = OnceLock::new();
    static LEAVE: OnceLock<Option<Arc<ClipPcm>>> = OnceLock::new();
    let (cell, bytes) = match cue {
        Cue::Join => (&JOIN, JOIN_WAV),
        Cue::Leave => (&LEAVE, LEAVE_WAV),
    };
    cell.get_or_init(|| match AudioEngine::decode_clip(bytes, 10_000) {
        Ok(c) => Some(Arc::new(ClipPcm::from_samples(c.samples().iter().map(|s| s * CUE_GAIN).collect()))),
        Err(e) => {
            tracing::warn!(%e, ?cue, "voice cue does not decode");
            None
        }
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(conv: &str, people: &[&str]) -> Seen { Some((conv.to_string(), people.iter().map(|p| (*p).to_string()).collect())) }

    #[test]
    fn membership_changes_pick_the_right_cue() {
        assert_eq!(cue_for(&None, &None), None);
        assert_eq!(cue_for(&None, &seen("r", &[])), Some(Cue::Join), "I join an empty room");
        assert_eq!(cue_for(&None, &seen("r", &["bob"])), Some(Cue::Join), "I join a room with people");
        assert_eq!(cue_for(&seen("r", &["bob"]), &None), Some(Cue::Leave), "I leave");
        assert_eq!(cue_for(&seen("r", &[]), &seen("r", &["bob"])), Some(Cue::Join), "someone enters");
        assert_eq!(cue_for(&seen("r", &["bob"]), &seen("r", &[])), Some(Cue::Leave), "someone leaves");
        assert_eq!(cue_for(&seen("r", &["bob"]), &seen("r", &["bob"])), None, "no change");
        assert_eq!(cue_for(&seen("r", &["bob"]), &seen("s", &["bob"])), Some(Cue::Join), "I move rooms");
        assert_eq!(cue_for(&seen("r", &["bob"]), &seen("r", &["eve"])), Some(Cue::Join), "one in, one out");
    }

    #[test]
    fn bundled_cues_decode() {
        for c in [Cue::Join, Cue::Leave] {
            let p = pcm(c).expect("decodes");
            assert!(p.duration_ms() > 1000 && p.duration_ms() < 5000, "{c:?} {} ms", p.duration_ms());
            assert!(p.samples().iter().all(|s| s.abs() <= CUE_GAIN + 0.01));
        }
    }
}
