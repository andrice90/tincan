// Dialogs for servers, channels, messages and contacts.

import { useEffect, useRef, useState, type ReactNode } from "react";
import { errorText, invoke } from "../api/client";
import type { ChannelId, ChannelKind, Message, ServerId, UserId } from "../api/types";
import {
  addContact,
  closeDialog,
  createChannel,
  createServer,
  deleteChannel,
  deleteMessage,
  joinServer,
  kickMember,
  leaveServer,
  renameChannel,
  renameServer,
} from "../store/actions";
import { useStore } from "../store/store";
import { CopyButton } from "../components/CopyButton";
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
          placeholder="ABC-DEF"
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
        <p className="field-note">The 6-character code, or a full invite.</p>
      </div>
      <ErrorLine error={s.error} />
    </Modal>
  );
}

export function InviteDialog({ serverId }: { serverId: ServerId }) {
  const server = useStore((s) => s.servers.find((x) => x.id === serverId));
  const isOwner = server?.isOwner ?? false;
  /** undefined while loading; null when the owner hasn't made a code yet. */
  const [code, setCode] = useState<string | null | undefined>(undefined);
  const [error, setError] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);
  const [confirming, setConfirming] = useState(false);
  const [rotating, setRotating] = useState(false);
  const live = useRef(true);

  useEffect(() => {
    live.current = true;
    return () => {
      live.current = false;
    };
  }, []);

  useEffect(() => {
    let current = true;
    setError(null);
    invoke("get_invite_code", { serverId }).then(
      (c) => current && setCode(c),
      (err: unknown) => current && setError(errorText(err)),
    );
    return () => {
      current = false;
    };
  }, [serverId, attempt]);

  const rotate = async () => {
    setRotating(true);
    setError(null);
    try {
      const c = await invoke("rotate_invite_code", { serverId });
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

  const fullInvite = () => invoke("create_invite", { serverId });
  const noCode = code === null;

  let footer: ReactNode;
  if (confirming) {
    footer = (
      <>
        <button type="button" className="btn btn-ghost" disabled={rotating} onClick={() => setConfirming(false)}>
          Cancel
        </button>
        <button type="button" className="btn btn-danger" disabled={rotating} onClick={() => void rotate()}>
          {rotating ? "Rotating" : "Rotate code"}
        </button>
      </>
    );
  } else if (noCode) {
    footer = (
      <>
        <button type="button" className="btn btn-ghost" onClick={closeDialog}>
          Done
        </button>
        <CopyButton className="btn btn-primary" text={fullInvite} label="Copy full invite" onError={setError} />
      </>
    );
  } else {
    footer = (
      <>
        <CopyButton className="btn btn-ghost foot-start" text={fullInvite} label="Copy full invite" onError={setError} />
        {isOwner && code && (
          <button type="button" className="btn" onClick={() => setConfirming(true)}>
            Rotate code
          </button>
        )}
        {error && code === undefined ? (
          <button type="button" className="btn btn-primary" onClick={() => setAttempt((a) => a + 1)}>
            Try again
          </button>
        ) : (
          <button type="button" className="btn btn-primary" onClick={closeDialog}>
            Done
          </button>
        )}
      </>
    );
  }

  return (
    <Modal
      title={
        <>
          Invite people to <bdi>{server?.name ?? "this server"}</bdi>
        </>
      }
      onClose={closeDialog}
      footer={footer}
    >
      {noCode ? (
        <p className="muted">The owner hasn't made an invite code yet. Send the full invite instead. It works for 7 days.</p>
      ) : (
        <div className="field">
          <label className="field-label" htmlFor="invite-code">
            Invite code
          </label>
          <div className="input-row">
            <input
              id="invite-code"
              className="input mono"
              readOnly
              value={code ?? ""}
              placeholder={error ? "" : "Making a code"}
              onFocus={(e) => e.currentTarget.select()}
            />
            <CopyButton text={code ?? null} label="Copy" />
          </div>
          <p className="field-note">
            {confirming
              ? "The current code stops working for new joins. People already in the server stay."
              : isOwner
                ? "Works until you rotate it."
                : "Works until the owner rotates it."}
          </p>
        </div>
      )}
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
  title: ReactNode;
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
      title={
        <>
          Leave <bdi>{server?.name ?? "this server"}</bdi>?
        </>
      }
      body="You can rejoin with a new invite."
      label="Leave server"
      action={() => leaveServer(serverId)}
    />
  );
}

export function ConfirmKickMember({ serverId, userId, name }: { serverId: ServerId; userId: UserId; name: string }) {
  const server = useStore((s) => s.servers.find((x) => x.id === serverId));
  return (
    <ConfirmDialog
      title={
        <>
          Remove <bdi>{name}</bdi> from <bdi>{server?.name ?? "this server"}</bdi>?
        </>
      }
      body="They're disconnected right away. They can rejoin with any invite code that still works."
      label="Remove"
      action={() => kickMember(serverId, userId)}
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
      title={
        <>
          Delete <bdi>{name}</bdi>?
        </>
      }
      body={channel?.kind === "voice" ? "The room is removed for everyone." : "The channel is removed for everyone."}
      label={channel?.kind === "voice" ? "Delete voice room" : "Delete channel"}
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
  const [code, setCode] = useState("");
  const [myCode, setMyCode] = useState<string | null>(null);
  const s = useSubmit(() => addContact(code.trim()), closeDialog);
  const setError = s.setError;

  useEffect(() => {
    let live = true;
    invoke("get_friend_code", {}).then(
      (c) => live && setMyCode(c),
      (err: unknown) => live && setError(errorText(err)),
    );
    return () => {
      live = false;
    };
  }, [setError]);

  return (
    <Modal
      title="Add contact"
      onClose={closeDialog}
      onSubmit={s.submit}
      footer={
        <>
          <CopyButton className="btn btn-ghost foot-start" text={myCode} label="Copy my friend code" />
          <Footer busy={s.busy} label="Add contact" disabled={!code.trim()} />
        </>
      }
    >
      <div className="field">
        <label className="field-label" htmlFor="contact-code">
          Their friend code
        </label>
        <input
          id="contact-code"
          className="input mono"
          value={code}
          placeholder="ABC-DEF"
          autoFocus
          spellCheck={false}
          onChange={(e) => {
            setCode(e.target.value);
            s.setError(null);
          }}
        />
        <p className="field-note">A full Tincan ID works too.</p>
      </div>
      <ErrorLine error={s.error} />
    </Modal>
  );
}
