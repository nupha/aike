import * as Path from "path";

// --- Helpers ---
let pass = 0;
let fail = 0;
function check(label, condition, detail) {
  if (condition) {
    console.log(`  ✅ ${label}`);
    pass++;
  } else {
    console.log(`  ❌ ${label}${detail ? " — " + detail : ""}`);
    fail++;
  }
}

// ===================================================================
// Aike.stdio tests
// ===================================================================
async function testStdio() {
  console.log("\n=== process.stdio ===");

  // 1. Existence and type
  check("process.stdin exists", process.stdin !== undefined && process.stdin !== null);
  check(
    "process.stdout exists",
    process.stdout !== undefined && process.stdout !== null,
  );
  check(
    "process.stderr exists",
    process.stderr !== undefined && process.stderr !== null,
  );

  // 2. fd values
  check("stdin.fd === 0", process.stdin.fd === 0, `got ${process.stdin.fd}`);
  check("stdout.fd === 1", process.stdout.fd === 1, `got ${process.stdout.fd}`);
  check("stderr.fd === 2", process.stderr.fd === 2, `got ${process.stderr.fd}`);

  // 3. Property descriptors — enumerable, non-writable, non-configurable
  for (const name of ["stdin", "stdout", "stderr"]) {
    const d = Object.getOwnPropertyDescriptor(process, name);
    check(`${name} enumerable`, d.enumerable === true, `got ${d.enumerable}`);
    check(`${name} non-writable`, d.writable === false, `got ${d.writable}`);
    check(
      `${name} non-configurable`,
      d.configurable === false,
      `got ${d.configurable}`,
    );
  }

  // 4. stdout.writeSync()
  process.stdout.writeSync("hello from stdout\n");
  check("stdout.writeSync works", true);

  // 5. stderr.writeSync()
  process.stderr.writeSync("hello from stderr\n");
  check("stderr.writeSync works", true);

  // 6. Cannot read from stdout/stderr
  let stdoutReadError = false;
  try {
    await process.stdout.read();
  } catch (e) {
    stdoutReadError = true;
  }
  check("stdout.read() throws", stdoutReadError);

  let stderrReadError = false;
  try {
    await process.stderr.read();
  } catch (e) {
    stderrReadError = true;
  }
  check("stderr.read() throws", stderrReadError);

  // 7. Cannot onData on stdout/stderr
  let stdoutOnReadError = false;
  try {
    process.stdout.onData(() => {});
  } catch (e) {
    stdoutOnReadError = true;
  }
  check("stdout.onData() throws", stdoutOnReadError);

  let stderrOnDataError = false;
  try {
    process.stderr.onData(() => {});
  } catch (e) {
    stderrOnDataError = true;
  }
  check("stderr.onData() throws", stderrOnDataError);

  // 8. stdin is same object across accesses
  check("stdin is stable", process.stdin === process.stdin);
  check("stdout is stable", process.stdout === process.stdout);
  check("stderr is stable", process.stderr === process.stderr);
}

