//! Soundboard volumes: one per person (a multiplier on their clips, on top of their overall
//! volume) and one per voice room (a multiplier on every clip I hear in that room, mine
//! included). Both are local, saved in the kv store like the per-user volumes, and applied
//! to the soundboard tracks (`KIND_SOUND`) the engine keeps apart from each voice.
//!
//! A remote member's soundboard track plays at `overall * person soundboard * room
//! soundboard` (the engine clamps track gains at 4). My own clips play here at `room
//! soundboard`; what I send is never scaled.

use std::collections::HashMap;
use std::sync::Arc;

use tincan_core::media::{KIND_SOUND, KIND_VOICE, track_id};
use tincan_core::storage::Storage;
use tincan_core::{ChannelId, UserId};

use super::session::Kind;
use super::{Media, State};
use crate::app::Core;
use crate::dto;

/// kv key of the per-user soundboard volumes (JSON object: z32 -> gain).
const USER_SOUND_KEY: &str = "app/user_soundboard_volumes";
/// kv key of the per-room soundboard volumes (JSON object: voice room id hex -> gain).
const ROOM_SOUND_KEY: &str = "app/room_soundboard_volumes";
/// Saved volumes are 0..2 (the UI's 0 to 200 %).
const MAX_VOLUME: f32 = 2.0;
/// Rooms remembered (a room id comes from the user's own commands, so this only bounds a
/// very long history of deleted rooms).
const MAX_ROOMS: usize = 1000;

/// A volume from the UI or the store: finite, 0..2, else an error / 1.
pub(super) fn checked(volume: f64) -> Result<f32, String> {
    if !volume.is_finite() {
        return Err("Volume must be a number between 0 and 2.".into());
    }
    Ok(volume.clamp(0.0, f64::from(MAX_VOLUME)) as f32)
}

fn stored(v: f32) -> f32 { if v.is_finite() { v.clamp(0.0, MAX_VOLUME) } else { 1.0 } }

/// Rounded the way the slider shows it.
pub(super) fn shown(v: f32) -> f64 { (f64::from(v) * 1000.0).round() / 1000.0 }

fn load<K: std::hash::Hash + Eq>(core: &Core, key: &str, parse: impl Fn(&str) -> Option<K>) -> HashMap<K, f32> {
    let Ok(Some(raw)) = core.storage.get_kv(key) else { return HashMap::new() };
    let map: HashMap<String, f32> = serde_json::from_slice(&raw).unwrap_or_default();
    map.into_iter().filter_map(|(k, v)| Some((parse(&k)?, stored(v)))).collect()
}

fn save<K>(core: &Core, key: &str, map: &HashMap<K, f32>, show: impl Fn(&K) -> String) {
    let out: HashMap<String, f32> = map.iter().map(|(k, v)| (show(k), *v)).collect();
    if let Ok(json) = serde_json::to_vec(&out)
        && let Err(e) = core.storage.set_kv(key, &json)
    {
        tracing::warn!(error = %e, key, "saving soundboard volumes failed");
    }
}

/// Persisted per-user soundboard volumes.
pub(super) fn load_user_sound(core: &Core) -> HashMap<UserId, f32> { load(core, USER_SOUND_KEY, UserId::from_z32) }

/// Persisted per-room soundboard volumes.
pub(super) fn load_room_sound(core: &Core) -> HashMap<ChannelId, f32> { load(core, ROOM_SOUND_KEY, ChannelId::from_hex) }

/// Gain of a remote member's soundboard track.
pub(super) fn sound_gain(overall: f32, person: f32, room: f32) -> f32 { (overall * person * room).clamp(0.0, 4.0) }

/// Insert `v`, or remove the entry when it is the default 1 (so the map holds only changes).
fn put<K: std::hash::Hash + Eq>(map: &mut HashMap<K, f32>, k: K, v: f32) {
    if (v - 1.0).abs() < 1e-4 {
        map.remove(&k);
    } else {
        map.insert(k, v);
    }
}

