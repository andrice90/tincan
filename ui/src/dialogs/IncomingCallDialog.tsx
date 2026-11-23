// Shown while someone is ringing me.

import type { CallInfo } from "../api/types";
import { Avatar, IdSuffix } from "../components/common";
import { Modal } from "../components/Modal";
import { acceptCall, declineCall } from "../store/actions";

export function IncomingCallDialog({ call }: { call: CallInfo }) {
  return (
    <Modal
      title={
        <>
          <bdi>{call.name}</bdi> is calling
        </>
      }
      // Escape or a stray click outside must not decline: only the Decline button does
      onClose={() => {}}
      className="modal-call"
      footer={
        <>
          <button type="button" className="btn btn-danger" onClick={() => declineCall(call.callId)}>
            Decline
          </button>
          <button type="button" className="btn btn-primary" data-autofocus onClick={() => acceptCall(call.callId)}>
            Accept
          </button>
        </>
      }
    >
      <div className="call-ringing">
        <Avatar userId={call.userId} name={call.name} size={64} />
        <IdSuffix userId={call.userId} name={call.name} />
      </div>
    </Modal>
  );
}
