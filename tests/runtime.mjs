// Multi-runtime JS API tests (`Aike.createRuntime`).

let pass = 0;
let fail = 0;
function check(label, cond, detail) {
  if (cond) {
    console.log(`  PASS ${label}`);
    pass++;
  } else {
    console.log(`  FAIL ${label}${detail !== undefined ? " — " + detail : ""}`);
    fail++;
  }
}

async function testBasic() {
  console.log("\n=== basic ===");
  check("Aike.createRuntime exists", typeof Aike.createRuntime === "function");

  const rt = Aike.createRuntime();
  check("handle has run", typeof rt.run === "function");
  check("handle has postMessage", typeof rt.postMessage === "function");
  check("handle has close", typeof rt.close === "function");

  const v = await rt.run("6 * 7");
  check("run returns completion value", v === 42, `got ${v}`);

  const obj = await rt.run("({ a: 1, b: [1, 2, 3], s: 'hi' })");
  check(
    "run returns structured value",
    obj &&
      obj.a === 1 &&
      Array.isArray(obj.b) &&
      obj.b[2] === 3 &&
      obj.s === "hi",
    JSON.stringify(obj),
  );

  // run twice on the same handle
  const a = await rt.run(
    "let x = (globalThis.__x || 0) + 1; globalThis.__x = x; x",
  );
  const b = await rt.run("globalThis.__x");
  check("state persists across runs", a === 1 && b === 1, `${a},${b}`);

  rt.close();
  rt.close(); // idempotent
  let closedThrew = false;
  try {
    rt.run("1");
  } catch (e) {
    closedThrew = true;
  }
  check("run after close throws", closedThrew);
}

async function testIsolation() {
  console.log("\n=== isolation ===");
  const rt = Aike.createRuntime();
  check(
    "child has no Aike.createRuntime",
    (await rt.run("typeof Aike")) === "undefined",
  );
  check(
    "child has no process",
    (await rt.run("typeof process")) === "undefined",
  );
  check(
    "child has no console by default",
    (await rt.run("typeof console")) === "undefined",
  );
  check(
    "child eval is stripped",
    (await rt.run("typeof eval")) === "undefined",
  );
  check("child is pure ECMA", (await rt.run("typeof Object")) === "function");
  rt.close();

  const rt2 = Aike.createRuntime({ global: ["console"] });
  check(
    "whitelist installs console",
    (await rt2.run("typeof console")) === "object",
  );
  check(
    "whitelist excludes process",
    (await rt2.run("typeof process")) === "undefined",
  );
  rt2.close();

  const rt3 = Aike.createRuntime({ global: { blackList: ["process"] } });
  check(
    "blacklist excludes process",
    (await rt3.run("typeof process")) === "undefined",
  );
  check(
    "blacklist keeps console",
    (await rt3.run("typeof console")) === "object",
  );
  rt3.close();

  // object form: { whiteList }
  const rt5 = Aike.createRuntime({ global: { whiteList: ["console"] } });
  check(
    "object whiteList installs console",
    (await rt5.run("typeof console")) === "object",
  );
  check(
    "object whiteList excludes process",
    (await rt5.run("typeof process")) === "undefined",
  );
  rt5.close();

  // whiteList wins; blackList is ignored
  const rt6 = Aike.createRuntime({
    global: { whiteList: ["console"], blackList: ["console"] },
  });
  check(
    "whiteList wins over blackList",
    (await rt6.run("typeof console")) === "object",
  );
  rt6.close();

  // empty array / empty object forms
  const rt7 = Aike.createRuntime({ global: [] });
  check(
    "empty array = pure ECMA",
    (await rt7.run("typeof console")) === "undefined",
  );
  rt7.close();
  const rt8 = Aike.createRuntime({ global: {} });
  check(
    "empty object = fail-open (all caps)",
    (await rt8.run("typeof console")) === "object",
  );
  check(
    "empty object installs process",
    (await rt8.run("typeof process")) === "object",
  );
  rt8.close();

  // child never exposes host entry points, even when Aike is whitelisted
  const rt4 = Aike.createRuntime({ global: ["Aike"] });
  check(
    "child Aike has no createRuntime",
    (await rt4.run("typeof Aike.createRuntime")) === "undefined",
  );
  check(
    "child Aike has no evalScript",
    (await rt4.run("typeof Aike.evalScript")) === "undefined",
  );
  check(
    "child Aike has no evalModule",
    (await rt4.run("typeof Aike.evalModule")) === "undefined",
  );
  check(
    "child Aike has now()",
    (await rt4.run("typeof Aike.now")) === "function",
  );
  rt4.close();

  // invalid capability names / option types
  let badCap = false;
  try {
    Aike.createRuntime({ global: ["nope"] });
  } catch (e) {
    badCap = e instanceof TypeError;
  }
  check("unknown capability throws TypeError", badCap);

  let badGlobalType = false;
  try {
    Aike.createRuntime({ global: 42 });
  } catch (e) {
    badGlobalType = e instanceof TypeError;
  }
  check("non-array/object global throws TypeError", badGlobalType);

  let badGlobalElem = false;
  try {
    Aike.createRuntime({ global: ["console", 5] });
  } catch (e) {
    badGlobalElem = e instanceof TypeError;
  }
  check("non-string capability element throws TypeError", badGlobalElem);

  let badOptions = false;
  try {
    Aike.createRuntime(42);
  } catch (e) {
    badOptions = e instanceof TypeError;
  }
  check("non-object options throws TypeError", badOptions);
}