// ===================================================================
// EventEmitter tests
// ===================================================================
async function testEventEmitter() {
  console.log("\n=== EventEmitter ===");

  // 1. Existence and instantiation
  check("EventEmitter is a function", typeof EventEmitter === "function");
  const ee = new EventEmitter();
  check("EventEmitter instance created", ee !== undefined && ee !== null);

  // 2. Expected methods exist
  const methods = [
    "on",
    "addListener",
    "once",
    "off",
    "removeListener",
    "emit",
    "removeAllListeners",
    "listeners",
    "listenerCount",
    "eventNames",
  ];
  for (const m of methods) {
    check(`ee.${m} is a function`, typeof ee[m] === "function");
  }

  // 3. on + emit — basic
  let called = false;
  ee.on("test", function () {
    console.log("---------", this, ee);
    called = true;
  });
  const emitResult = ee.emit("test");
  check("on/emit basic fires callback", called);
  check(
    "emit returns true when listeners fire",
    emitResult === true,
    `got ${emitResult}`,
  );

  // 4. on returns this
  const chainRet = ee.on("chain", () => {});
  check("on returns this for chaining", chainRet === ee);

  // 5. addListener alias
  let addListenerCalled = false;
  ee.addListener("addListenerTest", () => {
    addListenerCalled = true;
  });
  ee.emit("addListenerTest");
  check("addListener alias works", addListenerCalled);

  // 6. Multiple listeners for same event
  let countA = 0;
  let countB = 0;
  ee.on("multi", () => {
    countA++;
  });
  ee.on("multi", () => {
    countB++;
  });
  ee.emit("multi");
  check("multiple listeners both fire", countA === 1 && countB === 1);

  // 7. emit passes args
  let capturedArgs = null;
  ee.on("args", (...args) => {
    capturedArgs = args;
  });
  ee.emit("args", 1, "two", { three: 3 });
  check("emit passes args", capturedArgs !== null && capturedArgs.length === 3);
  check("emit arg[0] === 1", capturedArgs?.[0] === 1);
  check("emit arg[1] === 'two'", capturedArgs?.[1] === "two");
  check("emit arg[2].three === 3", capturedArgs?.[2]?.three === 3);

  // 8. emit returns false when no listeners
  const noListeners = new EventEmitter();
  check(
    "emit returns false when no listeners",
    noListeners.emit("nonexistent") === false,
  );

  // 9. once — fires once
  let onceCount = 0;
  ee.once("onlyOnce", () => {
    onceCount++;
  });
  ee.emit("onlyOnce");
  ee.emit("onlyOnce");
  ee.emit("onlyOnce");
  check("once fires only 1 time", onceCount === 1, `got ${onceCount}`);

  // 10. once returns this
  const onceRet = ee.once("onceChain", () => {});
  check("once returns this for chaining", onceRet === ee);

  // 11. off/removeListener — remove specific listener
  let offCalled = 0;
  const fnToRemove = () => {
    offCalled++;
  };
  const fnToKeep = () => {
    offCalled++;
  };
  ee.on("offTest", fnToRemove);
  ee.on("offTest", fnToKeep);
  ee.off("offTest", fnToRemove);
  ee.emit("offTest");
  check(
    "off removes specific listener (only 1 fires)",
    offCalled === 1,
    `got ${offCalled}`,
  );

  // 12. off returns this
  const offRet = ee.off("offTest", fnToKeep);
  check("off returns this for chaining", offRet === ee);

  // 13. removeListener alias
  const fnAlias = () => {};
  ee.on("removeListenerAlias", fnAlias);
  const aliasRet = ee.removeListener("removeListenerAlias", fnAlias);
  check("removeListener alias returns this", aliasRet === ee);
  check(
    "removeListener alias works",
    ee.listenerCount("removeListenerAlias") === 0,
  );

  // 14. listenerCount
  const lc = new EventEmitter();
  check("listenerCount empty is 0", lc.listenerCount("x") === 0);
  lc.on("x", () => {});
  check("listenerCount with 1 listener", lc.listenerCount("x") === 1);
  lc.on("x", () => {});
  check("listenerCount with 2 listeners", lc.listenerCount("x") === 2);

  // 15. listeners()
  const lsn = new EventEmitter();
  const fn1 = () => 1;
  const fn2 = () => 2;
  const emptyArr = lsn.listeners("nonexistent");
  check(
    "listeners empty returns []",
    Array.isArray(emptyArr) && emptyArr.length === 0,
  );
  lsn.on("ev", fn1);
  lsn.on("ev", fn2);
  const arr = lsn.listeners("ev");
  check("listeners returns array", Array.isArray(arr) && arr.length === 2);

  // 16. eventNames()
  const en = new EventEmitter();
  check("eventNames empty", en.eventNames().length === 0);
  en.on("a", () => {});
  en.on("b", () => {});
  const names = en.eventNames();
  check("eventNames returns registered names", names.length === 2);
  check("eventNames includes 'a'", names.includes("a"));
  check("eventNames includes 'b'", names.includes("b"));

  // 17. removeAllListeners
  const ra = new EventEmitter();
  ra.on("x", () => {});
  ra.on("y", () => {});
  ra.removeAllListeners("x");
  check(
    "removeAllListeners('x') removes x listeners",
    ra.listenerCount("x") === 0,
  );
  check(
    "removeAllListeners('x') keeps y listeners",
    ra.listenerCount("y") === 1,
  );
  ra.removeAllListeners();
  check("removeAllListeners() removes all", ra.eventNames().length === 0);
  const raRet = ra.removeAllListeners();
  check("removeAllListeners returns this", raRet === ra);

  // 18. Signal constraints — on/once with signal name throws TypeError
  for (const method of ["on", "once"]) {
    let err = null;
    try {
      ee[method]("SIGINT", () => {});
    } catch (e) {
      err = e;
    }
    check(`${method}("SIGINT") throws TypeError`, err instanceof TypeError);
    check(
      `${method}("SIGINT") mentions Aike.onSignal`,
      err?.message?.includes?.("Aike.onSignal"),
    );
  }

  // 19. Signal constraints — emit with signal name throws TypeError
  let emitSigErr = null;
  try {
    ee.emit("SIGTERM");
  } catch (e) {
    emitSigErr = e;
  }
  check('emit("SIGTERM") throws TypeError', emitSigErr instanceof TypeError);

  // 20. Error in listener is ignored (Node.js convention)
  const errEm = new EventEmitter();
  errEm.on("boom", () => {
    throw new Error("listener error");
  });
  let emitOk = false;
  try {
    errEm.emit("boom");
    emitOk = true;
  } catch (_) {}
  check("listener error does not propagate", emitOk);

  // 21. Multiple instances are isolated
  const a = new EventEmitter();
  const b = new EventEmitter();
  let aCalled = false;
  let bCalled = false;
  a.on("sharedName", () => {
    aCalled = true;
  });
  b.on("sharedName", () => {
    bCalled = true;
  });
  a.emit("sharedName");
  check("instance A fires its own listener", aCalled);
  check("instance B is not affected by A's emit", !bCalled);
}

