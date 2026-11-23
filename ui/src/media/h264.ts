// H.264 Annex-B helpers for the screen share decoder.

/** Calls `fn` with each NAL unit payload (without its start code). Stops when `fn` returns true. */
export function forEachNal(au: Uint8Array, fn: (nal: Uint8Array) => boolean | void): void {
  const n = au.length;
  let start = -1;
  let i = 0;
  while (i + 2 < n) {
    if (au[i] === 0 && au[i + 1] === 0 && au[i + 2] === 1) {
      if (start >= 0) {
        let end = i;
        // a 4-byte start code leaves a zero on the previous NAL
        while (end > start && au[end - 1] === 0) end--;
        if (fn(au.subarray(start, end))) return;
      }
      i += 3;
      start = i;
    } else {
      i++;
    }
  }
  if (start >= 0 && start < n) fn(au.subarray(start, n));
}

const hex2 = (b: number) => b.toString(16).padStart(2, "0");

/** "avc1.PPCCLL" from the first SPS (NAL type 7) in an access unit, or null if there is none. */
export function codecFromAnnexB(au: Uint8Array): string | null {
  let codec: string | null = null;
  forEachNal(au, (nal) => {
    if (nal.length >= 4 && (nal[0] & 0x1f) === 7) {
      codec = `avc1.${hex2(nal[1])}${hex2(nal[2])}${hex2(nal[3])}`;
      return true;
    }
    return false;
  });
  return codec;
}
