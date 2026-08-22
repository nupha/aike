// spawn + stdio-ipc unified tests — native read primitives (onLine/onData,
// child.shutdown.
//
//   ../aike/target/debug/aike -m tests/spawn/spawn-stdio-ipc.mjs
//
// Covers:
//   A. onLine framing (LF, CRLF, partial-line hold + EOF flush)
//   B. onData pause/resume (unsubscribe preserves unread stream data)
//   C. child.shutdown() stages (eof, term, kill, idempotent, validation)
//   D. conflict handling (first-registration-wins, same-mode replace)

import * as path from "path";

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
const settle = () => Aike.sleep(250);

// ═══════════════════════════════════════════════════════════════════════════
// A. onLine framing — LF split + CRLF stripped + partial held while alive
// ═══════════════════════════════════════════════════════════════════════════

const child1 = Aike.spawn(process.execPath, ["-e", `
  process.stdout.writeSync("hello\\n");
  process.stdout.writeSync("crlf\\r\\n");
  process.stdout.writeSync("partial");
  setInterval(() => {}, 1000);
`], { stdin: "pipe", stdout: "pipe", stderr: "inherit" });

const lines = [];
let eof = false;
child1.stdout.onLine((line) => {
  if (line === null) { eof = true; return; }
  lines.push(line);
});
await Aike.sleep(300);

check("complete lines delivered, CRLF stripped", JSON.stringify(lines) === JSON.stringify(["hello", "crlf"]), JSON.stringify(lines));
check("no premature EOF / partial line held", eof === false);

// Shutdown term stage: timer child survives stdin EOF.
const t0 = Date.now();
const stage1 = await child1.shutdown({ eofTimeoutMs: 200, termTimeoutMs: 2000 });
const dt1 = Date.now() - t0;
check(`shutdown stage "term"`, stage1 === "term", `got ${stage1}`);
check("EOF stage timed out (~200ms) before SIGTERM", dt1 >= 200 && dt1 < 1900, `${dt1}ms`);
await settle();
check("stdout EOF delivered after exit", eof === true);
check("partial line flushed at EOF", JSON.stringify(lines) === JSON.stringify(["hello", "crlf", "partial"]), JSON.stringify(lines));

// ═══════════════════════════════════════════════════════════════════════════
// B. onData pause/resume — unsubscribe preserves unread stream data
// ═══════════════════════════════════════════════════════════════════════════

const paused = Aike.spawn(process.execPath, ["-m", "-e", `
  await new Promise((resolve) => { process.stdin.onData(() => resolve()); });
  process.stdout.writeSync("l1\\nl2\\nl3\\nl4\\n");
`], { stdin: "pipe", stdout: "pipe" });

const got2 = [];
paused.stdout.onLine((l) => { if (l !== null) got2.push(l); });
paused.stdout.onLine(null); // pause BEFORE the child writes
await Aike.sleep(100);
paused.stdin.writeSync("go\n");
paused.stdin.flush();
await Aike.sleep(300);

check("nothing delivered while unsubscribed", got2.length === 0, JSON.stringify(got2));
paused.stdout.onLine((l) => { if (l !== null) got2.push(l); });
await Aike.sleep(300);
check("resubscribe receives data written during pause", JSON.stringify(got2) === JSON.stringify(["l1", "l2", "l3", "l4"]), JSON.stringify(got2));
await paused.shutdown({ eofTimeoutMs: 500 });

// ═══════════════════════════════════════════════════════════════════════════
// C. child.shutdown() stages — eof, kill, idempotent, validation
// ═══════════════════════════════════════════════════════════════════════════

// "kill" stage: child ignores EOF + SIGTERM.
const stubber = Aike.spawn(process.execPath, ["-m", "-e", `
  Aike.onSignal("SIGTERM", () => {});
  setInterval(() => {}, 1000);
`], { stdin: "pipe", stdout: "pipe", stderr: "inherit" });

const t2 = Date.now();
const stageK = await stubber.shutdown({ eofTimeoutMs: 200, termTimeoutMs: 300, killTimeoutMs: 2000 });
const dtK = Date.now() - t2;
check(`shutdown stage "kill"`, stageK === "kill", `got ${stageK}`);
check("kill stage within bounds", dtK >= 200 && dtK < 2500, `${dtK}ms`);

// Idempotent: second close resolves immediately.
const stageK2 = await stubber.shutdown();
check("second shutdown is idempotent", stageK2 === "already_closed");

// Strict validation: negative timeout → RangeError.
const bad = Aike.spawn("/bin/sleep", ["0"], { stdout: "ignore" });
let ve = false;
try { await bad.shutdown({ eofTimeoutMs: -5 }); } catch (e) { ve = e instanceof RangeError; }
check("negative timeout rejected as RangeError", ve);

// ═══════════════════════════════════════════════════════════════════════════
// D. conflict handling — first-registration-wins, same-mode replace
// ═══════════════════════════════════════════════════════════════════════════

const echo = Aike.spawn(process.execPath, ["-m", "-e", `
  const dec = new TextDecoder();
  let buf = "";
  process.stdin.onData((chunk) => {
    if (chunk === null) { process.exit(0); }
    buf += dec.decode(chunk, { stream: true });
    let nl;
    while ((nl = buf.indexOf("\\n")) >= 0) {
      const line = buf.slice(0, nl).replace(/\\r$/, ""); buf = buf.slice(nl + 1);
      process.stdout.writeSync("got:" + line + "\\n");
    }
  });
`], { stdin: "pipe", stdout: "pipe", stderr: "inherit" });

const got = [];
echo.stdout.onLine((l) => { if (l !== null) got.push(l); });
echo.stdin.writeSync("abc\ndef\r\n");
echo.stdin.flush();
await Aike.sleep(300);
check("echo roundtrip with CRLF", JSON.stringify(got) === JSON.stringify(["got:abc", "got:def"]), JSON.stringify(got));

// Conflicting mode ignored with warning (first-registration-wins).
let dataCbFired = false;
echo.stdout.onData(() => { dataCbFired = true; });
echo.stdin.writeSync("more\n");
echo.stdin.flush();
await Aike.sleep(300);
check("onLine still receives after ignored onData", JSON.stringify(got) === JSON.stringify(["got:abc", "got:def", "got:more"]), JSON.stringify(got));
check("ignored onData callback never fires", dataCbFired === false);

// Same-mode re-register replaces callback.
let last = "";
echo.stdout.onLine((l) => { if (l !== null) last = l; });
echo.stdin.writeSync("final\n");
echo.stdin.flush();
await Aike.sleep(300);
check("same-mode re-register replaces callback", last === "got:final", `last=${last}`);
check("old collector stopped at replacement", JSON.stringify(got) === JSON.stringify(["got:abc", "got:def", "got:more"]));

const t3 = Date.now();
const stageE = await echo.shutdown({ eofTimeoutMs: 1000 });
check(`shutdown stage "eof" (child exits on stdin close)`, stageE === "eof", `got ${stageE}`);
