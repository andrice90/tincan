// Settings: profile, voice, files, network, appearance.

import { useEffect, useRef, useState } from "react";
import { errorText, invoke, listen, pickFiles } from "../api/client";
import type { AudioDevice, AudioDevices, Settings } from "../api/types";
import { CopyButton } from "../components/CopyButton";
import { Modal } from "../components/Modal";
import { hostLine } from "../components/ChannelSidebar";
import { formatBytes } from "../lib/format";
import { Avatar } from "../components/common";
import { closeDialog, pickImageFor, saveSettings, setAvatar, setName } from "../store/actions";
import { useStore, type SettingsTab } from "../store/store";
import { connStatus, debounced, fetchLimitOptions } from "./settingsLogic";

const TABS: { id: SettingsTab; label: string }[] = [
  { id: "profile", label: "Profile" },
  { id: "voice", label: "Voice" },
  { id: "files", label: "Files" },
  { id: "network", label: "Network" },
  { id: "appearance", label: "Appearance" },
];

function useSave(): { error: string | null; save: (change: Partial<Settings>) => void } {
  const [error, setError] = useState<string | null>(null);
  return {
    error,
    save: (change) => {
      setError(null);
      saveSettings(change).catch((err: unknown) => setError(errorText(err)));
    },
  };
}

function Switch({ id, label, checked, onChange }: { id: string; label: string; checked: boolean; onChange: (v: boolean) => void }) {
  return (
    <label className="switch-row" htmlFor={id}>
      <span>{label}</span>
      <input id={id} type="checkbox" role="switch" className="switch" checked={checked} onChange={(e) => onChange(e.target.checked)} />
    </label>
  );
}

function ProfileTab() {
  const me = useStore((s) => s.me);
  const [name, setNameValue] = useState(me?.name ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    if (!saved) return;
    const t = setTimeout(() => setSaved(false), 2000);
    return () => clearTimeout(t);
  }, [saved]);

  if (!me) return null;
  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await setName(name.trim());
      setSaved(true);
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  };

  const removeAvatar = async () => {
    setError(null);
    try {
      await setAvatar(null);
    } catch (err) {
      setError(errorText(err));
    }
  };

  return (
    <>
      <div className="field">
        <span className="field-label">Avatar</span>
        <div className="avatar-row">
          <Avatar userId={me.userId} name={me.name} size={64} />
          <button type="button" className="btn" onClick={() => pickImageFor({ kind: "avatar" }, { kind: "settings", tab: "profile" })}>
            Change avatar
          </button>
          {me.avatar && (
            <button type="button" className="btn btn-ghost" onClick={() => void removeAvatar()}>
              Remove
            </button>
          )}
        </div>
      </div>
      <div className="field">
        <label className="field-label" htmlFor="profile-name">
          Display name
        </label>
        <div className="input-row">
          <input
            id="profile-name"
            className="input"
            value={name}
            maxLength={32}
            onChange={(e) => {
              setNameValue(e.target.value);
              setError(null);
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter") void save();
            }}
          />
          <button type="button" className="btn" disabled={busy || !name.trim() || name.trim() === me.name} onClick={() => void save()}>
            {saved ? "Saved" : "Save"}
          </button>
        </div>
      </div>
      {error && (
        <p className="error-text" role="alert">
          {error}
        </p>
      )}
      <FriendCodeField />
      <div className="field field-start">
        <CopyButton text={me.userCode} label="Copy full ID" />
        <p className="field-note">Your full ID never changes, even when you rotate the friend code.</p>
      </div>
    </>
  );
}

function FriendCodeField() {
  const [code, setCode] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [rotating, setRotating] = useState(false);
  const live = useRef(true);

  useEffect(() => {
    live.current = true;
    invoke("get_friend_code", {}).then(
      (c) => live.current && setCode(c),
      (err: unknown) => live.current && setError(errorText(err)),
    );
    return () => {
      live.current = false;
    };
  }, []);

  const rotate = async () => {
    setRotating(true);
    setError(null);
    try {
      const c = await invoke("rotate_friend_code", {});
      if (live.current) setCode(c);
    } catch (err) {
      if (live.current) setError(errorText(err));
    } finally {
      if (live.current) {
        setRotating(false);
        setConfirming(false);
      }
    }
  };

  return (
    <div className="field">
      <label className="field-label" htmlFor="profile-friend-code">
        Friend code
      </label>
      <div className="input-row">
        <input
          id="profile-friend-code"
          className="input mono"
          readOnly
          value={code ?? ""}
          placeholder={error ? "" : "Making a code"}
          onFocus={(e) => e.currentTarget.select()}
        />
        {confirming ? (
          <>
            <button type="button" className="btn btn-ghost" disabled={rotating} onClick={() => setConfirming(false)}>
              Cancel
            </button>
            <button type="button" className="btn btn-danger" disabled={rotating} onClick={() => void rotate()}>
              {rotating ? "Rotating" : "Rotate"}
            </button>
          </>
        ) : (
          <>
            <CopyButton text={code} label="Copy" />
            <button type="button" className="btn" disabled={!code} onClick={() => setConfirming(true)}>
              Rotate
            </button>
          </>
        )}
      </div>
      <p className="field-note">
        {confirming ? "Your current code stops working. Contacts you already have stay." : "People add you as a contact with this code."}
      </p>
      {error && (
        <p className="error-text" role="alert">
          {error}
        </p>
      )}
    </div>
  );
}

