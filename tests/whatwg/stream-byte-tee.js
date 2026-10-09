// Regression: `ReadableStream.tee()` must deliver every chunk to BOTH branches.
//
// A byte stream's chunks are transferred (detached) by the receiving
// controller's `enqueue`, so the tee has to hand each branch its own buffer —
// sharing one view left the second branch empty and its `read()` never settled.
// The second branch of a default stream must also receive a structured clone,
// not the same object.
//
// Run: aike -m tests/whatwg/stream-byte-tee.js

function withTimeout(promise, label) {
  return Promise.race([
    promise,
    new Promise((_, reject) => setTimeout(() => reject(new Error(`${label} timed out`)), 5000)),
  ]);
}

/// Read one chunk from each branch; both must see the same bytes.
async function teeBoth(label, stream, expected) {
  const [a, b] = stream.tee();
  const ra = a.getReader();
  const rb = b.getReader();
  const gotA = await withTimeout(ra.read(), `${label} branch1`);
  const gotB = await withTimeout(rb.read(), `${label} branch2`);
  const bytes = (r) => (r.value ? [...r.value].join(",") : null);
  if (gotA.done || gotB.done) {
    throw new Error(`${label}: branch reported done before the chunk`);
  }
  if (bytes(gotA) !== expected || bytes(gotB) !== expected) {
    throw new Error(
      `${label}: expected ${expected} on both branches, got ${bytes(gotA)} / ${bytes(gotB)}`,
    );
  }
  // The two branches must not share one buffer object.
  if (gotA.value.buffer === gotB.value.buffer) {
    throw new Error(`${label}: both branches share the same buffer`);
  }
}

// Byte stream that enqueues in `start` (chunk already queued when read).
await teeBoth(
  "byte/start-enqueue",
  new ReadableStream({
    type: "bytes",
    start(c) {
      c.enqueue(new Uint8Array([1, 2]));
      c.close();
    },
  }),
  "1,2",
);

// Byte stream fed from `pull` (read happens before any chunk exists).
await teeBoth(
  "byte/pull-driven",
  new ReadableStream({
    type: "bytes",
    pull(c) {
      c.enqueue(new Uint8Array([3, 4]));
      c.close();
    },
  }),
  "3,4",
);

// Default (non-byte) stream: branch2 gets a structured clone.
await teeBoth(
  "default",
  new ReadableStream({
    start(c) {
      c.enqueue(new Uint8Array([5, 6]));
      c.close();
    },
  }),
  "5,6",
);

console.log("PASS stream-byte-tee");