// ===================================================================
// Aike.onSignal tests
// ===================================================================
function testOnSignal() {
  console.log("\n=== Aike.onSignal ===");

  // 0. Aike.signals exists and contains platform-available signals
  check("Aike.signals is an object", typeof Aike.signals === "object" && Aike.signals !== null);
  check("Aike.signals.SIGINT is a number", typeof Aike.signals.SIGINT === "number");
  check("Aike.signals.SIGTERM is a number", typeof Aike.signals.SIGTERM === "number");
  check(
    "Aike.signals.SIGUSR1 may be undefined (Windows) or number (Unix)",
    Aike.signals.SIGUSR1 === undefined || typeof Aike.signals.SIGUSR1 === "number",
  );

  // 1. onSignal exists and is a function
  check(
    "onSignal exists",
    typeof Aike.onSignal === "function",
    `got ${typeof Aike.onSignal}`,
  );

  // 2. Unknown signal throws TypeError
  let unknownError = false;
  try {
    Aike.onSignal("SIGFOO", () => {});
  } catch (e) {
    unknownError = e instanceof TypeError;
  }
  check("unknown signal throws TypeError", unknownError);

  // 3. Non-function handler throws
  let nonFuncError = false;
  try {
    Aike.onSignal("SIGINT", 42);
  } catch (e) {
    nonFuncError = true;
  }
  check("non-function handler throws", nonFuncError);

  // 4-6. Unix-only signals (e.g. SIGUSR1) — test only if available on this platform
  if (Aike.signals.SIGUSR1 !== undefined) {
    // 4. Register a handler returns `this` (Aike for chaining)
    const ret = Aike.onSignal("SIGUSR1", () => {});
    check("onSignal returns this", ret === Aike, `got ${typeof ret}`);

    // 5. Overwrite (re-register same signal) succeeds
    let overwriteOk = false;
    let lastCalled = null;
    Aike.onSignal("SIGUSR1", () => {
      lastCalled = "cb2";
    });
    overwriteOk = true;
    check("re-registration overwrites", overwriteOk);

    // 6. Remove with null
    let removeOk = false;
    try {
      Aike.onSignal("SIGUSR1", null);
      removeOk = true;
    } catch (e) {}
    check("remove with null succeeds", removeOk);
  } else {
    console.log("  ⏭️  SIGUSR1 tests skipped (not available on this platform)");
  }

  // 7. Remove non-existent handler is no-op (no crash)
  let removeMissingOk = false;
  try {
    Aike.onSignal("SIGTERM", null);
    removeMissingOk = true;
  } catch (e) {}
  check("remove non-existent is no-op", removeMissingOk);

  // 8. Register multiple different signals
  let multiOk = false;
  try {
    Aike.onSignal("SIGINT", () => {});
    Aike.onSignal("SIGTERM", () => {});
    multiOk = true;
  } catch (e) {}
  check("register multiple signals", multiOk);

  // Cleanup: remove all handlers registered in this test
  Aike.onSignal("SIGINT", null);
  Aike.onSignal("SIGTERM", null);
  if (Aike.signals.SIGUSR1 !== undefined) {
    Aike.onSignal("SIGUSR1", null);
  }
}

