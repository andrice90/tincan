import { eq, ok, test } from "../test/harness";
import { closedPresence, initialPresence, presenceDone, presenceStep, type Presence } from "./presence";

const D = 260;

test("presence: starts open on a value, closed on null", () => {
  eq(initialPresence("a"), { phase: "open", kept: "a", gen: 0 });
  eq(initialPresence(null), { phase: "closed", kept: null, gen: 0 });
});

test("presence: opens on a value and follows it while open", () => {
  let p: Presence<string> = closedPresence();
  p = presenceStep(p, "a", D);
  eq(p.phase, "open");
  eq(p.kept, "a");
  const same = presenceStep(p, "a", D);
  ok(same === p, "an unchanged value returns the same presence");
  p = presenceStep(p, "b", D);
  eq(p.kept, "b", "a new value replaces the kept one at once");
});

test("presence: closing keeps the last value until its timer runs out", () => {
  let p = presenceStep(closedPresence<string>(), "a", D);
  p = presenceStep(p, null, D);
  eq(p.phase, "closing");
  eq(p.kept, "a", "content stays mounted for the close transition");
  ok(presenceStep(p, null, D) === p, "staying null changes nothing");
  p = presenceDone(p, p.gen);
  eq(p, { phase: "closed", kept: null, gen: 1 });
});

test("presence: no delay closes at once (Classic, reduced motion)", () => {
  let p = presenceStep(closedPresence<string>(), "a", 0);
  p = presenceStep(p, null, 0);
  eq(p.phase, "closed");
  eq(p.kept, null);
  // motion turned off while a close was running
  let q = presenceStep(presenceStep(closedPresence<string>(), "a", D), null, D);
  q = presenceStep(q, null, 0);
  eq(q.phase, "closed");
});

test("presence: reopening during a close cancels it, and the old timer does nothing", () => {
  let p = presenceStep(closedPresence<string>(), "a", D);
  p = presenceStep(p, null, D);
  const firstClose = p.gen;
  p = presenceStep(p, "b", D);
  eq(p.phase, "open");
  eq(p.kept, "b");
  ok(presenceDone(p, firstClose) === p, "a timer that fires after reopening is ignored");
  // close again: only the new close's timer unmounts
  p = presenceStep(p, null, D);
  ok(p.gen !== firstClose, "each close has its own generation");
  ok(presenceDone(p, firstClose) === p, "the first close's timer can't end the second close");
  eq(presenceDone(p, p.gen).phase, "closed");
});

test("presence: a done timer on an open or closed panel is ignored", () => {
  const open = presenceStep(closedPresence<string>(), "a", D);
  ok(presenceDone(open, open.gen) === open, "open stays open");
  const closed = closedPresence<string>();
  ok(presenceDone(closed, 0) === closed, "closed stays closed");
});

test("presence: a custom sameness keeps the shown object", () => {
  const sameSig = (a: { sig: string }, b: { sig: string }) => a.sig === b.sig;
  const first = { sig: "x" };
  let p = presenceStep(closedPresence<{ sig: string }>(), first, D, sameSig);
  p = presenceStep(p, { sig: "x" }, D, sameSig);
  ok(p.kept === first, "an equal value built again does not re-render");
  p = presenceStep(p, { sig: "y" }, D, sameSig);
  eq(p.kept?.sig, "y");
});
