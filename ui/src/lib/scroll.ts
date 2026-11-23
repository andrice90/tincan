// Scroll anchoring for the message history. Pure, so it can be tested.

/**
 * What the history does with the anchor captured before loading older messages, after its
 * rows changed. The anchor is only applied (and used up) when the first row changed, which
 * is when the older page was put in front. Any other change (a new message at the bottom, an
 * edit) re-captures it, so the prepend that follows still keeps the reading position.
 */
export function anchorStep(anchor: number | null, firstKey: string | undefined, prevFirst: string | undefined): "apply" | "recapture" | "none" {
  if (anchor === null) return "none";
  return firstKey === prevFirst ? "recapture" : "apply";
}