// ===================================================================
// spawnSync tests
// ===================================================================
async function testSpawnSync() {
  console.log("\n=== Aike.spawnSync ===");

  // 1. Existence
  check("spawnSync exists", typeof Aike.spawnSync === "function");

  // 2-10. Unix-only spawnSync tests (skip on Windows)
  if (process.platform !== "windows") {
    // 2. Basic echo with args array
    {
      const r = Aike.spawnSync("echo", ["hello spawnSync"]);
      check("returns object", typeof r === "object" && r !== null);
      check("pid is positive int", Number.isInteger(r.pid) && r.pid > 0);
      check("stdout is Uint8Array", r.stdout instanceof Uint8Array);
      check(
        "stdout content",
        new TextDecoder().decode(r.stdout).trim() === "hello spawnSync",
        `got ${JSON.stringify(new TextDecoder().decode(r.stdout))}`,
      );
      check(
        "stderr is empty Uint8Array",
        r.stderr instanceof Uint8Array && r.stderr.length === 0,
      );
      check("exitCode === 0", r.exitCode === 0);
      check("signalCode === null", r.signalCode === null);
      check("success === true", r.success === true);
    }

    // 3. Non-zero exit
    {
      const r = Aike.spawnSync("false");
      check("false exitCode !== 0", r.exitCode !== 0);
      check("false success === false", r.success === false);
      check("false signalCode === null", r.signalCode === null);
    }

    // 4. With input (stdin pipe)
    {
      const inputStr = "hello stdin\nline2";
      const r = Aike.spawnSync("cat", [], { input: inputStr });
      check(
        "cat+input stdout matches",
        new TextDecoder().decode(r.stdout) === inputStr,
        `got ${JSON.stringify(new TextDecoder().decode(r.stdout))}`,
      );
      check("cat+input exitCode === 0", r.exitCode === 0);
    }

    // 5. env option
    {
      const r = Aike.spawnSync("sh", ["-c", "echo $MY_VAR"], {
        env: { MY_VAR: "test_env_val" },
      });
      check(
        "env MY_VAR",
        new TextDecoder().decode(r.stdout).trim() === "test_env_val",
        `got ${JSON.stringify(new TextDecoder().decode(r.stdout))}`,
      );
      check("env exitCode === 0", r.exitCode === 0);
    }

    // 6. cwd option (macOS /tmp symlinks to /private/tmp)
    {
      const r = Aike.spawnSync("pwd", [], { cwd: "/tmp" });
      const cwdOut = new TextDecoder().decode(r.stdout).trim();
      check(
        "cwd /tmp",
        cwdOut === "/tmp" || cwdOut === "/private/tmp",
        `got ${JSON.stringify(cwdOut)}`,
      );
    }

    // 7. stdout mode = ignore
    {
      const r = Aike.spawnSync("echo", ["ignored"], { stdout: "ignore" });
      check("stdout=ignore gives empty", r.stdout.length === 0);
      check("stdout=ignore exitCode === 0", r.exitCode === 0);
    }

    // 8. stderr content + stdout still works
    {
      const r = Aike.spawnSync("sh", ["-c", "echo err_msg >&2; echo out_msg"]);
      check(
        "stderr content",
        new TextDecoder().decode(r.stderr).trim() === "err_msg",
        `got ${JSON.stringify(new TextDecoder().decode(r.stderr))}`,
      );
      check(
        "stdout still captured",
        new TextDecoder().decode(r.stdout).trim() === "out_msg",
        `got ${JSON.stringify(new TextDecoder().decode(r.stdout))}`,
      );
    }

    // 9. No args array — just command string
    {
      const r = Aike.spawnSync("true");
      check("true exitCode === 0", r.exitCode === 0);
      check("true success === true", r.success === true);
    }

    // 10. stdin/inherit (default stdin is inherit, so echo works without input)
    {
      const r = Aike.spawnSync("echo", ["default stdin"]);
      check(
        "default stdin inherit works",
        new TextDecoder().decode(r.stdout).trim() === "default stdin",
      );
      check("pid is number", typeof r.pid === "number" && r.pid > 0);
    }
  }
}

