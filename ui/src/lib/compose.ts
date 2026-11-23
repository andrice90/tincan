// What the composer box holds around a send. Pure, so it can be tested.

/** The text and files in the composer. */
export interface Draft {
  text: string;
  files: string[];
}

/**
 * The box when a send finishes. The box is emptied when the send starts, so whatever is in
 * it now was typed during the send and stays. A failed send puts its text back in front of
 * that, and its files back with any picked since.
 */
export function finishSend(box: Draft, sent: Draft, ok: boolean): Draft {
  if (ok) return box;
  const text = box.text === "" ? sent.text : sent.text === "" ? box.text : `${sent.text}\n${box.text}`;
  const files = [...sent.files, ...box.files.filter((f) => !sent.files.includes(f))];
  return { text, files };
}
