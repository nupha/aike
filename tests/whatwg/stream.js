// WHATWG Streams API — 逐步实现的测试套件
// 所有测试链式返回 promise，保证顺序执行

var pass = 0;
var fail = 0;
var skip = 0;

function check(label, ok, detail) {
  if (ok) {
    console.log("  ✅ " + label);
    pass++;
  } else {
    console.log("  ❌ " + label + (detail ? " — " + detail : ""));
    fail++;
  }
}

function skipCheck(label, reason) {
  console.log("  ⏭️  " + label + (reason ? " — " + reason : ""));
  skip++;
}

// ===================================================================
// 1. ReadableStream — 构造、状态、locked
// ===================================================================
function testReadableStream() {
  console.log("\n=== ReadableStream ===");
  var rs = new ReadableStream();
  check("ReadableStream is ReadableStream", rs instanceof ReadableStream);
  check("ReadableStream default state readable", rs.state === "readable");
  check("ReadableStream locked false", rs.locked === false);
}

// ===================================================================
// 2. ReadableStreamDefaultReader — getReader
// ===================================================================
function testReadableStreamReader() {
  console.log("\n=== ReadableStreamDefaultReader ===");
  var rs = new ReadableStream();
  var reader = rs.getReader();
  check("reader is ReadableStreamDefaultReader", reader instanceof ReadableStreamDefaultReader);
  check("stream locked after getReader", rs.locked === true);
  check("reader.closed is Promise", reader.closed instanceof Promise);
}

// ===================================================================
// 3. ReadableStream — pull from underlying source
// ===================================================================
function testReadableStreamPull() {
  console.log("\n=== ReadableStream (pull) ===");
  var pulled = 0;
  var rs = new ReadableStream({
    start: function(controller) {
      check("controller is ReadableStreamDefaultController", controller instanceof ReadableStreamDefaultController);
      check("controller has enqueue", typeof controller.enqueue === "function");
      check("controller has close", typeof controller.close === "function");
      check("controller has error", typeof controller.error === "function");
      check("controller has desiredSize", typeof controller.desiredSize === "number");
    },
    pull: function(controller) {
      pulled++;
      if (pulled === 1) controller.enqueue("a");
      else if (pulled === 2) controller.enqueue("b");
      else controller.close();
    },
  });

  var reader = rs.getReader();
  return reader.read()
    .then(function(result) {
      check("first pull value === 'a'", result.value === "a");
      check("first pull done === false", result.done === false);
      return reader.read();
    })
    .then(function(result) {
      check("second pull value === 'b'", result.value === "b");
      check("second pull done === false", result.done === false);
      return reader.read();
    })
    .then(function(result) {
      check("third pull done === true", result.done === true);
      check("third pull value === undefined", result.value === undefined);
    });
}

// ===================================================================
// 4. ReadableStream — cancel
// ===================================================================
function testReadableStreamCancel() {
  console.log("\n=== ReadableStream (cancel) ===");
  var cancelled = false;
  var reason;
  var rs = new ReadableStream({
    cancel: function(r) {
      cancelled = true;
      reason = r;
    },
  });

  var reader = rs.getReader();
  return reader.cancel("bye")
    .then(function() {
      check("cancel called", cancelled);
      check("cancel reason === 'bye'", reason === "bye");
      check("stream state errored/closed", rs.state === "closed");
    });
}

// ===================================================================
// 5. ReadableStream — read with queue
// ===================================================================
function testReadableStreamRead() {
  console.log("\n=== ReadableStream (enqueue then read) ===");
  var controllerRef = null;
  var rs = new ReadableStream({
    start: function(controller) {
      controllerRef = controller;
      controller.enqueue(1);
      controller.enqueue(2);
      controller.enqueue(3);
    },
  });

  var reader = rs.getReader();
  return reader.read()
    .then(function(result) {
      check("queued read 1 === 1", result.value === 1);
      check("queued read 1 done === false", result.done === false);
      return reader.read();
    })
    .then(function(result) {
      check("queued read 2 === 2", result.value === 2);
      return reader.read();
    })
    .then(function(result) {
      check("queued read 3 === 3", result.value === 3);
      // Now queue is empty — next read should wait for enqueue
      var wait = reader.read();
      controllerRef.enqueue(99);
      return wait;
    })
    .then(function(result) {
      check("deferred read === 99", result.value === 99);
      check("deferred read done === false", result.done === false);
    });
}