// ===================================================================
// spawn tests
// ===================================================================
async function testSpawn() {
  console.log("\n=== Aike.spawn ===");

  // 1. Existence
  check("spawn exists", typeof Aike.spawn === "function");

  // 2-12. Unix-only async spawn tests (skip on Windows)
  if (process.platform !== "windows") {
    // 2. Basic async echo — read stdout via FileHandle.text()
    {
      const p = Aike.spawn("echo", ["hello async"]);
      check("returns object", typeof p === "object" && p !== null);
      check(
        "pid is positive int",
        Number.isInteger(p.pid) && p.pid > 0,
        `got ${p.pid}`,
      );
      check(
        "stdout is IoHandle (not null)",
        p.stdout !== null && typeof p.stdout === "object",
        `got ${typeof p.stdout}`,
      );
      check(
        "stderr is IoHandle (not null)",
        p.stderr !== null && typeof p.stderr === "object",
      );
      check("stdin is null (inherit)", p.stdin === null);
      check("exitCode is Promise", p.exitCode instanceof Promise);
      check("signalCode is Promise", p.signalCode instanceof Promise);
      check("kill is function", typeof p.kill === "function");
      check("isRunning is function", typeof p.isRunning === "function");

      const stdoutBytes = new TextDecoder().decode(await p.stdout.bytes()).trim();
      check(
        "stdout content via .bytes()",
        stdoutBytes === "hello async",
        `got ${JSON.stringify(stdoutBytes)}`,
      );
      const exitCode = await p.exitCode;
      check("exitCode === 0", exitCode === 0, `got ${exitCode}`);
      const sigCode = await p.signalCode;
      check("signalCode === null", sigCode === null, `got ${sigCode}`);
    }

    // 3. spawn with stdin pipe + cat
    {
      const p = Aike.spawn("cat", [], { stdin: "pipe" });
      check(
        "stdin is IoHandle (piped)",
        p.stdin !== null && typeof p.stdin === "object",
      );
      check("stdin.fd is number", typeof p.stdin.fd === "number");

      const writeData = "hello from spawn stdin\nsecond line";
      p.stdin.writeSync(writeData);
      await p.stdin.close();

      const stdoutBytes = await p.stdout.bytes();
      const stdoutStr = new TextDecoder().decode(stdoutBytes);
      check(
        "cat stdin→stdout via .bytes()",
        stdoutStr === writeData,
        `got ${JSON.stringify(stdoutStr)}`,
      );
      const code = await p.exitCode;
      check("cat exitCode === 0", code === 0, `got ${code}`);
    }

    // 4. stdout + stderr separation
    {
      const p = Aike.spawn("sh", [
        "-c",
        "echo stderr_line >&2; echo stdout_line",
      ]);
      const out = new TextDecoder().decode(await p.stdout.bytes()).trim();
      const err = new TextDecoder().decode(await p.stderr.bytes()).trim();
      await p.exitCode;
      check(
        "stdout content",
        out === "stdout_line",
        `got ${JSON.stringify(out)}`,
      );
      check(
        "stderr content",
        err === "stderr_line",
        `got ${JSON.stringify(err)}`,
      );
    }

    // 5. env option
    {
      const p = Aike.spawn("sh", ["-c", "echo $SPAWN_TEST"], {
        env: { SPAWN_TEST: "env_works" },
      });
      const out = new TextDecoder().decode(await p.stdout.bytes()).trim();
      await p.exitCode;
      check("spawn env", out === "env_works", `got ${JSON.stringify(out)}`);
    }

    // 6. stdout mode = ignore
    {
      const p = Aike.spawn("echo", ["ignore_me"], { stdout: "ignore" });
      check("stdout=ignore is null", p.stdout === null);
      const code = await p.exitCode;
      check("stdout=ignore exitCode === 0", code === 0, `got ${code}`);
    }

    // 7. stderr mode = ignore
    {
      const p = Aike.spawn("sh", ["-c", "echo visible"], { stderr: "ignore" });
      const out = new TextDecoder().decode(await p.stdout.bytes()).trim();
      await p.exitCode;
      check(
        "stderr=ignore stdout still works",
        out === "visible",
        `got ${JSON.stringify(out)}`,
      );
    }

    // 8. stdout.read() — incremental read
    {
      const p = Aike.spawn("echo", ["chunk_test"]);
      const chunk = await p.stdout.read();
      check(
        ".read() returns Uint8Array",
        chunk instanceof Uint8Array,
        `got ${typeof chunk}`,
      );
      check(
        ".read() content",
        new TextDecoder().decode(chunk).includes("chunk_test"),
      );
      await p.exitCode;
    }

    // 9. kill + signalCode
    {
      const p = Aike.spawn("sleep", ["30"]);
      check("isRunning before kill", p.isRunning() === true);
      const killed = p.kill();
      check("kill returns true", killed === true, `got ${killed}`);
      const exitCode = await p.exitCode;
      // killed by signal: exitCode is -1
      const sigCode = await p.signalCode;
      check("signalCode is SIGTERM", sigCode === "SIGTERM", `got ${sigCode}`);
      // wait a brief moment for OS to reap
      await new Promise((resolve) => setTimeout(resolve, 200));
      check("isRunning after kill", p.isRunning() === false);
    }

    // 10. async iteration (for await...of) on stdout
    {
      const p = Aike.spawn("echo", ["iter_test"]);
      let collected = "";
      for await (const chunk of p.stdout) {
        collected += new TextDecoder().decode(chunk);
      }
      await p.exitCode;
      check(
        "async iteration on stdout",
        collected.trim() === "iter_test",
        `got ${JSON.stringify(collected)}`,
      );
    }

    // 11. stderr default mode = pipe
    {
      const p = Aike.spawn("sh", ["-c", "echo err_only >&2"]);
      const errBytes = new TextDecoder().decode(await p.stderr.bytes()).trim();
      await p.exitCode;
      check(
        "stderr piped (default)",
        errBytes === "err_only",
        `got ${JSON.stringify(errBytes)}`,
      );
    }

    // 12. stderr ignore + stdout
    {
      const p = Aike.spawn("sh", ["-c", "echo out"], { stderr: "ignore" });
      check("stderr is null when ignored", p.stderr === null);
      const out = new TextDecoder().decode(await p.stdout.bytes()).trim();
      await p.exitCode;
      check(
        "stdout after stderr=ignore",
        out === "out",
        `got ${JSON.stringify(out)}`,
      );
    }
  }
}

