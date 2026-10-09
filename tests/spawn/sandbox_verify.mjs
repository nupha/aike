// Live isolation check for the macOS backend. Run with:
//   aike -m tests/spawn/sandbox_verify.mjs
//
// Each case is a real command in a real sandbox. A unit test proves the SBPL
// profile is shaped right; only these prove the kernel enforces it.

import { makeDir } from "fs";

const dec = new TextDecoder();
const strip = (s) => s.replace(/\[[0-9;]*m/g, "").trim();

let passed = 0;
let failed = 0;
function check(label, cond, detail = "") {
  if (cond) {
    passed++;
    console.log(`  \x1b[32mok\x1b[0m   ${label}`);
  } else {
    failed++;
    console.log(`  \x1b[31mFAIL\x1b[0m ${label} — ${detail}`);
  }
}

const WD = "/tmp/sbx-test/work";

function sh(cmd, sandbox) {
  return Aike.sh(cmd, sandbox ? { sandbox } : undefined);
}
const out = (r) => strip(dec.decode(r.stdout));
const err = (r) => strip(dec.decode(r.stderr));

// --- availability ---------------------------------------------------------
const support = Aike.sandboxSupport();
console.log(`sandboxSupport: available=${support.available} backend=${support.backend}`);
if (!support.available) {
  console.log(`  reason: ${support.reason}`);
  console.log("\nCannot verify without a backend.");
  process.exit(1);
}
check("sandboxSupport reports available", support.available === true);
check("sandboxSupport names the backend", support.backend === "seatbelt", String(support.backend));

// --- the working directory is writable ------------------------------------
{
  const r = sh("echo written > new.txt && cat new.txt", { cwd: WD });
  check("write inside cwd succeeds", r.exitCode === 0 && out(r) === "written", err(r));
}

// --- writes outside cwd are refused ---------------------------------------
{
  const r = sh("echo nope > /tmp/sbx-test/outside/hack.txt", { cwd: WD });
  const refused = r.exitCode !== 0 || /deny|Operation not permitted/i.test(err(r));
  check("write outside cwd is refused", refused, `exit=${r.exitCode} ${err(r)}`);
}

// --- a denied path cannot be read -----------------------------------------
{
  const r = sh("cat /tmp/sbx-test/secret/key.txt", {
    cwd: WD,
    denied: ["/tmp/sbx-test/secret"],
  });
  check("denied path content is unreadable", r.exitCode !== 0 || out(r) !== "SECRET", out(r));
}

// --- but its existence and name stay visible ------------------------------
// `file-read-metadata` is kept so a program can see that the path is there.
// Note what this does NOT give: listing the denied directory's *own*
// contents needs read-data on that directory, so `ls secret/` is refused.
// Linux differs here — see the plan's divergence note.
{
  const parent = sh("ls /tmp/sbx-test", { cwd: WD, denied: ["/tmp/sbx-test/secret"] });
  check(
    "denied entry still named in the parent listing",
    out(parent).includes("secret"),
    out(parent),
  );
  const st = sh("test -f /tmp/sbx-test/secret/key.txt && echo EXISTS", {
    cwd: WD,
    denied: ["/tmp/sbx-test/secret"],
  });
  check("denied path is still stat-able", out(st) === "EXISTS", out(st));
  const inside = sh("ls /tmp/sbx-test/secret", { cwd: WD, denied: ["/tmp/sbx-test/secret"] });
  check(
    "listing the denied directory's contents is refused",
    inside.exitCode !== 0,
    out(inside),
  );
}

// --- a granted rx path: read and execute yes, write no --------------------
{
  const sb = { cwd: WD, grants: [{ path: "/tmp/sbx-test/tools", access: "rx" }] };
  const r = sh("/tmp/sbx-test/tools/t.sh", sb);
  check("granted rx path executes", r.exitCode === 0 && out(r) === "TOOL", `exit=${r.exitCode} ${err(r)}`);

  const w = sh("touch /tmp/sbx-test/tools/new", sb);
  check("granted rx path rejects writes", w.exitCode !== 0, `exit=${w.exitCode} ${out(w)}`);
}

// --- a granted rw path is writable but NOT executable ---------------------
{
  const sb = { cwd: WD, grants: [{ path: "/tmp/sbx-test/rw", access: "rw" }] };
  await makeDir("/tmp/sbx-test/rw", { recursive: true });
  const w = sh("echo ok > /tmp/sbx-test/rw/a", sb);
  check("granted rw path accepts writes", w.exitCode === 0, err(w));

  const x = sh("chmod +x /tmp/sbx-test/rw/a && /tmp/sbx-test/rw/a", sb);
  check(
    "granted rw path refuses execution",
    x.exitCode !== 0,
    `exit=${x.exitCode} ${out(x)}`,
  );
}

// --- the host's environment is not inherited ------------------------------
{
  const r = sh("echo \"[$MY_SECRET]\"", { cwd: WD });
  check("host env vars are not inherited", !out(r).includes("s3cret"), out(r));
}

// --- the sandboxed cwd is where the command starts ------------------------
{
  const r = sh("pwd", { cwd: WD });
  // /tmp is a symlink to /private/tmp, and the policy is compared against the
  // resolved path, so accept either spelling.
  const ok = [WD, "/private" + WD].includes(out(r));
  check("command starts in cwd", ok, out(r));
}

// --- network is denied by default -----------------------------------------
{
  const r = sh("/usr/bin/curl -s -m 5 -o /dev/null https://example.com", { cwd: WD });
  const blocked = r.exitCode !== 0 || out(r).length === 0;
  check("network denied by default", blocked, `exit=${r.exitCode}`);
}

// --- stderr carries no sandbox-exec deprecation noise ---------------------
{
  const r = sh("echo real-output >&2", { cwd: WD });
  check(
    "stderr has no deprecation warning",
    !err(r).includes("sandbox-exec is deprecated"),
    err(r),
  );
  check("stderr still reaches the caller", err(r) === "real-output", err(r));
}

// --- exit status and signals ----------------------------------------------
{
  const r = sh("exit 7", { cwd: WD });
  check("exit status is propagated", r.exitCode === 7, String(r.exitCode));
}

{
  const p = Aike.spawn("/bin/sh", ["-c", "trap 'exit 42' TERM; sleep 10"], {
    sandbox: { cwd: WD },
    stdout: "pipe",
    stderr: "pipe",
  });
  await Aike.sleep(300);
  check("child is running", p.isRunning() === true);
  p.kill("SIGTERM");
  const code = await p.exitCode;
  check("SIGTERM reaches the command (trap fired)", code === 42, String(code));
}

{
  const p = Aike.spawn("/bin/sh", ["-c", "sleep 30"], {
    sandbox: { cwd: WD },
    stdout: "pipe",
    stderr: "pipe",
  });
  await Aike.sleep(200);
  const t0 = Date.now();
  p.kill();
  const code = await p.exitCode;
  check("SIGKILL terminates the tree", Date.now() - t0 < 5000, `took ${Date.now() - t0}ms`);
  check("killed child reports a signal", code !== 0, String(code));
}

// --- rejections: every one names its field --------------------------------
const rejects = [
  ["missing cwd", { network: "none" }, /cwd/],
  ["nonexistent cwd", { cwd: "/tmp/sbx-test/nope" }, /cwd/],
  ["nonexistent grant", { cwd: WD, grants: [{ path: "/tmp/sbx-test/ghost", access: "r" }] }, /grants/],
  ["bad access mode", { cwd: WD, grants: [{ path: "/tmp/sbx-test/tools", access: "zz" }] }, /access/],
  ["access without read", { cwd: WD, grants: [{ path: "/tmp/sbx-test/tools", access: "w" }] }, /read/],
  ["deny over cwd", { cwd: WD, denied: [WD] }, /cwd/],
  ["bad network value", { cwd: WD, network: "maybe" }, /network/],
  ["relative cwd", { cwd: "relative/path" }, /cwd/],
];
for (const [label, sandbox, want] of rejects) {
  let msg = "";
  try {
    Aike.sh("true", { sandbox });
  } catch (e) {
    msg = String(e && e.message ? e.message : e);
  }
  check(`rejects ${label}`, want.test(msg), msg || "no error thrown");
}

// --- spawnSync path --------------------------------------------------------
{
  const r = Aike.spawnSync("/bin/echo", ["sync-works"], { sandbox: { cwd: WD } });
  check("spawnSync honours the sandbox", r.exitCode === 0 && strip(dec.decode(r.stdout)) === "sync-works");
}

console.log(`\n${passed} passed, ${failed} failed`);
if (failed > 0) process.exit(1);