function DeviceSelect({
  id,
  label,
  devices,
  value,
  onChange,
}: {
  id: string;
  label: string;
  devices: AudioDevice[];
  value: string | null;
  onChange: (v: string | null) => void;
}) {
  const def = devices.find((d) => d.isDefault);
  return (
    <div className="field">
      <label className="field-label" htmlFor={id}>
        {label}
      </label>
      <select id={id} className="select" value={value ?? ""} onChange={(e) => onChange(e.target.value || null)}>
        <option value="">{def ? `Windows default (${def.name})` : "Windows default"}</option>
        {devices.map((d) => (
          <option key={d.id} value={d.id}>
            {d.name}
          </option>
        ))}
      </select>
    </div>
  );
}

function LevelMeter() {
  const fill = useRef<HTMLDivElement>(null);
  useEffect(() => {
    let on = true;
    invoke("mic_test", { on: true }).catch(() => {
      on = false;
    });
    const off = listen("mic-level", ({ level }) => {
      // direct style write: 20 updates a second should not re-render React
      if (fill.current) fill.current.style.transform = `scaleX(${Math.max(0, Math.min(1, level))})`;
    });
    return () => {
      off();
      if (on) invoke("mic_test", { on: false }).catch(() => {});
    };
  }, []);
  return (
    <div className="field">
      <span className="field-label" id="meter-label">
        Input level
      </span>
      <div className="meter" role="meter" aria-labelledby="meter-label" aria-valuemin={0} aria-valuemax={1}>
        <div className="meter-fill" ref={fill} />
      </div>
    </div>
  );
}

/** A 0..200% slider that saves shortly after the last change, and at once when it closes. */
function VolumeSlider({ id, label, value, onCommit }: { id: string; label: string; value: number; onCommit: (v: number) => void }) {
  const [v, setV] = useState(value);
  const commit = useRef(onCommit);
  commit.current = onCommit;
  const [save] = useState(() =>
    debounced<number>((x) => commit.current(x), 250, {
      set: (fn, ms) => setTimeout(fn, ms),
      clear: (t) => clearTimeout(t as ReturnType<typeof setTimeout>),
    }),
  );
  // closing Settings or switching tabs within the debounce must not drop the change
  useEffect(() => () => save.flush(), [save]);
  return (
    <div className="field">
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
            save.push(next);
          }}
        />
        <span className="volume-value">{Math.round(v * 100)}%</span>
      </div>
    </div>
  );
}

function VoiceTab({ settings, save }: { settings: Settings; save: (c: Partial<Settings>) => void }) {
  const [devices, setDevices] = useState<AudioDevices | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    invoke("audio_devices", {}).then(
      (d) => live && setDevices(d),
      (err: unknown) => live && setError(errorText(err)),
    );
    return () => {
      live = false;
    };
  }, []);

  return (
    <>
      {error && (
        <p className="error-text" role="alert">
          {error}
        </p>
      )}
      <DeviceSelect
        id="input-device"
        label="Input device"
        devices={devices?.inputs ?? []}
        value={settings.inputDeviceId}
        onChange={(inputDeviceId) => save({ inputDeviceId })}
      />
      <LevelMeter />
      <VolumeSlider id="input-volume" label="Input volume" value={settings.inputVolume} onCommit={(inputVolume) => save({ inputVolume })} />
      <DeviceSelect
        id="output-device"
        label="Output device"
        devices={devices?.outputs ?? []}
        value={settings.outputDeviceId}
        onChange={(outputDeviceId) => save({ outputDeviceId })}
      />
      <VolumeSlider id="output-volume" label="Output volume" value={settings.outputVolume} onCommit={(outputVolume) => save({ outputVolume })} />
      <div className="settings-switches">
        <Switch
          id="noise-suppression"
          label="Noise suppression"
          checked={settings.noiseSuppression}
          onChange={(noiseSuppression) => save({ noiseSuppression })}
        />
        <Switch
          id="echo-cancellation"
          label="Echo cancellation"
          checked={settings.echoCancellation}
          onChange={(echoCancellation) => save({ echoCancellation })}
        />
      </div>
    </>
  );
}

