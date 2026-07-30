// 🌐 WHATWG Streams API 标准测试 — ReadableStream / WritableStream / TransformStream
//
// 运行：aike -m tests/whatwg/stream.js

let pass = 0;
let fail = 0;
let skip = 0;

function check(label, ok, detail) {
  if (ok) {
    console.log(`  ✅ ${label}`);
    pass++;
  } else {
    console.log(`  ❌ ${label}${detail ? " — " + detail : ""}`);
    fail++;
  }
}

function skipCheck(label, reason) {
  console.log(`  ⏭️  ${label}${reason ? " — " + reason : ""}`);
  skip++;
}

// ===================================================================
// 1. ReadableStream — 基本构造和状态
// ===================================================================
function testReadableStreamBasic() {
  console.log("\n=== ReadableStream (basic) ===");

  // 空构造
  const rs = new ReadableStream();
  check("ReadableStream locked (empty)", rs.locked === false);

  // 带 start 的构造
  let started = false;
  const rs2 = new ReadableStream({
    start(controller) {
      started = true;
      check(
        "start() receives controller",
        typeof controller.enqueue === "function",
      );
      check(
        "start() controller has close",
        typeof controller.close === "function",
      );
      check(
        "start() controller has error",
        typeof controller.error === "function",
      );
      check(
        "start() controller desiredSize",
        typeof controller.desiredSize === "number",
      );
    },
  });
  check("ReadableStream start() called", started);

  // locked 状态 — getReader 后
  const rs3 = new ReadableStream();
  check("locked before getReader", rs3.locked === false);
  const reader = rs3.getReader();
  check("locked after getReader", rs3.locked === true);
  reader.releaseLock();
  check("locked after releaseLock", rs3.locked === false);
}

// ===================================================================
// 2. ReadableStream — getReader().read()
// ===================================================================
function testReadableStreamRead() {
  console.log("\n=== ReadableStream (read) ===");

  // 手动 enqueue 然后 read
  let controllerRef = null;
  const rs = new ReadableStream({
    start(c) {
      controllerRef = c;
    },
  });
  const reader = rs.getReader();

  // 先 read（会 pending），再 enqueue
  const readPromise = reader.read();
  let resolved = false;
  readPromise.then((result) => {
    resolved = true;
    check(
      "read() result.done === false",
      result.done === false,
      `done=${result.done}`,
    );
    check(
      "read() result.value === 42",
      result.value === 42,
      `value=${result.value}`,
    );
  });

  controllerRef.enqueue(42);

  // 再读第二个 — 有排队数据的 read 应直接 resolve
  return readPromise
    .then(() => {
      // 此时第二个 read 应该触发 pending → enqueue → resolve
      let p2 = reader.read();
      controllerRef.enqueue(99);
      return p2.then((r2) => {
        check("2nd read() value === 99", r2.value === 99);
      });
    })
    .then(() => {
      // close 后 read 应得到 done=true
      controllerRef.close();
      return reader.read().then((closeResult) => {
        check("read() after close done=true", closeResult.done === true);
        check(
          "read() after close value=undefined",
          closeResult.value === undefined,
        );
      });
    });
}

// ===================================================================
// 3. ReadableStream — cancel
// ===================================================================
function testReadableStreamCancel() {
  console.log("\n=== ReadableStream (cancel) ===");

  let cancelCalled = false;
  const rs = new ReadableStream({
    cancel(reason) {
      cancelCalled = true;
      check("cancel() reason passed", reason === "bye");
    },
  });

  return rs.cancel("bye").then(() => {
    check("cancel() promise resolves", true);
    check("cancel() callback called", cancelCalled);
  });
}

// ===================================================================
// 4. ReadableStream — async iterator (for await...of)
// ===================================================================
function testReadableStreamAsyncIterator() {
  console.log("\n=== ReadableStream (async iterator) ===");

  let controllerRef = null;
  const rs = new ReadableStream({
    start(c) {
      controllerRef = c;
    },
  });

  // 异步迭代
  const results = [];
  const iterPromise = (async () => {
    for await (const chunk of rs) {
      results.push(chunk);
      if (results.length >= 3) break;
    }
    check(
      "async iterator yielded 3 values",
      results.length === 3,
      `got ${JSON.stringify(results)}`,
    );
    check(
      "async iterator values match",
      results[0] === 10 && results[1] === 20 && results[2] === 30,
    );
  })();

  controllerRef.enqueue(10);
  controllerRef.enqueue(20);
  controllerRef.enqueue(30);

  return iterPromise;
}

// ===================================================================
// 5. ReadableStream — 二次 getReader 错误
// ===================================================================
function testReadableStreamLockError() {
  console.log("\n=== ReadableStream (lock error) ===");

  const rs = new ReadableStream();
  const r1 = rs.getReader();
  let threw = false;
  try {
    rs.getReader();
  } catch (e) {
    threw = true;
  }
  check("2nd getReader throws", threw);
  r1.releaseLock();
  const r2 = rs.getReader();
  check("getReader after releaseLock works", r2 !== undefined && r2 !== null);
  r2.releaseLock();
}