async function testMessaging() {
  console.log("\n=== messaging ===");
  const rt = Aike.createRuntime();

  await rt.run(
    "onmessage = (m) => { postMessage({ echo: m.n, doubled: m.n * 2 }); }",
  );
  const got = [];
  rt.onmessage = (v) => got.push(v);
  rt.postMessage({ n: 21 });

  // Give the parent loop a few ticks to deliver both ways.
  await Aike.sleep(20);
  check(
    "child received and replied",
    got.length === 1 && got[0].doubled === 42,
    JSON.stringify(got),
  );

  // function cannot cross
  let fnThrew = false;
  try {
    rt.postMessage(function () {});
  } catch (e) {
    fnThrew = e instanceof TypeError;
  }
  check("postMessage(function) throws TypeError", fnThrew);

  rt.close();
}

async function testErrorsAndTimeout() {
  console.log("\n=== errors / timeout ===");
  const rt = Aike.createRuntime({
    timeout: 200,
    global: ["console", "timers"],
  });

  let err = null;
  try {
    await rt.run('throw new TypeError("boom")');
  } catch (e) {
    err = e;
  }
  check("child throw rejects", err !== null);
  check(
    "error name surfaced",
    err && err.name === "TypeError",
    JSON.stringify(err),
  );
  check(
    "error message surfaced",
    err && /boom/.test(err.message),
    err && err.message,
  );

  let to = null;
  try {
    await rt.run("while (true) {}");
  } catch (e) {
    to = e;
  }
  check(
    "timeout aborts busy loop",
    to !== null && to.name === "TimeoutError",
    JSON.stringify(to),
  );

  // async wait (never-settling promise) also times out via the run deadline
  let to2 = null;
  try {
    await rt.run("new Promise(() => {})");
  } catch (e) {
    to2 = e;
  }
  check(
    "timeout aborts pending promise",
    to2 !== null && to2.name === "TimeoutError",
    JSON.stringify(to2),
  );

  // timeout is per-run: each run gets its own budget
  const t1 = await rt.run("new Promise((r) => setTimeout(() => r('a'), 30))");
  const t2 = await rt.run("new Promise((r) => setTimeout(() => r('b'), 30))");
  check(
    "timeout budget resets per run",
    t1 === "a" && t2 === "b",
    `${t1},${t2}`,
  );

  // runtime still usable after a timeout
  const ok = await rt.run("1 + 1");
  check("runtime usable after timeout", ok === 2, `got ${ok}`);

  rt.close();

  // timeout: 0 = unlimited (a normal short run still completes)
  const rt0 = Aike.createRuntime({ timeout: 0 });
  const u = await rt0.run("Promise.resolve().then(() => 7)");
  check("timeout 0 = unlimited (short run ok)", u === 7, `got ${u}`);
  rt0.close();
}

