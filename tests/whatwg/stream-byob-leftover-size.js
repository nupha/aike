// Regression: when an enqueue partly fills a BYOB pull-into, the leftover bytes
// are pushed back to the queue and must count toward the queue total, so
// desiredSize drops below the high-water mark. Also checks that draining the
// leftover restores it.
//
// Run: aike -m tests/whatwg/stream-byob-leftover-size.js

let ctrl;
const rs = new ReadableStream(
  {
    type: "bytes",
    start(c) {
      ctrl = c;
    },
    pull() {},
  },
  { highWaterMark: 5 },
);
const reader = rs.getReader({ mode: "byob" });
const p1 = reader.read(new Uint8Array(2));

ctrl.enqueue(new Uint8Array([1, 2, 3, 4]));
if (!(ctrl.desiredSize < 5)) {
  throw new Error(`leftover not counted: desiredSize=${ctrl.desiredSize}`);
}

const a = await p1;
if (a.value[0] !== 1 || a.value[1] !== 2) {
  throw new Error("first read has wrong bytes");
}
const b = await reader.read(new Uint8Array(2));
if (b.value[0] !== 3 || b.value[1] !== 4) {
  throw new Error("leftover read has wrong bytes");
}
if (ctrl.desiredSize !== 5) {
  throw new Error(`queue total not restored: desiredSize=${ctrl.desiredSize}`);
}
console.log("PASS stream-byob-leftover-size");
