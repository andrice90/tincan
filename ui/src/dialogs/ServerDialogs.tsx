// Dialogs for servers, channels, messages and contacts.

import { useEffect, useState } from "react";
import { copyText, errorText, invoke } from "../api/client";
import type { ChannelId, ChannelKind, Message, ServerId } from "../api/types";
import {
  addContact,
  closeDialog,
  createChannel,
  createServer,
  deleteChannel,
  deleteMessage,
  joinServer,
  leaveServer,
  renameChannel,
  renameServer,
} from "../store/actions";
import { useStore } from "../store/store";
import { Modal } from "../components/Modal";
import { useSubmit } from "./useSubmit";

const NAME_MAX = 48;

function Footer({ busy, label, danger, disabled }: { busy: boolean; label: string; danger?: boolean; disabled?: boolean }) {
  return (
    <>
      <button type="button" className="btn btn-ghost" onClick={closeDialog}>
        Cancel
      </button>
      <button type="submit" className={danger ? "btn btn-danger" : "btn btn-primary"} disabled={busy || disabled}>
        {label}
      </button>
    </>
  );
}

function ErrorLine({ error }: { error: string | null }) {
  if (!error) return null;
  return (
    <p className="error-text dialog-error" role="alert">
      {error}
    </p>
  );
}

function NameDialog({
  title,
  label,
  initial,
  action,
  submitLabel,
  normalize,
}: {
  title: string;
  label: string;
  initial: string;
  action: (name: string) => Promise<void>;
  submitLabel: string;
  normalize?: (v: string) => string;
}) {
  const [name, setName] = useState(initial);
  const s = useSubmit(() => action(name.trim()), closeDialog);
  return (
    <Modal
      title={title}
      onClose={closeDialog}
      onSubmit={s.submit}
      footer={<Footer busy={s.busy} label={submitLabel} disabled={!name.trim()} />}
    >
      <div className="field">
        <label className="field-label" htmlFor="name-input">
          {label}
        </label>
        <input
          id="name-input"
          className="input"
          value={name}
          maxLength={NAME_MAX}
          autoFocus
          onChange={(e) => {
            setName(normalize ? normalize(e.target.value) : e.target.value);
            s.setError(null);
          }}
        />
      </div>
      <ErrorLine error={s.error} />
    </Modal>
  );
}

export function CreateServerDialog() {
  const me = useStore((s) => s.me);
  return (
    <NameDialog
      title="Create server"
      label="Server name"
      initial={me ? `${me.name}'s server` : ""}
      submitLabel="Create server"
      action={createServer}
    />
  );
}

export function JoinServerDialog() {
  const [code, setCode] = useState("");
  const s = useSubmit(() => joinServer(code.trim()), closeDialog);
  return (
    <Modal
      title="Join a server"
      onClose={closeDialog}
      onSubmit={s.submit}
      footer={<Footer busy={s.busy} label={s.busy ? "Joining" : "Join server"} disabled={!code.trim()} />}
    >
      <div className="field">
        <label className="field-label" htmlFor="invite-input">
          Invite code
        </label>
        <textarea
          id="invite-input"
          className="textarea mono"
          value={code}
          placeholder="tincan1..."
          autoFocus
          spellCheck={false}
          onChange={(e) => {
            setCode(e.target.value);
            s.setError(null);
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              s.submit();
            }
          }}
        />
      </div>
      <ErrorLine error={s.error} />
    </Modal>
  );
}

export function InviteDialog({ serverId }: { serverId: ServerId }) {
  const server = useStore((s) => s.servers.find((x) => x.id === serverId));
  const [code, setCode] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    let live = true;
    setError(null);
    invoke("create_invite", { serverId }).then(
      (c) => live && setCode(c),
      (err: unknown) => live && setError(errorText(err)),
    );
    return () => {
      live = false;
    };
  }, [serverId, attempt]);

  useEffect(() => {
    if (!copied) return;
    const t = setTimeout(() => setCopied(false), 2000);
    return () => clearTimeout(t);
  }, [copied]);

  return (
    <Modal
      title={`Invite people to ${server?.name ?? "this server"}`}
      onClose={closeDialog}
      footer={
        <>
          <button type="button" className="btn btn-ghost" onClick={closeDialog}>
            Done
          </button>
          {error ? (
            <button type="button" className="btn btn-primary" onClick={() => setAttempt((a) => a + 1)}>
              Try again
            </button>
          ) : (
            <button
              type="button"
              className="btn btn-primary"
              disabled={!code}
              onClick={() => code && void copyText(code).then(setCopied)}
            >
              {copied ? "Copied" : "Copy invite"}
            </button>
          )}
        </>
      }
    >
      <div className="field">
        <label className="field-label" htmlFor="invite-code">
          Invite code
        </label>
        <input
          id="invite-code"
          className="input mono"
          readOnly
          value={code ?? ""}
          placeholder={error ? "" : "Creating invite"}
          onFocus={(e) => e.currentTarget.select()}
        />
        <p className="field-note">Anyone with this invite can join for 7 days.</p>
      </div>
      <ErrorLine error={error} />
    </Modal>
  );
}

export function RenameServerDialog({ serverId }: { serverId: ServerId }) {
  const server = useStore((s) => s.servers.find((x) => x.id === serverId));
  return (
    <NameDialog
      title="Rename server"
      label="Server name"
      initial={server?.name ?? ""}
      submitLabel="Save"
      action={(name) => renameServer(serverId, name)}
    />
  );
}