async function testAsyncChild() {
  console.log("\n=== async child ===");
  const rt = Aike.createRuntime({ global: ["timers"] });

  const v = await rt.run("Promise.resolve(123)");
  check("awaits completion promise", v === 123, `got ${v}`);

  const t = await rt.run(
    "new Promise((r) => setTimeout(() => r('later'), 30))",
  );
  check("child timer fires", t === "later", `got ${t}`);

  // Two runs issued without awaiting: both complete (independent jobs).
  const p1 = rt.run("new Promise((r) => setTimeout(() => r(1), 20))");
  const p2 = rt.run("2");
  const both = await Promise.all([p1, p2]);
  check(
    "concurrent runs both complete",
    both[0] === 1 && both[1] === 2,
    JSON.stringify(both),
  );

  // Per-runtime timers: a timer scheduled by a run keeps running *between*
  // runs, so the child can post a message with no run in flight. The root also
  // uses a timer concurrently, which must not disturb the child's timers.
  const got = [];
  rt.onmessage = (x) => got.push(x);
  await rt.run("setTimeout(() => postMessage('bg'), 30); 'ok'");
  await Aike.sleep(80);
  check(
    "child timer fires between runs",
    got.length === 1 && got[0] === "bg",
    JSON.stringify(got),
  );

  rt.close();
}

async function testMemoryLimit() {
  console.log("\n=== memory limit (plumbing only; see note) ===");
  // KNOWN LIMITATION: `memory` maps to the engine's GC memory limit, but the
  // current engine build does not surface it as a clean MemoryLimitError (it
  // crashes the process under allocation pressure, on the main runtime too:
  // `aike --memory-limit=1m -e "..."`). So this suite only verifies the option
  // is parsed/plumbed and normal runs are unaffected — NOT enforcement.
  const rt = Aike.createRuntime({ memory: 1 << 20 });
  const v = await rt.run(
    "const a = []; for (let i = 0; i < 100; i++) a.push(i); a.length",
  );
  check("run works with memory limit set", v === 100, `got ${v}`);
  rt.close();

  let badMem = false;
  try {
    Aike.createRuntime({ memory: -1 });
  } catch (e) {
    badMem = e instanceof RangeError;
  }
  check("negative memory throws RangeError", badMem);

  let badMemType = false;
  try {
    Aike.createRuntime({ memory: "1m" });
  } catch (e) {
    badMemType = e instanceof TypeError;
  }
  check("non-number memory throws TypeError", badMemType);
}

async function testRunOnce() {
  console.log("\n=== runOnce helper ===");
  async function runOnce(code, opts) {
    const rt = Aike.createRuntime(opts);
    try {
      return await rt.run(code);
    } finally {
      rt.close();
    }
  }
  check("runOnce returns value", (await runOnce("5 * 5")) === 25);
}

