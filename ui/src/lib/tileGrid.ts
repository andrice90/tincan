// Voice tile grid: the column count follows the number of people, never the width, so a
// sliding panel or a dragged grab bar only scales the tiles (CSS, from the container size)
// and never rewraps or re-centres them in jumps.

/** Columns and rows for `n` tiles: the most square grid, wider than tall. */
export function tileGrid(n: number): { cols: number; rows: number } {
  const count = Math.max(1, Math.floor(n));
  const cols = Math.ceil(Math.sqrt(count));
  return { cols, rows: Math.ceil(count / cols) };
}