impl Media {
    /// The soundboard volume of the room I am in (1 in a call or out of voice).
    pub(super) fn current_room_sound(st: &State) -> f32 {
        match st.attached.as_ref().map(|a| a.kind) {
            Some(Kind::Room { channel, .. }) => st.room_sound.get(&channel).copied().unwrap_or(1.0),
            _ => 1.0,
        }
    }

    /// Gains of `user`'s voice and soundboard tracks (short id `short`) and of my own clips.
    pub(super) fn apply_user_gains(e: &tincan_audio::AudioEngine, st: &State, user: &UserId, short: u16) {
        let overall = st.volumes.get(user).copied().unwrap_or(1.0);
        let person = st.sound_volumes.get(user).copied().unwrap_or(1.0);
        e.set_track_volume(track_id(short, KIND_VOICE), overall);
        e.set_track_volume(track_id(short, KIND_SOUND), sound_gain(overall, person, Self::current_room_sound(st)));
    }

    /// `set_user_soundboard_volume` (0..2, remembered across sessions and restarts).
    pub(super) fn set_user_soundboard_volume(&self, core: &Arc<Core>, user: UserId, volume: f64) -> Result<(), String> {
        let v = checked(volume)?;
        let mut st = self.st.lock();
        put(&mut st.sound_volumes, user, v);
        save(core, USER_SOUND_KEY, &st.sound_volumes, UserId::to_z32);
        if let Some(e) = self.engine_handle() {
            self.apply_volumes(&e, &st);
        }
        // `VoiceParticipant.soundboardVolume` changed.
        self.emit_voice(core, &mut st);
        Ok(())
    }

    /// `set_room_soundboard_volume` (0..2 per voice room, remembered).
    pub(super) fn set_room_soundboard_volume(&self, core: &Arc<Core>, channel: ChannelId, volume: f64) -> Result<(), String> {
        let v = checked(volume)?;
        let mut st = self.st.lock();
        if st.room_sound.len() >= MAX_ROOMS && !st.room_sound.contains_key(&channel) {
            st.room_sound.clear();
        }
        put(&mut st.room_sound, channel, v);
        save(core, ROOM_SOUND_KEY, &st.room_sound, ChannelId::to_hex);
        if let Some(e) = self.engine_handle() {
            self.apply_volumes(&e, &st);
        }
        Ok(())
    }

    /// `room_soundboard_volume`.
    pub(super) fn room_soundboard_volume(&self, channel: &ChannelId) -> f64 { shown(self.st.lock().room_sound.get(channel).copied().unwrap_or(1.0)) }

    /// `user_volume`: both saved volumes of anyone (1 when never changed).
    pub(super) fn user_volume(&self, user: &UserId) -> dto::UserVolume {
        let st = self.st.lock();
        dto::UserVolume {
            volume: shown(st.volumes.get(user).copied().unwrap_or(1.0)),
            soundboard_volume: shown(st.sound_volumes.get(user).copied().unwrap_or(1.0)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gains_multiply_and_clamp() {
        assert_eq!(sound_gain(1.0, 1.0, 1.0), 1.0);
        assert!((sound_gain(0.5, 0.5, 2.0) - 0.5).abs() < 1e-6);
        assert_eq!(sound_gain(2.0, 2.0, 2.0), 4.0, "engine maximum");
        assert_eq!(sound_gain(2.0, 0.0, 2.0), 0.0, "a muted person's clips stay muted");
    }

    #[test]
    fn volumes_are_checked_and_defaults_are_not_kept() {
        assert!(checked(f64::NAN).is_err());
        assert_eq!(checked(3.0).unwrap(), 2.0);
        assert_eq!(checked(-1.0).unwrap(), 0.0);
        let mut m = HashMap::new();
        put(&mut m, 1u8, 0.5);
        assert_eq!(m.get(&1), Some(&0.5));
        put(&mut m, 1u8, 1.0);
        assert!(m.is_empty(), "back to the default: no entry");
        assert_eq!(stored(f32::INFINITY), 1.0);
        assert_eq!(shown(0.333_33), 0.333);
    }
}