// The import policy is the only gate on module access: the capability list
// governs globals only.
async function testImportPolicy() {
  console.log("\n=== import policy ===");

  // A dynamic import inside a child run: the run's completion value must be
  // serializable, so the module namespace is never returned.
  const IMP = (spec) =>
    `(async () => { await import(${JSON.stringify(spec)}); return "imported"; })()`;

  // --- no policy: every import is denied ---
  const bare = Aike.createRuntime();
  for (const name of ["ai", "fs"]) {
    let err = null;
    try {
      await bare.run(IMP(name));
    } catch (e) {
      err = e;
    }
    check(
      `no importPolicy denies import("${name}")`,
      err !== null &&
        err.name === "ReferenceError" &&
        err.message.includes(
          `import of '${name}' is not allowed (no importPolicy)`,
        ),
      err && err.message,
    );
  }
  bare.close();

  // --- a policy that allows everything ---
  const allow = Aike.createRuntime({ importPolicy: () => true });
  const ai = await allow.run(
    `(async () => { const m = await import("ai"); return typeof m.LlmClient + "," + typeof m.DecisionClient + "," + typeof m.EmbeddingClient; })()`,
  );
  check(
    'allowed import("ai") exposes the module\'s classes',
    ai === "function,function,function",
    String(ai),
  );
  // The built-in module is installed once: the second import must resolve to
  // the same module record, not a second one.
  const sameModule = await allow.run(
    `(async () => { const a = await import("ai"); const b = await import("ai"); return a === b; })()`,
  );
  check(
    "importing a built-in twice yields one module",
    sameModule === true,
    String(sameModule),
  );
  allow.close();

  // --- the policy sees the module name ---
  const seen = [];
  const spy = Aike.createRuntime({
    importPolicy: (name) => {
      seen.push(name);
      return true;
    },
  });
  await spy.run(IMP("ai"));
  check("policy sees the module name", seen[0] === "ai", JSON.stringify(seen));
  spy.close();

  // --- the policy sees a file's absolute path ---
  const fixture = new URL("fixtures/import_ok.mjs", import.meta.url).pathname;
  const seenPaths = [];
  const fileSpy = Aike.createRuntime({
    importPolicy: (name) => {
      seenPaths.push(name);
      return true;
    },
  });
  const fixtureValue = await fileSpy.run(
    `(async () => { const m = await import(${JSON.stringify(fixture)}); return m.value; })()`,
  );
  check(
    "a file import reaches the module",
    fixtureValue === "fixture-ok",
    String(fixtureValue),
  );
  check(
    "policy sees a file's absolute path",
    seenPaths.length === 1 && seenPaths[0] === fixture,
    JSON.stringify(seenPaths),
  );
  fileSpy.close();

  // --- a static import is policy-checked too ---
  const staticFixture = new URL("fixtures/static_import.mjs", import.meta.url)
    .pathname;
  const staticSeen = [];
  const staticSpy = Aike.createRuntime({
    importPolicy: (name) => {
      staticSeen.push(name);
      // Allow the outer module only: its *static* import must be refused.
      return name.endsWith("static_import.mjs");
    },
  });
  let staticErr = null;
  try {
    await staticSpy.run(
      `(async () => { const m = await import(${JSON.stringify(staticFixture)}); return m.staticValue; })()`,
    );
  } catch (e) {
    staticErr = e;
  }
  check(
    "a static import is policy-checked as well",
    staticErr !== null &&
      staticErr.name === "ReferenceError" &&
      staticErr.message.includes("is denied by the runtime's importPolicy"),
    staticErr && staticErr.message,
  );
  check(
    "the nested static import is reported by path",
    staticSeen.some((n) => n.endsWith("import_ok.mjs")),
    JSON.stringify(staticSeen),
  );
  staticSpy.close();

  // --- the policy denies ---
  const deny = Aike.createRuntime({ importPolicy: () => false });
  let denied = null;
  try {
    await deny.run(IMP("ai"));
  } catch (e) {
    denied = e;
  }
  check(
    "policy returning false denies the import",
    denied !== null &&
      denied.name === "ReferenceError" &&
      denied.message.includes(
        "import of 'ai' is denied by the runtime's importPolicy",
      ),
    denied && denied.message,
  );
  deny.close();

  // --- the policy throws ---
  const thrower = Aike.createRuntime({
    importPolicy: () => {
      throw new Error("policy exploded");
    },
  });
  let thrown = null;
  try {
    await thrower.run(IMP("ai"));
  } catch (e) {
    thrown = e;
  }
  check(
    "a throwing policy is reported, not swallowed",
    thrown !== null &&
      thrown.name === "ReferenceError" &&
      thrown.message.includes("importPolicy threw") &&
      thrown.message.includes("policy exploded"),
    thrown && thrown.message,
  );
  thrower.close();

  // --- the policy returns a non-boolean ---
  const bad = Aike.createRuntime({ importPolicy: () => 42 });
  let nonBool = null;
  try {
    await bad.run(IMP("ai"));
  } catch (e) {
    nonBool = e;
  }
  check(
    "a non-boolean policy result is reported",
    nonBool !== null &&
      nonBool.name === "ReferenceError" &&
      nonBool.message.includes("importPolicy must return a boolean"),
    nonBool && nonBool.message,
  );
  bad.close();

  // --- a policy must not re-enter the runtime tree ---
  const reentrant = Aike.createRuntime({
    importPolicy: () => {
      Aike.createRuntime();
      return true;
    },
  });
  let reentered = null;
  try {
    await reentrant.run(IMP("ai"));
  } catch (e) {
    reentered = e;
  }
  check(
    "a policy creating a runtime is refused",
    reentered !== null &&
      reentered.name === "TypeError" &&
      reentered.message.includes(
        "importPolicy must not create or close runtimes",
      ),
    reentered && reentered.message,
  );
  reentrant.close();

  // --- closing a runtime from inside the policy is refused too ---
  const victim = Aike.createRuntime();
  const closer = Aike.createRuntime({
    importPolicy: () => {
      victim.close();
      return true;
    },
  });
  let closed = null;
  try {
    await closer.run(IMP("ai"));
  } catch (e) {
    closed = e;
  }
  check(
    "a policy closing a runtime is refused",
    closed !== null &&
      closed.name === "TypeError" &&
      closed.message.includes("importPolicy must not create or close runtimes"),
    closed && closed.message,
  );
  closer.close();
  victim.close();

  // --- the capability list governs globals only ---
  let moduleCap = null;
  try {
    Aike.createRuntime({ global: ["fs"] });
  } catch (e) {
    moduleCap = e;
  }
  check(
    "a module name is not a capability",
    moduleCap !== null &&
      moduleCap.name === "TypeError" &&
      moduleCap.message.includes("unknown host capability"),
    moduleCap && moduleCap.message,
  );

  const consoleOnly = Aike.createRuntime({ global: ["console"] });
  check(
    'global: ["console"] installs console',
    (await consoleOnly.run("typeof console")) === "object",
  );
  let consoleImport = null;
  try {
    await consoleOnly.run(IMP("console"));
  } catch (e) {
    consoleImport = e;
  }
  check(
    'import("console") fails — console is a global, not a module',
    consoleImport !== null,
  );
  consoleOnly.close();

  // --- the root runtime is unaffected ---
  const { LlmClient } = await import("ai");
  check('root import("ai") still works', typeof LlmClient === "function");
}

