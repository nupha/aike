// onLine / shutdown tests — unified read subscription (single pump per handle)
// and the child.shutdown() graceful ladder.
//
//   ../aike/target/debug/aike -m tests/spawn/oneline-shutdown.mjs
//
// Covers: LF splitting, CRLF stripping, partial-line hold + EOF flush,
// pause/resume (unsubscribe preserves unread stream data), first-registration-
// wins mode conflict (later mode warns + ignored), shutdown stages
// ("term"/"eof"), idempotency, strict option validation, mode switch after
// unsubscribe.

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

// ── 1) onLine framing: LF split + CRLF stripped + partial held while alive ──
const child = Aike.spawn(process.execPath, ["-e", `
  process.stdout.writeSync("hello\\n");
  process.stdout.writeSync("crlf\\r\\n");
  process.stdout.writeSync("partial");
  setInterval(() => {}, 1000); // survive stdin EOF
`], { stdin: "pipe", stdout: "pipe", stderr: "inherit" });

const lines = [];
let eof = false;
child.stdout.onLine((line) => {
  if (line === null) { eof = true; return; }
  lines.push(line);
});
await Aike.sleep(300);

check("complete lines delivered, CRLF stripped", JSON.stringify(lines) === JSON.stringify(["hello", "crlf"]), JSON.stringify(lines));
check("no premature EOF / partial line held", eof === false);

// ── 2) shutdown(): stage "term" — timer child survives stdin EOF ──
const t0 = Date.now();
const stage = await child.shutdown({ eofTimeoutMs: 200, termTimeoutMs: 2000 });
const dt = Date.now() - t0;
check(`shutdown stage "term"`, stage === "term", `got ${stage}`);
check("EOF stage timed out (~200ms) before SIGTERM", dt >= 200 && dt < 1900, `${dt}ms`);
await settle();
check("stdout EOF delivered after exit", eof === true);
check("partial line flushed at EOF", JSON.stringify(lines) === JSON.stringify(["hello", "crlf", "partial"]), JSON.stringify(lines));

// ── 3) pause preserves UNREAD stream data across unsubscribe/resubscribe ──
const pausedChild = Aike.spawn(process.execPath, ["-m", "-e", `
  await new Promise((resolve) => { process.stdin.onData(() => resolve()); });
  process.stdout.writeSync("l1\\nl2\\nl3\\nl4\\n");
`], { stdin: "pipe", stdout: "pipe" });

const got2 = [];
pausedChild.stdout.onLine((l) => { if (l !== null) got2.push(l); });
pausedChild.stdout.onLine(null); // pause BEFORE the child writes
await Aike.sleep(100);
pausedChild.stdin.writeSync("go\n");
pausedChild.stdin.flush();
await Aike.sleep(300); // child wrote everything while nobody listened

check("nothing delivered while unsubscribed", got2.length === 0, JSON.stringify(got2));
let eof2 = false;
pausedChild.stdout.onLine((l) => { if (l === null) { eof2 = true; return; } got2.push(l); });
await Aike.sleep(300);
check("resubscribe receives data written during pause", JSON.stringify(got2) === JSON.stringify(["l1", "l2", "l3", "l4"]), JSON.stringify(got2));
void eof2;
await pausedChild.shutdown({ eofTimeoutMs: 500 });

// ── 4) echo child: CRLF roundtrip, conflict handling, "eof" stage ──
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

// First registration wins: conflicting later mode is ignored with a warning.
let dataCbFired = false;
echo.stdout.onData(() => { dataCbFired = true; });
echo.stdin.writeSync("more\n");
echo.stdin.flush();
await Aike.sleep(300);
check("onLine still receives lines after ignored onData", JSON.stringify(got) === JSON.stringify(["got:abc", "got:def", "got:more"]), JSON.stringify(got));
check("ignored onData callback never fires", dataCbFired === false);

// Same-mode re-register silently REPLACES the callback.
let last = "";
echo.stdout.onLine((l) => { if (l !== null) last = l; });
echo.stdin.writeSync("final\n");
echo.stdin.flush();
await Aike.sleep(300);
check("same-mode re-register replaces callback", last === "got:final", `last=${last}`);
check("old collector stopped at replacement", JSON.stringify(got) === JSON.stringify(["got:abc", "got:def", "got:more"]));

const t1 = Date.now();
const stage2 = await echo.shutdown({ eofTimeoutMs: 1000 });
check(`shutdown stage "eof" (child exits on stdin close)`, stage2 === "eof", `got ${stage2}`);

// ── 5) switch mode after explicit unsubscribe: allowed, unread data kept ──
const sw = Aike.spawn(process.execPath, ["-m", "-e", `
  await new Promise((resolve) => { process.stdin.onData(() => resolve()); });
  process.stdout.writeSync("raw-bytes");
`], { stdin: "pipe", stdout: "pipe" });
sw.stdout.onData((c) => { void c; });
sw.stdout.onData(null); // unsubscribe → pump paused
await Aike.sleep(50);
sw.stdin.writeSync("go\n");
sw.stdin.flush();
await Aike.sleep(150); // child wrote while paused

let lineTxt = "";
let eofSw = false;
sw.stdout.onLine((l) => { if (l === null) { eofSw = true; return; } lineTxt += l; });
await sw.shutdown({ eofTimeoutMs: 500 }); // EOF flushes the buffered partial line
await settle();
check("mode switch after null delivers buffered bytes as one line", lineTxt === "raw-bytes", `got ${JSON.stringify(lineTxt)}`);
check("EOF delivered after switch+shutdown", eofSw === true);

// ── 6) strict option validation ──
const bad = Aike.spawn("/bin/sleep", ["0"], { stdout: "ignore" });
let ve = false;
try { await bad.shutdown({ eofTimeoutMs: -5 }); } catch (e) { ve = e instanceof RangeError; }
check("negative timeout rejected as RangeError", ve);

console.log(`\n=== oneline-shutdown: ${pass} passed, ${fail} failed ===`);
if (fail > 0) process.exit(1);