// ===================================================================
// process.cwd tests
// ===================================================================
function testCwd() {
  console.log("\n=== process.cwd ===");
  check("cwd is a function", typeof process.cwd === "function");
  const cwd = process.cwd();
  check("cwd returns a string", typeof cwd === "string");
  check("cwd is absolute path", Path.isAbsolute(cwd), `got ${cwd}`);
  check("cwd is non-empty", cwd.length > 0);
}

// ===================================================================
// process.env tests
// ===================================================================
function testEnv() {
  console.log("\n=== process.env ===");
  check("env exists", typeof process.env === "object" && process.env !== null);
  // PATH: env var names are case-insensitive on Windows; check case-insensitively
  const envKeys = Object.keys(process.env);
  const pathKey = envKeys.find(k => k.toUpperCase() === "PATH");
  check(
    "env has PATH (case-insensitive)",
    typeof pathKey === "string" && typeof process.env[pathKey] === "string",
    `got PATH=${typeof process.env.PATH}, Path=${typeof process.env.Path}`,
  );
  if (pathKey) {
    check("env PATH length > 0", process.env[pathKey].length > 0);
  }

  // Property descriptor
  const d = Object.getOwnPropertyDescriptor(process, "env");
  check("env enumerable", d.enumerable === true);
  check("env non-writable", d.writable === false);
  check("env non-configurable", d.configurable === false);

  // Cannot replace the whole env object, but individual properties are writable (Node.js convention)
  let envReplaceError = false;
  try {
    process.env = {};
  } catch (_) {
    envReplaceError = true;
  }
  check("process.env cannot be replaced", envReplaceError);

  // Individual env properties are writable (use case-insensitive PATH key)
  const writablePathKey = pathKey || "PATH";
  check(
    `env.${writablePathKey} is writable`,
    (() => {
      const orig = process.env[writablePathKey];
      process.env[writablePathKey] = "__test_writeable__";
      const changed = process.env[writablePathKey] === "__test_writeable__";
      process.env[writablePathKey] = orig;
      return changed;
    })(),
  );
}

