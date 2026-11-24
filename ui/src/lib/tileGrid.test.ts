import { eq, test } from "../test/harness";
import { tileGrid } from "./tileGrid";

test("tile grid: columns follow the count, not the width", () => {
  eq(tileGrid(0), { cols: 1, rows: 1 });
  eq(tileGrid(1), { cols: 1, rows: 1 });
  eq(tileGrid(2), { cols: 2, rows: 1 });
  eq(tileGrid(3), { cols: 2, rows: 2 });
  eq(tileGrid(4), { cols: 2, rows: 2 });
  eq(tileGrid(5), { cols: 3, rows: 2 });
  eq(tileGrid(9), { cols: 3, rows: 3 });
  eq(tileGrid(10), { cols: 4, rows: 3 });
});