// ===================================================================
// 6. WritableStream — 基本构造和操作
// ===================================================================
function testWritableStream() {
  console.log("\n=== WritableStream ===");

  let writeChunks = [];
  let closeCalled = false;
  let startCalled = false;

  const ws = new WritableStream({
    start(controller) {
      startCalled = true;
      check(
        "WritableStream start() controller has error",
        typeof controller.error === "function",
      );
    },
    write(chunk, controller) {
      writeChunks.push(chunk);
    },
    close() {
      closeCalled = true;
    },
  });

  check("start() called", startCalled);
  check("locked before getWriter", ws.locked === false);
  const writer = ws.getWriter();
  check("locked after getWriter", ws.locked === true);

  // write
  return writer
    .write("hello")
    .then(() => {
      check("write() resolves", true);
      return writer.write("world");
    })
    .then(() => {
      check(
        "wrote 2 chunks",
        writeChunks.length === 2,
        `got ${JSON.stringify(writeChunks)}`,
      );
      check("chunk[0] === hello", writeChunks[0] === "hello");
      check("chunk[1] === world", writeChunks[1] === "world");
    })
    .then(() => {
      // close
      return writer.close().then(() => {
        check("close() called", closeCalled);
        check("close() resolves", true);
      });
    });
}

// ===================================================================
// 7. WritableStream — abort
// ===================================================================
function testWritableStreamAbort() {
  console.log("\n=== WritableStream (abort) ===");

  let abortCalled = false;
  const ws = new WritableStream({
    abort(reason) {
      abortCalled = true;
      check("abort reason passed", reason === "oops");
    },
  });

  return ws.abort("oops").then(() => {
    check("abort resolves", true);
    check("abort callback called", abortCalled);
  });
}

// ===================================================================
// 8. WritableStream — writer releaseLock
// ===================================================================
function testWritableStreamReleaseLock() {
  console.log("\n=== WritableStream (releaseLock) ===");

  const ws = new WritableStream();
  const w = ws.getWriter();
  check("locked after getWriter", ws.locked === true);
  w.releaseLock();
  check("locked after releaseLock", ws.locked === false);

  const w2 = ws.getWriter();
  check("getWriter after releaseLock works", true);
  w2.releaseLock();
}

// ===================================================================
// 9. TransformStream — 构造和属性
// ===================================================================
function testTransformStream() {
  console.log("\n=== TransformStream ===");

  const ts = new TransformStream();
  check(
    "TransformStream.readable is ReadableStream",
    ts.readable instanceof ReadableStream,
  );
  check(
    "TransformStream.writable is WritableStream",
    ts.writable instanceof WritableStream,
  );
  check("TransformStream readable locked", ts.readable.locked === false);
  check("TransformStream writable locked", ts.writable.locked === false);

  // 由于 TransformStreamDefaultController 方法暂未实现（stub），跳过 transform 测试
  skipCheck(
    "TransformStream transform",
    "controller stubs — not yet implemented",
  );
}

// ===================================================================
// 10. ByteLengthQueuingStrategy
// ===================================================================
function testByteLengthQueuingStrategy() {
  console.log("\n=== ByteLengthQueuingStrategy ===");

  const s = new ByteLengthQueuingStrategy({ highWaterMark: 1024 });
  check(
    "ByteLengthQS highWaterMark",
    s.highWaterMark === 1024,
    `got ${s.highWaterMark}`,
  );

  const buf = new Uint8Array(10);
  check("ByteLengthQS size()", s.size(buf) === 10, `got ${s.size(buf)}`);
}

// ===================================================================
// 11. CountQueuingStrategy
// ===================================================================
function testCountQueuingStrategy() {
  console.log("\n=== CountQueuingStrategy ===");

  const s = new CountQueuingStrategy({ highWaterMark: 5 });
  check(
    "CountQS highWaterMark",
    s.highWaterMark === 5,
    `got ${s.highWaterMark}`,
  );
  check("CountQS size() === 1", s.size() === 1, `got ${s.size()}`);
}

// ===================================================================
// 入口
// ===================================================================
testReadableStreamBasic();
testReadableStreamLockError();

testReadableStreamRead()
  .then(() => testReadableStreamCancel())
  .then(() => testReadableStreamAsyncIterator())
  .then(() => testWritableStream())
  .then(() => testWritableStreamAbort())
  .then(() => testWritableStreamReleaseLock())
  .then(() => testTransformStream())
  .then(() => testByteLengthQueuingStrategy())
  .then(() => testCountQueuingStrategy())
  .then(() => {
    console.log(`\n=== SUMMARY: pass=${pass} fail=${fail} skip=${skip} ===`);
    console.log(fail > 0 ? "RESULT: FAIL" : "RESULT: PASS");
  })
  .catch((e) => {
    console.log("UNEXPECTED ERROR:", e);
    console.log("RESULT: FAIL");
  });
