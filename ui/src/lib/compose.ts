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

/** The composer showing each conversation key now: it takes back the text of a failed send. */
const live = new Map<string, (sent: Draft) => void>();

/**
 * A composer for `key` is on screen and takes failed sends with `restore`. Returns the
 * function to call when it goes away.
 */
export function registerComposer(key: string, restore: (sent: Draft) => void): () => void {
  live.set(key, restore);
  return () => {
    if (live.get(key) === restore) live.delete(key);
  };
}

/**
 * A send from a composer that is gone failed: hand its text and files to the composer that
 * shows `key` now, or keep the text as the draft of `key` (in front of anything typed since)
 * for the next composer that mounts.
 */
export function restoreFailed(key: string, sent: Draft, drafts: Map<string, string>): void {
  const restore = live.get(key);
  if (restore) {
    restore(sent);
    return;
  }
  const text = finishSend({ text: drafts.get(key) ?? "", files: [] }, sent, false).text;
  if (text) drafts.set(key, text);
}