function FilesTab({ settings, save }: { settings: Settings; save: (c: Partial<Settings>) => void }) {
  const limits = fetchLimitOptions(settings.autoFetchLimit);
  const [error, setError] = useState<string | null>(null);
  const pick = async () => {
    setError(null);
    try {
      const [dir] = await pickFiles({ multiple: false, directory: true });
      if (dir) save({ downloadDir: dir });
    } catch (err) {
      setError(errorText(err));
    }
  };
  return (
    <>
      <div className="field">
        <label className="field-label" htmlFor="auto-fetch">
          Download files automatically
        </label>
        <select
          id="auto-fetch"
          className="select"
          value={settings.autoFetchLimit}
          onChange={(e) => save({ autoFetchLimit: Number(e.target.value) })}
        >
          {limits.map((n) => (
            <option key={n} value={n}>
              {n === 0 ? "Never" : `Up to ${formatBytes(n)}`}
            </option>
          ))}
        </select>
      </div>
      <div className="field">
        <label className="field-label" htmlFor="download-dir">
          Download folder
        </label>
        <div className="input-row">
          <input id="download-dir" className="input" readOnly value={settings.downloadDir} title={settings.downloadDir} />
          <button type="button" className="btn" onClick={() => void pick()}>
            Change folder
          </button>
        </div>
      </div>
      {error && (
        <p className="error-text" role="alert">
          {error}
        </p>
      )}
    </>
  );
}

function NetworkTab({ settings, save, saveError }: { settings: Settings; save: (c: Partial<Settings>) => void; saveError: string | null }) {
  const servers = useStore((s) => s.servers);
  const meId = useStore((s) => s.me?.userId);
  const [relay, setRelay] = useState(settings.customRelayUrl ?? "");
  const changed = (relay.trim() || null) !== settings.customRelayUrl;
  return (
    <>
      <Switch id="allow-hosting" label="Allow this PC to host servers" checked={settings.allowHosting} onChange={(allowHosting) => save({ allowHosting })} />
      <div className="field">
        <label className="field-label" htmlFor="relay-url">
          Custom relay URL
        </label>
        <div className="input-row">
          <input
            id="relay-url"
            className="input"
            value={relay}
            placeholder="https://relay.example.com"
            spellCheck={false}
            onChange={(e) => setRelay(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && changed) save({ customRelayUrl: relay.trim() || null });
            }}
          />
          <button type="button" className="btn" disabled={!changed} onClick={() => save({ customRelayUrl: relay.trim() || null })}>
            Save
          </button>
        </div>
        <p className="field-note">Leave empty to use the public relays.</p>
      </div>
      {saveError && (
        <p className="error-text" role="alert">
          {saveError}
        </p>
      )}
      <div className="field">
        <span className="field-label">Connection status</span>
        {servers.length === 0 ? (
          <p className="muted">You're not in any servers.</p>
        ) : (
          <ul className="conn-list">
            {servers.map((s) => {
              const status = connStatus(s.connection, s.onlineCount);
              return (
                <li key={s.id} className="conn-row">
                  <span className="truncate">
                    <bdi>{s.name}</bdi>
                  </span>
                  <span className="muted truncate">{hostLine(s.connection, meId)}</span>
                  <span className={status.cls}>{status.text}</span>
                </li>
              );
            })}
          </ul>
        )}
      </div>
    </>
  );
}

function AppearanceTab({ settings, save }: { settings: Settings; save: (c: Partial<Settings>) => void }) {
  const options: { id: Settings["theme"]; label: string }[] = [
    { id: "system", label: "Match Windows" },
    { id: "dark", label: "Dark" },
    { id: "light", label: "Light" },
  ];
  return (
    <div className="field">
      <span className="field-label" id="theme-label">
        Theme
      </span>
      <div className="segmented" role="radiogroup" aria-labelledby="theme-label">
        {options.map((o) => (
          <button key={o.id} type="button" role="radio" aria-checked={settings.theme === o.id} onClick={() => save({ theme: o.id })}>
            {o.label}
          </button>
        ))}
      </div>
    </div>
  );
}

export function SettingsDialog({ initialTab }: { initialTab: SettingsTab }) {
  const [tab, setTab] = useState<SettingsTab>(initialTab);
  const settings = useStore((s) => s.settings);
  const version = useStore((s) => s.version);
  const { error, save } = useSave();

  return (
    <Modal
      title="Settings"
      onClose={closeDialog}
      className="modal-settings"
      footer={
        <button type="button" className="btn btn-primary" onClick={closeDialog}>
          Done
        </button>
      }
    >
      <div className="settings">
        <nav className="settings-nav" aria-label="Settings sections">
          <div role="tablist" aria-orientation="vertical">
            {TABS.map((t) => (
              <button
                key={t.id}
                type="button"
                role="tab"
                id={`tab-${t.id}`}
                aria-selected={tab === t.id}
                aria-controls="settings-panel"
                className="settings-tab"
                onClick={() => setTab(t.id)}
              >
                {t.label}
              </button>
            ))}
          </div>
          {version && <span className="settings-version faint">Tincan {version}</span>}
        </nav>
        <div className="settings-panel" id="settings-panel" role="tabpanel" aria-labelledby={`tab-${tab}`}>
          {error && tab !== "network" && (
            <p className="error-text settings-error" role="alert">
              {error}
            </p>
          )}
          {!settings ? null : tab === "profile" ? (
            <ProfileTab />
          ) : tab === "voice" ? (
            <VoiceTab settings={settings} save={save} />
          ) : tab === "files" ? (
            <FilesTab settings={settings} save={save} />
          ) : tab === "network" ? (
            <NetworkTab settings={settings} save={save} saveError={error} />
          ) : (
            <AppearanceTab settings={settings} save={save} />
          )}
        </div>
      </div>
    </Modal>
  );
}
