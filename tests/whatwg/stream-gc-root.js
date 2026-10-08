// Regression: a ReadableStream chunk dequeued by read() must survive a GC that
// runs inside the `pull` callback. At that moment the chunk is no longer in the
// (GC-traced) controller queue and is only a native stack local, so without an
// explicit root a GC sweeps it and binding it into the iterator result hits a
// freed node (`gc-lite` "bad node" panic).
//
// Run: aike -m tests/whatwg/stream-gc-root.js

let i = 0;
const rs = new ReadableStream({
  start(c) {
    c.enqueue(new Uint8Array([1]));
  },
  pull(c) {
    if (i >= 3) {
      c.close();
      return;
    }
    ++i;
    Aike.gc();
    c.enqueue(new Uint8Array([2]));
  },
});

const reader = rs.getReader();
let done = false;
let reads = 0;
while (!done && reads < 20) {
  const r = await reader.read();
  if (r.value !== undefined && !(r.value instanceof Uint8Array)) {
    throw new Error(`chunk is not a Uint8Array: ${r.value}`);
  }
  done = r.done === true;
  reads += 1;
}
if (!done) {
  throw new Error("stream never closed");
}
console.log(`PASS stream-gc-root (${reads} reads)`);
