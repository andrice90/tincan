// Volume sliders for the right-click menus: a person's overall and soundboard volume
// (member list, voice tiles) and a voice room's soundboard volume (channel list).

import { useEffect, useRef, useState } from "react";
import { invoke } from "../api/client";
import type { ChannelId, UserId } from "../api/types";
import { setRoomSoundboardVolume, setUserSoundboardVolume, setUserVolume } from "../store/actions";
import { initialVolume, shouldSend } from "../store/calls";

/** Volumes picked in this session (0..2); the app's saved values seed them. */
const picked = {
  voice: new Map<UserId, number>(),
  sound: new Map<UserId, number>(),
  room: new Map<ChannelId, number>(),
};

function PercentSlider({ id, label, start, onPick, onCommit }: { id: string; label: string; start: number; onPick: (v: number) => void; onCommit: (v: number) => void }) {
  const [v, setV] = useState(start);
  // The value last sent (the one shown at open until then): a release or key that changed
  // nothing sends nothing.
  const sent = useRef(start);
  const commit = () => {
    if (!shouldSend(sent.current, v)) return;
    sent.current = v;
    onCommit(v);
  };
  return (
    <>
      <label className="field-label" htmlFor={id}>
        {label}
      </label>
      <div className="volume-row">
        <input
          id={id}
          type="range"
          min={0}
          max={200}
          step={5}
          value={Math.round(v * 100)}
          onChange={(e) => {
            const next = Number(e.target.value) / 100;
            setV(next);
            onPick(next);
          }}
          onPointerUp={commit}
          onKeyUp={commit}
        />
        <span className="volume-value">{Math.round(v * 100)}%</span>
      </div>
    </>
  );
}

/**
 * Someone's volume and soundboard volume. `known` are the values a voice session reported;
 * without them the saved values are asked for (`user_volume`), and nothing shows until they
 * arrive, so a release can never save a made-up 100%.
 */
export function PersonVolumes({ userId, known }: { userId: UserId; known?: { volume: number | null; soundboardVolume: number | null } }) {
  const [saved, setSaved] = useState<{ volume: number; soundboardVolume: number } | null>(
    known && known.volume !== null && known.soundboardVolume !== null ? { volume: known.volume, soundboardVolume: known.soundboardVolume } : null,
  );
  useEffect(() => {
    if (saved) return;
    let live = true;
    invoke("user_volume", { userId }).then(
      (v) => live && setSaved(v),
      () => {},
    );
    return () => {
      live = false;
    };
  }, [userId, saved]);
  const voice = initialVolume(picked.voice.get(userId), saved?.volume ?? null);
  const sound = initialVolume(picked.sound.get(userId), saved?.soundboardVolume ?? null);
  return (
    <div className="volume-sliders">
      {voice !== null && (
        <PercentSlider
          id={`vol-${userId}`}
          label="Volume"
          start={voice}
          onPick={(v) => picked.voice.set(userId, v)}
          onCommit={(v) => setUserVolume(userId, v)}
        />
      )}
      {sound !== null && (
        <PercentSlider
          id={`sbvol-${userId}`}
          label="Soundboard volume"
          start={sound}
          onPick={(v) => picked.sound.set(userId, v)}
          onCommit={(v) => setUserSoundboardVolume(userId, v)}
        />
      )}
    </div>
  );
}

/** A voice room's soundboard volume: every clip I hear in it, mine included. */
export function RoomSoundVolume({ channelId }: { channelId: ChannelId }) {
  const [saved, setSaved] = useState<number | null>(null);
  useEffect(() => {
    let live = true;
    invoke("room_soundboard_volume", { channelId }).then(
      (v) => live && setSaved(v),
      () => {},
    );
    return () => {
      live = false;
    };
  }, [channelId]);
  const start = initialVolume(picked.room.get(channelId), saved);
  if (start === null) return null;
  return (
    <div className="volume-sliders">
      <PercentSlider
        id={`roomsb-${channelId}`}
        label="Soundboard volume"
        start={start}
        onPick={(v) => picked.room.set(channelId, v)}
        onCommit={(v) => setRoomSoundboardVolume(channelId, v)}
      />
    </div>
  );
}