function ConfirmDialog({
  title,
  body,
  label,
  action,
}: {
  title: string;
  body: string;
  label: string;
  action: () => Promise<void>;
}) {
  const s = useSubmit(action, closeDialog);
  return (
    <Modal title={title} onClose={closeDialog} onSubmit={s.submit} footer={<Footer busy={s.busy} label={label} danger />}>
      <p className="muted">{body}</p>
      <ErrorLine error={s.error} />
    </Modal>
  );
}

export function ConfirmLeaveServer({ serverId }: { serverId: ServerId }) {
  const server = useStore((s) => s.servers.find((x) => x.id === serverId));
  return (
    <ConfirmDialog
      title={`Leave ${server?.name ?? "this server"}?`}
      body="You can rejoin with a new invite."
      label="Leave server"
      action={() => leaveServer(serverId)}
    />
  );
}

/** Text channel names: lowercase, dashes for spaces. */
function textChannelName(v: string): string {
  return v
    .toLowerCase()
    .replace(/\s+/g, "-")
    .replace(/[^\p{L}\p{N}_-]/gu, "")
    .slice(0, NAME_MAX);
}

export function CreateChannelDialog({ serverId, initialKind }: { serverId: ServerId; initialKind: ChannelKind }) {
  const [kind, setKind] = useState<ChannelKind>(initialKind);
  const [name, setName] = useState("");
  const s = useSubmit(() => createChannel(serverId, kind === "text" ? textChannelName(name) : name.trim(), kind), closeDialog);
  return (
    <Modal
      title="Create channel"
      onClose={closeDialog}
      onSubmit={s.submit}
      footer={<Footer busy={s.busy} label="Create channel" disabled={!name.trim()} />}
    >
      <div className="field">
        <span className="field-label" id="kind-label">
          Type
        </span>
        <div className="segmented" role="radiogroup" aria-labelledby="kind-label">
          <button type="button" role="radio" aria-checked={kind === "text"} onClick={() => setKind("text")}>
            Text
          </button>
          <button type="button" role="radio" aria-checked={kind === "voice"} onClick={() => setKind("voice")}>
            Voice
          </button>
        </div>
      </div>
      <div className="field">
        <label className="field-label" htmlFor="channel-name">
          Name
        </label>
        <input
          id="channel-name"
          className="input"
          value={name}
          maxLength={NAME_MAX}
          placeholder={kind === "text" ? "new-channel" : "Hangout"}
          autoFocus
          onChange={(e) => {
            setName(kind === "text" ? textChannelName(e.target.value) : e.target.value);
            s.setError(null);
          }}
        />
      </div>
      <ErrorLine error={s.error} />
    </Modal>
  );
}

export function RenameChannelDialog({ serverId, channelId }: { serverId: ServerId; channelId: ChannelId }) {
  const channel = useStore((s) => s.channels[serverId]?.find((c) => c.id === channelId));
  const isText = channel?.kind !== "voice";
  return (
    <NameDialog
      title={isText ? "Rename channel" : "Rename voice room"}
      label="Name"
      initial={channel?.name ?? ""}
      submitLabel="Save"
      normalize={isText ? textChannelName : undefined}
      action={(name) => renameChannel(serverId, channelId, name)}
    />
  );
}

export function ConfirmDeleteChannel({ serverId, channelId }: { serverId: ServerId; channelId: ChannelId }) {
  const channel = useStore((s) => s.channels[serverId]?.find((c) => c.id === channelId));
  const name = channel ? (channel.kind === "text" ? `#${channel.name}` : channel.name) : "this channel";
  return (
    <ConfirmDialog
      title={`Delete ${name}?`}
      body={channel?.kind === "voice" ? "The room is removed for everyone." : "The channel is removed for everyone."}
      label="Delete channel"
      action={() => deleteChannel(serverId, channelId)}
    />
  );
}

export function ConfirmDeleteMessage({ message }: { message: Message }) {
  return (
    <ConfirmDialog
      title="Delete message?"
      body={message.text ? `"${message.text.length > 120 ? `${message.text.slice(0, 120)}...` : message.text}"` : "The message and its files are removed for everyone."}
      label="Delete"
      action={() => deleteMessage(message)}
    />
  );
}

export function AddContactDialog() {
  const me = useStore((s) => s.me);
  const [code, setCode] = useState("");
  const [copied, setCopied] = useState(false);
  const s = useSubmit(() => addContact(code.trim()), closeDialog);

  useEffect(() => {
    if (!copied) return;
    const t = setTimeout(() => setCopied(false), 2000);
    return () => clearTimeout(t);
  }, [copied]);

  return (
    <Modal
      title="Add contact"
      onClose={closeDialog}
      onSubmit={s.submit}
      footer={
        <>
          {me && (
            <button type="button" className="btn btn-ghost foot-start" onClick={() => void copyText(me.userCode).then(setCopied)}>
              {copied ? "Copied" : "Copy my ID"}
            </button>
          )}
          <Footer busy={s.busy} label="Add contact" disabled={!code.trim()} />
        </>
      }
    >
      <div className="field">
        <label className="field-label" htmlFor="contact-code">
          Their Tincan ID
        </label>
        <input
          id="contact-code"
          className="input mono"
          value={code}
          placeholder="tincan1u..."
          autoFocus
          spellCheck={false}
          onChange={(e) => {
            setCode(e.target.value);
            s.setError(null);
          }}
        />
      </div>
      <ErrorLine error={s.error} />
    </Modal>
  );
}