// ===================================================================
// Aike.evalScript tests
// ===================================================================
function testEvalScript() {
  console.log("\n=== Aike.evalScript ===");
  check("evalScript is a function", typeof Aike.evalScript === "function");

  check(
    "simple expression",
    Aike.evalScript("1 + 2") === 3,
    `got ${Aike.evalScript("1 + 2")}`,
  );
  check(
    "string result",
    Aike.evalScript("JSON.stringify({a:1})") === '{"a":1}',
  );
  check("boolean result", Aike.evalScript("!0") === true);
  check("null result", Aike.evalScript("null") === null);
  check("array result", Array.isArray(Aike.evalScript("[1,2,3]")));

  // backtrace_barrier option
  check(
    "backtrace_barrier option",
    Aike.evalScript("42", { backtrace_barrier: true }) === 42,
  );

  // async option — result is a Promise
  const asyncResult = Aike.evalScript("Promise.resolve(99)", { async: true });
  check(
    "async option returns Promise",
    asyncResult instanceof Promise,
    `got ${typeof asyncResult}`,
  );
}

// ===================================================================
// Aike.gc tests
// ===================================================================
function testGc() {
  console.log("\n=== Aike.gc ===");
  check("gc is a function", typeof Aike.gc === "function");
  let gcOk = false;
  try {
    Aike.gc();
    gcOk = true;
  } catch (e) {}
  check("gc() executes without throwing", gcOk);
}

// ===================================================================
// Aike.isatty + tty* tests
// ===================================================================
function testIsatty() {
  console.log("\n=== Aike.isatty ===");
  check("isatty is a function", typeof Aike.isatty === "function");
  check("isatty(0) returns boolean", typeof Aike.isatty(0) === "boolean");
  check("isatty(1) returns boolean", typeof Aike.isatty(1) === "boolean");
  check("isatty(2) returns boolean", typeof Aike.isatty(2) === "boolean");
  check("isatty(999) returns false", Aike.isatty(999) === false);
}

function testTty() {
  console.log("\n=== Aike.tty ===");
  check(
    "ttyGetWinSize is a function",
    typeof Aike.ttyGetWinSize === "function",
  );
  const ws = Aike.ttyGetWinSize(1);
  check(
    "ttyGetWinSize returns array or null",
    ws === null || (Array.isArray(ws) && ws.length === 2),
  );
  if (Array.isArray(ws)) {
    check("winSize cols > 0", ws[0] > 0, `got ${ws[0]}`);
    check("winSize rows > 0", ws[1] > 0, `got ${ws[1]}`);
  }

  check("ttySetRaw is a function", typeof Aike.ttySetRaw === "function");

  // ttySetRaw on invalid fd throws TypeError (Unix only — Windows impl doesn't validate fd)
  let ttyRawError = false;
  if (process.platform !== "windows") {
    try {
      Aike.ttySetRaw(999);
    } catch (e) {
      ttyRawError = e instanceof TypeError;
    }
    check("ttySetRaw(999) throws TypeError", ttyRawError);
  }
}

// ===================================================================
// Aike.now tests
// ===================================================================
function testNow() {
  console.log("\n=== Aike.now ===");
  check("now is a function", typeof Aike.now === "function");
  const t1 = Aike.now();
  check("now returns a number", typeof t1 === "number", `got ${typeof t1}`);
  check("now returns positive value", t1 > 0, `got ${t1}`);
  const t2 = Aike.now();
  check("now is monotonic (t2 >= t1)", t2 >= t1, `got t1=${t1}, t2=${t2}`);
}

// ===================================================================
// Aike.schedule + CronJob tests
// ===================================================================
function testSchedule() {
  console.log("\n=== Aike.schedule ===");
  check("schedule is a function", typeof Aike.schedule === "function");

  // Schedule with a valid 5-field pattern
  const job = Aike.schedule("0 0 1 1 *", () => {}); // Jan 1 midnight (won't fire soon)
  check("schedule returns an object", typeof job === "object" && job !== null);
  check(
    "job.id is a positive number",
    Number.isInteger(job.id) && job.id > 0,
    `got ${job.id}`,
  );
  check(
    "job.name is null",
    job.name === null,
    `got ${JSON.stringify(job.name)}`,
  );
  check(
    "job.pattern matches",
    job.pattern === "0 0 1 1 *",
    `got ${job.pattern}`,
  );
  check(
    "job.nextRun is string or null",
    typeof job.nextRun === "string" || job.nextRun === null,
  );
  check("job.active is initially true", job.active === true);
  check("job.cancel is a function", typeof job.cancel === "function");

  // Test with name option
  const namedJob = Aike.schedule("0 0 * * *", () => {}, {
    name: "test-schedule",
  });
  check(
    "named job.name",
    namedJob.name === "test-schedule",
    `got ${JSON.stringify(namedJob.name)}`,
  );

  // Cancel
  namedJob.cancel();
  check("job.active is false after cancel", namedJob.active === false);

  // Invalid pattern throws
  let patternError = false;
  try {
    Aike.schedule("invalid pattern", () => {});
  } catch (e) {
    patternError = true;
  }
  check("invalid cron pattern throws", patternError);

  // Cancel returns undefined
  check("cancel() returns undefined", job.cancel() === undefined);
}