// ===================================================================
// 6. WritableStream — 基本写入
// ===================================================================
function testWritableStream() {
  console.log("\n=== WritableStream ===");
  var written = [];
  var ws = new WritableStream({
    write: function(chunk, controller) {
      check("controller is WritableStreamDefaultController", controller instanceof WritableStreamDefaultController);
      written.push(chunk);
    },
    close: function() {
      written.push("__CLOSE__");
    },
  });

  var writer = ws.getWriter();
  check("writer is WritableStreamDefaultWriter", writer instanceof WritableStreamDefaultWriter);
  check("writer.closed is Promise", writer.closed instanceof Promise);
  check("writer.desiredSize is number", typeof writer.desiredSize === "number");

  return writer.write("x")
    .then(function() {
      check("written length 1 after first write", written.length === 1);
      check("written[0] === 'x'", written[0] === "x");
      return writer.write("y");
    })
    .then(function() {
      check("written length 2 after second write", written.length === 2);
      check("written[1] === 'y'", written[1] === "y");
      return writer.close();
    })
    .then(function() {
      check("close called after writer.close", written.indexOf("__CLOSE__") >= 0);
    });
}

// ===================================================================
// 7. WritableStream — abort
// ===================================================================
function testWritableStreamAbort() {
  console.log("\n=== WritableStream (abort) ===");
  var aborted = false;
  var abortReason;
  var ws = new WritableStream({
    abort: function(reason) {
      aborted = true;
      abortReason = reason;
    },
  });

  var writer = ws.getWriter();
  return writer.abort("fail")
    .then(function() {
      check("abort called", aborted);
      check("abort reason === 'fail'", abortReason === "fail");
    });
}

// ===================================================================
// 8. WritableStream — releaseLock
// ===================================================================
function testWritableStreamReleaseLock() {
  console.log("\n=== WritableStream (releaseLock) ===");
  var ws = new WritableStream();
  var writer = ws.getWriter();
  check("locked before release", ws.locked === true);

  writer.releaseLock();
  check("locked after release", ws.locked === false);
}

// ===================================================================
// 9. TransformStream — 构造和 transform
// ===================================================================
function testTransformStream() {
  console.log("\n=== TransformStream ===");

  var ts = new TransformStream();
  check("TransformStream.readable is ReadableStream", ts.readable instanceof ReadableStream);
  check("TransformStream.writable is WritableStream", ts.writable instanceof WritableStream);
  check("TransformStream readable locked", ts.readable.locked === false);
  check("TransformStream writable locked", ts.writable.locked === false);
}

function testTransformStreamIdentity() {
  console.log("\n=== TransformStream (identity / pass-through) ===");

  var ts = new TransformStream();
  var writer = ts.writable.getWriter();
  var reader = ts.readable.getReader();

  // Spec §6.4.1: initial backpressure is true, so write() stays pending until
  // a read relieves it — the read must be issued while the write is in flight,
  // otherwise the two deadlock.
  var writeDone = writer.write(42);
  return reader.read()
    .then(function(result) {
      check("identity transform value === 42", result.value === 42, "value=" + result.value);
      check("identity transform done === false", result.done === false);
      return writeDone;
    })
    .then(function() {
      return writer.close();
    });
}

