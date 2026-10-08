// Regression: a single enqueue that fully fills two BYOB pull-into descriptors
// resolves them one after another. The first resolve checks the result for a
// thenable, and the patched `then` getter forces a GC. The second descriptor
// has left the pending list by then, so it must stay traced through the GC.
//
// Run: aike -m tests/whatwg/stream-byob-gc-root.js

Object.defineProperty(Object.prototype, "then", {
  configurable: true,
  get() {
    Aike.gc();
    return undefined;
  },
});

let ctrl;
const rs = new ReadableStream({
  type: "bytes",
  start(c) {
    ctrl = c;
  },
  pull() {},
});
const reader = rs.getReader({ mode: "byob" });
const p1 = reader.read(new Uint8Array(2));
const p2 = reader.read(new Uint8Array(2));
ctrl.enqueue(new Uint8Array([1, 2, 3, 4]));

const a = await p1;
const b = await p2;

delete Object.prototype.then;

const ok =
  a.value instanceof Uint8Array &&
  a.value[0] === 1 &&
  a.value[1] === 2 &&
  b.value instanceof Uint8Array &&
  b.value[0] === 3 &&
  b.value[1] === 4;
if (!ok) {
  throw new Error(`unexpected reads: ${a.value} ${b.value}`);
}
console.log("PASS stream-byob-gc-root");