// ===================================================================
// Aike.sleep tests
// ===================================================================
async function testSleep() {
  console.log("\n=== Aike.sleep ===");

  // 1. Existence and type
  check("sleep is a function", typeof Aike.sleep === "function");

  // 2. Resolves after the requested delay (event-loop driven, not blocking)
  {
    const start = Date.now();
    await Aike.sleep(120);
    const elapsed = Date.now() - start;
    check("resolves after ~120ms", elapsed >= 100, `elapsed=${elapsed}ms`);
  }

  // 3. Yields to the event loop — .then callback runs after code continues
  {
    const order = [];
    Aike.sleep(40).then(() => order.push("sleep-done"));
    order.push("after-await");
    await Aike.sleep(60);
    check(
      "sleep callback runs after await",
      order[0] === "after-await" && order[1] === "sleep-done",
      `order=${JSON.stringify(order)}`,
    );
  }

  // 4. Chains with setTimeout on the same timer queue
  {
    const seq = [];
    setTimeout(() => seq.push("timeout"), 20);
    Aike.sleep(20).then(() => seq.push("sleep"));
    await Aike.sleep(50);
    check("both timers fired", seq.length === 2, `seq=${JSON.stringify(seq)}`);
    check(
      "order respects delay",
      seq.includes("timeout") && seq.includes("sleep"),
      `seq=${JSON.stringify(seq)}`,
    );
  }

  // 5. sleep(0) fires on the next tick, not synchronously
  {
    let fired = false;
    Aike.sleep(0).then(() => {
      fired = true;
    });
    check("not fired synchronously", !fired, "");
    await Aike.sleep(10);
    check("fired after yield", fired, "");
  }
}

// ===================================================================
// process runtime properties (argv, execArgv, execPath) + Aike.homedir
// ===================================================================
function testRuntimeProps() {
  console.log("\n=== process.runtime ===");

  check("platform is a string", typeof process.platform === "string");
  check("platform is non-empty", process.platform.length > 0);

  // Platform should match one of the standard Node.js values
  const knownPlatforms = ["windows", "darwin", "linux", "android", "freebsd", "openbsd", "aix", "sunos"];
  check(
    "platform is a known value",
    knownPlatforms.includes(process.platform),
    `got ${JSON.stringify(process.platform)}`,
  );

  // ── homedir ──
  check("homedir is a function", typeof Aike.homedir === "function");
  const home = Aike.homedir();
  check("homedir returns a string", typeof home === "string", `got ${typeof home}`);
  check("homedir is non-empty", home.length > 0, `got ${JSON.stringify(home)}`);
  check("homedir is absolute path", Path.isAbsolute(home), `got ${home}`);

  check("argv is an array", Array.isArray(process.argv));
  check("argv has at least 1 element", process.argv.length >= 1);
  check("argv[0] is a string", typeof process.argv[0] === "string");

  check("execArgv is an array", Array.isArray(process.execArgv));

  check("execPath is a string", typeof process.execPath === "string");
  check("execPath is absolute", Path.isAbsolute(process.execPath), `got ${process.execPath}`);

  // Property descriptors
  for (const name of ["argv", "execArgv", "execPath"]) {
    const d = Object.getOwnPropertyDescriptor(process, name);
    check(`${name} enumerable`, d.enumerable === true, `got ${d.enumerable}`);
  }
}

// ===================================================================
// Main
// ===================================================================
await testStdio();
await testEventEmitter();
await testOnSignal();
await testSpawnSync();
await testSpawn();
await testCwd();
await testEnv();
await testEvalScript();
await testGc();
await testIsatty();
await testTty();
await testNow();
await testSchedule();
await testSleep();
await testRuntimeProps();

// Summary
console.log(`\n${"=".repeat(40)}`);
console.log(`  Pass: ${pass}  Fail: ${fail}`);
if (fail > 0) {
  console.log("  SOME TESTS FAILED");
} else {
  console.log("  ALL TESTS PASSED");
}