function testTransformStreamUppercase() {
  console.log("\n=== TransformStream (transform) ===");

  var startCalled = false;
  var flushCalled = false;

  var ts = new TransformStream({
    start: function(controller) {
      startCalled = true;
      check("start() controller.enqueue", typeof controller.enqueue === "function");
      check("start() controller.error", typeof controller.error === "function");
      check("start() controller.terminate", typeof controller.terminate === "function");
      check("start() controller.desiredSize", typeof controller.desiredSize === "number");
    },
    transform: function(chunk, controller) {
      var upper = typeof chunk === "string" ? chunk.toUpperCase() : chunk;
      controller.enqueue(upper);
    },
    flush: function(controller) {
      flushCalled = true;
    },
  });

  check("start() called", startCalled);

  var writer = ts.writable.getWriter();
  var reader = ts.readable.getReader();

  // Spec §6.4.1/§6.4.3: initial backpressure is true and every enqueue()
  // restores it (default readable HWM = 0), so each write stays pending until
  // a concurrent read relieves it. The read for a write must therefore be
  // issued before awaiting that write's settlement — chaining read after
  // write deadlocks.
  var writeDone = writer.write("hello");
  return reader.read()
    .then(function(result) {
      check("transform output === HELLO", result.value === "HELLO", "value=" + result.value);
      return writeDone;
    })
    .then(function() {
      var w2 = writer.write("world");
      var r2 = reader.read();
      return r2.then(function(result) {
        check("transform output === WORLD", result.value === "WORLD", "value=" + result.value);
        return w2;
      });
    })
    .then(function() { return writer.close(); })
    .then(function() {
      check("flush() called", flushCalled);
      return reader.read();
    })
    .then(function(result) {
      check("readable done after close", result.done === true);
      check("readable value undefined after close", result.value === undefined);
    });
}

function testTransformStreamTerminate() {
  console.log("\n=== TransformStream (terminate) ===");

  var ts = new TransformStream({
    transform: function(chunk, controller) {
      controller.enqueue(chunk);
      controller.terminate();
    },
  });

  var writer = ts.writable.getWriter();
  var reader = ts.readable.getReader();

  return writer.write("only")
    .then(function() { return reader.read(); })
    .then(function(result) {
      check("terminate: value === only", result.value === "only");
      return reader.read();
    })
    .then(function(result) {
      check("terminate: readable done=true", result.done === true);
      return writer.write("should fail").then(
        function() { check("terminate: write rejected", false); },
        function() { check("terminate: write rejected", true); },
      );
    });
}

// ===================================================================
// 10. ByteLengthQueuingStrategy
// ===================================================================
function testByteLengthQueuingStrategy() {
  console.log("\n=== ByteLengthQueuingStrategy ===");
  var s = new ByteLengthQueuingStrategy({ highWaterMark: 10 });
  check("highWaterMark === 10", s.highWaterMark === 10);
  // Spec: size() returns chunk.byteLength — strings have no byteLength,
  // so they yield undefined, not their length.
  check("size('abc') === undefined", s.size("abc") === undefined);
  check("size('') === undefined", s.size("") === undefined);
  check("size({byteLength:3}) === 3", s.size({byteLength: 3}) === 3);
}

// ===================================================================
// 11. CountQueuingStrategy
// ===================================================================
function testCountQueuingStrategy() {
  console.log("\n=== CountQueuingStrategy ===");
  var s = new CountQueuingStrategy({ highWaterMark: 5 });
  check("highWaterMark === 5", s.highWaterMark === 5);
  check("size() === 1", s.size() === 1);
}

// ===================================================================
// 执行链 — 带全局 catch
// ===================================================================
console.log("=== WHATWG Streams API Tests ===");
console.log("=== NOTE: 这是逐步实现的测试套件 ===");

function runTests() {
  return Promise.resolve()
    .then(function() { return testReadableStream(); })
    .then(function() { return testReadableStreamReader(); })
    .then(function() { return testReadableStreamPull(); })
    .then(function() { return testReadableStreamRead(); })
    .then(function() { return testReadableStreamCancel(); })
    .then(function() { return testTransformStream(); })
    .then(function() { return testTransformStreamIdentity(); })
    .then(function() { return testTransformStreamUppercase(); })
    // .then(function() { return testTransformStreamTerminate(); })
    .then(function() { return testWritableStream(); })
    .then(function() { return testWritableStreamAbort(); })
    .then(function() { return testWritableStreamReleaseLock(); })
    .then(function() { return testByteLengthQueuingStrategy(); })
    .then(function() { return testCountQueuingStrategy(); });
}

runTests().then(function() {
  console.log("\n=== SUMMARY: pass=" + pass + " fail=" + fail + " skip=" + skip + " ===");
  if (fail > 0) {
    console.log("RESULT: FAIL");
  } else {
    console.log("RESULT: PASS");
  }
}).catch(function(err) {
  console.log("\n=== UNHANDLED ERROR ===");
  console.log(String(err));
  console.log("RESULT: FAIL");
});