async function testRootOnly() {
  console.log("\n=== root-only gating ===");
  // Root runtime: these features are available.
  check("root Aike.onSignal available", typeof Aike.onSignal === "function");
  check("root Aike.schedule available", typeof Aike.schedule === "function");
  check("root process.exit available", typeof process.exit === "function");

  // Child granted `Aike`: root-only entries are simply not installed.
  const rt2 = Aike.createRuntime({ global: ["Aike"] });
  for (const name of ["onSignal", "schedule", "ttyGetWinSize", "ttySetRaw"]) {
    check(
      `child Aike.${name} absent`,
      (await rt2.run(`typeof Aike.${name}`)) === "undefined",
    );
  }
  check(
    "child Aike has now()",
    (await rt2.run("typeof Aike.now")) === "function",
  );
  // The abortOnParentExit option is option-level (spawn itself is allowed).
  let spawnErr = null;
  try {
    await rt2.run("Aike.spawn('true', [], { abortOnParentExit: true })");
  } catch (e) {
    spawnErr = e;
  }
  check(
    "child Aike.spawn({abortOnParentExit}) throws TypeError",
    spawnErr !== null && spawnErr.name === "TypeError",
    JSON.stringify(spawnErr),
  );
  rt2.close();

  // Child granted `process`: process-global mutators are simply not installed.
  const rt3 = Aike.createRuntime({ global: ["process"] });
  for (const name of ["exit", "chdir", "kill", "umask"]) {
    check(
      `child process.${name} absent`,
      (await rt3.run(`typeof process.${name}`)) === "undefined",
    );
  }
  check(
    "child process.cwd still present",
    (await rt3.run("typeof process.cwd")) === "function",
  );
  // `title` is read-only in a child: assignment is ignored (no setter).
  const title = await rt3.run("process.title = 'child-title'; process.title");
  check(
    "child process.title is read-only",
    title !== "child-title",
    JSON.stringify(title),
  );
  rt3.close();

  // `localStorage` was removed from the aike globals — there is no root-only
  // capability left to probe here (the `process` block above is the remaining
  // root-only surface).
}

async function main() {
  await testBasic();
  await testIsolation();
  await testMessaging();
  await testErrorsAndTimeout();
  await testAsyncChild();
  await testMemoryLimit();
  await testRootOnly();
  await testRunOnce();
  await testImportPolicy();
  console.log(`\n${"=".repeat(40)}`);
  console.log(`  Pass: ${pass}  Fail: ${fail}`);
  if (fail > 0) {
    console.log("  SOME TESTS FAILED");
    process.exit(1);
  } else {
    console.log("  ALL TESTS PASSED");
  }
}

await main();
