// Renders the open dialog from the store, plus the incoming call prompt.

import { useStore } from "../store/store";
import { IncomingCallDialog } from "./IncomingCallDialog";
import {
  AddContactDialog,
  ConfirmDeleteChannel,
  ConfirmDeleteMessage,
  ConfirmLeaveServer,
  CreateChannelDialog,
  CreateServerDialog,
  InviteDialog,
  JoinServerDialog,
  RenameChannelDialog,
  RenameServerDialog,
} from "./ServerDialogs";
import { ScreenPickerDialog } from "./ScreenPickerDialog";
import { SettingsDialog } from "./SettingsDialog";
import { AddSoundDialog, RenameSoundDialog } from "./SoundDialogs";

export function Dialogs() {
  const d = useStore((s) => s.dialog);
  const ringing = useStore((s) => (s.call?.state === "ringing-in" ? s.call : null));

  let node = null;
  if (d) {
    switch (d.kind) {
      case "create-server":
        node = <CreateServerDialog />;
        break;
      case "join-server":
        node = <JoinServerDialog />;
        break;
      case "invite":
        node = <InviteDialog serverId={d.serverId} />;
        break;
      case "rename-server":
        node = <RenameServerDialog serverId={d.serverId} />;
        break;
      case "leave-server":
        node = <ConfirmLeaveServer serverId={d.serverId} />;
        break;
      case "create-channel":
        node = <CreateChannelDialog serverId={d.serverId} initialKind={d.channelKind} />;
        break;
      case "rename-channel":
        node = <RenameChannelDialog serverId={d.serverId} channelId={d.channelId} />;
        break;
      case "delete-channel":
        node = <ConfirmDeleteChannel serverId={d.serverId} channelId={d.channelId} />;
        break;
      case "delete-message":
        node = <ConfirmDeleteMessage message={d.message} />;
        break;
      case "settings":
        node = <SettingsDialog initialTab={d.tab} />;
        break;
      case "screen-picker":
        node = <ScreenPickerDialog />;
        break;
      case "add-contact":
        node = <AddContactDialog />;
        break;
      case "add-sound":
        node = <AddSoundDialog path={d.path} />;
        break;
      case "rename-sound":
        node = <RenameSoundDialog clip={d.clip} />;
        break;
    }
  }

  return (
    <>
      {node}
      {ringing && <IncomingCallDialog call={ringing} />}
    </>
  );
}
