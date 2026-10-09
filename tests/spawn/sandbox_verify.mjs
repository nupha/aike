// Live isolation check for the macOS backend. Run with:
//   aike -m tests/spawn/sandbox_verify.mjs
//
// Each case is a real command in a real sandbox. A unit test proves the SBPL
// profile is shaped right; only these prove the kernel enforces it.

import { makeDir, writeText } from "fs";

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

const ROOT = "/tmp/sbx-test";
const WD = `${ROOT}/work`;

// The sandbox takes its working directory from the spawn options, so `cwd`
// rides alongside `sandbox` rather than inside it.
function sh(cmd, sandbox) {
  if (!sandbox) return Aike.sh(cmd);
  const { cwd = WD, ...rest } = sandbox;
  return Aike.sh(cmd, { cwd, sandbox: rest });
}
const out = (r) => strip(dec.decode(r.stdout));
const err = (r) => strip(dec.decode(r.stderr));

// Fixtures, rebuilt every run: a file left behind by a previous run would make
// a denial look like an allow.
for (const d of [
  "work",
  "outside",
  "secret",
  "tools",
  "grants",
  "rw",
  "rwdir",
  "target",
]) {
  await makeDir(`${ROOT}/${d}`, { recursive: true });
}
await writeText(`${ROOT}/secret/key.txt`, "SECRET");
await writeText(`${ROOT}/outside/o.txt`, "OUTSIDE");
await writeText(`${ROOT}/tools/t.sh`, "#!/bin/sh\necho TOOL\n");
await writeText(`${ROOT}/grants/readable.txt`, "READABLE");
await writeText(`${ROOT}/grants/plain.txt`, "PLAIN");
// writeText creates the file without an execute bit, and the rx grant below
// claims to allow running it.
Aike.sh(`chmod +x ${ROOT}/tools/t.sh`);

// --- availability ---------------------------------------------------------
const support = Aike.sandboxSupport();
console.log(
  `sandboxSupport: available=${support.available} backend=${support.backend}`,
);
if (!support.available) {
  console.log(`  reason: ${support.reason}`);
  console.log("\nCannot verify without a backend.");
  process.exit(1);
}
check("sandboxSupport reports available", support.available === true);
check(
  "sandboxSupport names the backend",
  support.backend === "seatbelt",
  String(support.backend),
);

// --- the working directory is writable ------------------------------------
{
  const r = sh("echo written > new.txt && cat new.txt", { cwd: WD });
  check(
    "write inside cwd succeeds",
    r.exitCode === 0 && out(r) === "written",
    err(r),
  );
}

// --- writes outside cwd are refused ---------------------------------------
{
  const r = sh(`echo nope > ${ROOT}/outside/hack.txt`, { cwd: WD });
  const refused =
    r.exitCode !== 0 || /deny|Operation not permitted/i.test(err(r));
  check(
    "write outside cwd is refused",
    refused,
    `exit=${r.exitCode} ${err(r)}`,
  );
}

// --- a denied path cannot be read -----------------------------------------
{
  const r = sh(`cat ${ROOT}/secret/key.txt`, {
    cwd: WD,
    denied: [`${ROOT}/secret`],
  });
  check(
    "denied path content is unreadable",
    r.exitCode !== 0 || out(r) !== "SECRET",
    out(r),
  );
}

// --- but its existence and name stay visible ------------------------------
// `file-read-metadata` is kept so a program can see that the path is there.
// Note what this does NOT give: listing the denied directory's *own*
// contents needs read-data on that directory, so `ls secret/` is refused.
// Linux differs here — see the plan's divergence note.
{
  const parent = sh(`ls ${ROOT}`, { cwd: WD, denied: [`${ROOT}/secret`] });
  check(
    "denied entry still named in the parent listing",
    out(parent).includes("secret"),
    out(parent),
  );
  const st = sh(`test -f ${ROOT}/secret/key.txt && echo EXISTS`, {
    cwd: WD,
    denied: [`${ROOT}/secret`],
  });
  check("denied path is still stat-able", out(st) === "EXISTS", out(st));
  const inside = sh(`ls ${ROOT}/secret`, {
    cwd: WD,
    denied: [`${ROOT}/secret`],
  });
  check(
    "listing the denied directory's contents is refused",
    inside.exitCode !== 0,
    out(inside),
  );
}

// --- a granted rx path: read and execute yes, write no --------------------
{
  const sb = { cwd: WD, grants: [{ path: `${ROOT}/tools`, access: "rx" }] };
  const r = sh(`${ROOT}/tools/t.sh`, sb);
  check(
    "granted rx path executes",
    r.exitCode === 0 && out(r) === "TOOL",
    `exit=${r.exitCode} ${err(r)}`,
  );

  const w = sh(`touch ${ROOT}/tools/new`, sb);
  check(
    "granted rx path rejects writes",
    w.exitCode !== 0,
    `exit=${w.exitCode} ${out(w)}`,
  );
}

// --- a granted rw path is writable but NOT executable ---------------------
{
  const sb = { cwd: WD, grants: [{ path: `${ROOT}/rw`, access: "rw" }] };
  await makeDir(`${ROOT}/rw`, { recursive: true });
  const w = sh(`echo ok > ${ROOT}/rw/a`, sb);
  check("granted rw path accepts writes", w.exitCode === 0, err(w));

  const x = sh(`chmod +x ${ROOT}/rw/a && ${ROOT}/rw/a`, sb);
  check(
    "granted rw path refuses execution",
    x.exitCode !== 0,
    `exit=${x.exitCode} ${out(x)}`,
  );
}

// --- the host's environment is not inherited ------------------------------
{
  const r = sh('echo "[$MY_SECRET]"', { cwd: WD });
  check("host env vars are not inherited", !out(r).includes("s3cret"), out(r));
}

// --- the sandboxed cwd is where the command starts ------------------------
// `pwd` alone only proves what the shell reports; a relative write proves the
// process's actual working directory, which is the thing that matters.
{
  Aike.sh(`rm -f ${WD}/relative-proof.txt`);
  const w = sh("echo proof > relative-proof.txt", { cwd: WD });
  const landed = Aike.sh(`test -f ${WD}/relative-proof.txt && echo YES || echo NO`);
  check("a relative write lands in cwd", w.exitCode === 0 && strip(dec.decode(landed.stdout)) === "YES", err(w));

  const r = sh("pwd", { cwd: WD });
  check("pwd reports cwd", out(r).endsWith(`${ROOT}/work`), out(r));
}

// cwd is a starting point, not the boundary: cd'ing out and writing there is
// still refused, because the grants are what constrain the process.
{
  await makeDir(`${ROOT}/elsewhere`, { recursive: true });
  const r = sh(`cd ${ROOT}/elsewhere && touch escaped.txt`, { cwd: WD });
  check("cd out of cwd does not widen write access", r.exitCode !== 0, `exit=${r.exitCode}`);
  const leaked = Aike.sh(`test -f ${ROOT}/elsewhere/escaped.txt && echo LEAKED || echo CLEAN`);
  check("nothing landed after cd'ing out", strip(dec.decode(leaked.stdout)) === "CLEAN");

  const dotdot = sh(`echo x > ../outside/via_dotdot.txt`, { cwd: WD });
  check("a relative .. write out of cwd is refused", dotdot.exitCode !== 0, `exit=${dotdot.exitCode}`);
}

// With no `cwd`, the sandbox runs in the current directory — the same place a
// plain spawn would, and the one place it may write.
//
// `pwd -P` because the sandbox does not inherit `$PWD`, so the shell's logical
// path is unavailable and only the physical one is meaningful here. Compare
// against the host's own physical cwd, which is what `current_dir()` resolves.
{
  const here = strip(dec.decode(Aike.sh("/bin/pwd -P").stdout));
  const inSandbox = strip(dec.decode(Aike.sh("/bin/pwd -P", { sandbox: {} }).stdout));
  check("a sandbox with no cwd starts in the current directory", inSandbox === here, `host=${here} sandbox=${inSandbox}`);

  // A relative write proves it is really the process's directory, not just
  // what `pwd` reports.
  Aike.sh("rm -f aike-cwd-default.txt");
  const w = Aike.sh("echo probe > aike-cwd-default.txt", { sandbox: {} });
  const landed = Aike.sh("test -f aike-cwd-default.txt && echo YES || echo NO");
  check("and a relative write lands there", w.exitCode === 0 && strip(dec.decode(landed.stdout)) === "YES", err(w));
  Aike.sh("rm -f aike-cwd-default.txt");
}

// --- network is denied by default -----------------------------------------
{
  const r = sh("/usr/bin/curl -s -m 5 -o /dev/null https://example.com", {
    cwd: WD,
  });
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
    cwd: WD,
    sandbox: {},
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
    cwd: WD,
    sandbox: {},
    stdout: "pipe",
    stderr: "pipe",
  });
  await Aike.sleep(200);
  const t0 = Date.now();
  p.kill();
  const code = await p.exitCode;
  check(
    "SIGKILL terminates the tree",
    Date.now() - t0 < 5000,
    `took ${Date.now() - t0}ms`,
  );
  check("killed child reports a signal", code !== 0, String(code));
}

// --- rejections: every one names its field --------------------------------
// Each entry is the whole spawn options object: `cwd` lives at the top level
// now, so a rejection table keyed on it has to say both.
const rejects = [
  ["missing cwd (a relative one)", { cwd: "relative/path", sandbox: {} }, /cwd/],
  ["nonexistent cwd", { cwd: `${ROOT}/nope`, sandbox: {} }, /cwd/],
  [
    "cwd that is a file",
    { cwd: `${ROOT}/grants/plain.txt`, sandbox: {} },
    /cwd/,
  ],
  [
    "nonexistent grant",
    { cwd: WD, sandbox: { grants: [{ path: `${ROOT}/ghost`, access: "r" }] } },
    /grants/,
  ],
  [
    "bad access mode",
    { cwd: WD, sandbox: { grants: [{ path: `${ROOT}/tools`, access: "zz" }] } },
    /access/,
  ],
  [
    "access without read",
    { cwd: WD, sandbox: { grants: [{ path: `${ROOT}/tools`, access: "w" }] } },
    /read/,
  ],
  ["deny over cwd", { cwd: WD, sandbox: { denied: [WD] } }, /cwd/],
  ["bad network value", { cwd: WD, sandbox: { network: "maybe" } }, /network/],
  [
    "nonexistent denied",
    { cwd: WD, sandbox: { denied: [`${ROOT}/ghost`] } },
    /denied/,
  ],
  [
    "denied over a grant",
    {
      cwd: WD,
      sandbox: {
        grants: [{ path: `${ROOT}/tools`, access: "rx" }],
        denied: [`${ROOT}/tools`],
      },
    },
    /denied/,
  ],
  [
    "rw without x on a file",
    {
      cwd: WD,
      sandbox: { grants: [{ path: `${ROOT}/grants/plain.txt`, access: "rw" }] },
    },
    /rwx/,
  ],
  [
    "the {allow} form",
    { cwd: WD, sandbox: { network: { allow: ["a.com"] } } },
    /not implemented/,
  ],
  ["sandbox not an object", { cwd: WD, sandbox: [] }, /must be an object/],
];
for (const [label, opts, want] of rejects) {
  let msg = "";
  try {
    Aike.sh("true", opts);
  } catch (e) {
    msg = String(e && e.message ? e.message : e);
  }
  check(`rejects ${label}`, want.test(msg), msg || "no error thrown");
}

// --- spawnSync path --------------------------------------------------------
{
  const r = Aike.spawnSync("/bin/echo", ["sync-works"], {
    cwd: WD,
    sandbox: {},
  });
  check(
    "spawnSync honours the sandbox",
    r.exitCode === 0 && strip(dec.decode(r.stdout)) === "sync-works",
  );
}

// --- grants other than rx / rw --------------------------------------------
{
  const sb = { cwd: WD, grants: [{ path: `${ROOT}/grants`, access: "r" }] };
  const r = sh(`cat ${ROOT}/grants/readable.txt`, sb);
  check(
    "grant r allows reading",
    r.exitCode === 0 && out(r) === "READABLE",
    `${err(r)}`,
  );

  const w = sh(`touch ${ROOT}/grants/new`, sb);
  check("grant r refuses writing", w.exitCode !== 0, `exit=${w.exitCode}`);
}

// A grant naming a file rather than a directory uses a different Seatbelt
// filter (`literal`), which is a distinct code path.
{
  const sb = {
    cwd: WD,
    grants: [{ path: `${ROOT}/grants/plain.txt`, access: "r" }],
  };
  const r = sh(`cat ${ROOT}/grants/plain.txt`, sb);
  check(
    "a grant on a file is readable",
    r.exitCode === 0 && out(r) === "PLAIN",
    err(r),
  );
}

{
  const sb = {
    cwd: WD,
    grants: [
      { path: `${ROOT}/grants`, access: "r" },
      { path: `${ROOT}/tools`, access: "rx" },
    ],
  };
  const r = sh(`cat ${ROOT}/grants/readable.txt && ${ROOT}/tools/t.sh`, sb);
  check(
    "two grants both apply",
    r.exitCode === 0 && out(r).includes("READABLE") && out(r).includes("TOOL"),
    `${err(r)}`,
  );
}

// A denied path may be a file, not only a directory.
{
  const r = sh(`cat ${ROOT}/grants/plain.txt`, {
    cwd: WD,
    denied: [`${ROOT}/grants/plain.txt`],
  });
  check(
    "denied file is unreadable",
    r.exitCode !== 0 || out(r) !== "PLAIN",
    out(r),
  );
}

// --- symlinks must not route around a rule --------------------------------
{
  // A link inside cwd pointing at a denied path.
  Aike.sh(`ln -sfn ${ROOT}/secret ${ROOT}/work/link_to_secret`);
  const r = sh(`cat ${ROOT}/work/link_to_secret/key.txt`, {
    cwd: WD,
    denied: [`${ROOT}/secret`],
  });
  check(
    "a symlink to a denied path is still denied",
    r.exitCode !== 0 || out(r) !== "SECRET",
    out(r),
  );

  // A link inside a writable grant pointing at a denied path: writing through
  // it must not land.
  Aike.sh(`ln -sfn ${ROOT}/target ${ROOT}/rwdir/link_out`);
  const w = sh(`echo pwned > ${ROOT}/rwdir/link_out/via_link`, {
    cwd: WD,
    grants: [{ path: `${ROOT}/rwdir`, access: "rw" }],
    denied: [`${ROOT}/target`],
  });
  check(
    "a symlink cannot be used to write into a denied path",
    w.exitCode !== 0,
    `exit=${w.exitCode}`,
  );
}

// --- env and stdin --------------------------------------------------------
{
  const r = Aike.sh(`echo "[$AIKE_PROBE]"`, {
    cwd: WD,
    sandbox: {},
    env: { AIKE_PROBE: "forwarded" },
  });
  check(
    "caller env reaches the sandboxed command",
    out(r) === "[forwarded]",
    out(r),
  );
}

{
  const r = Aike.sh("cat", { cwd: WD, sandbox: {}, input: "piped-in" });
  check("stdin reaches the sandboxed command", out(r) === "piped-in", out(r));
}

// --- timeout --------------------------------------------------------------
{
  const r = Aike.sh("sleep 30", { sandbox: { cwd: WD }, timeout: 500 });
  check(
    "timeout kills the sandboxed tree",
    r.killedByTimeout === true,
    `exit=${r.exitCode} killed=${r.killedByTimeout}`,
  );
}

{
  let threw = false;
  try {
    Aike.sh("exit 3", { sandbox: { cwd: WD }, check: true });
  } catch {
    threw = true;
  }
  check("check:true throws for a sandboxed non-zero exit", threw);
}

// --- the async Subprocess surface -----------------------------------------
{
  const p = Aike.spawn(
    "/bin/sh",
    ["-c", "echo streaming; sleep 0.2; echo more"],
    {
      cwd: WD,
    sandbox: {},
      stdout: "pipe",
      stderr: "pipe",
    },
  );
  let got = "";
  for (;;) {
    const chunk = await p.stdout.read();
    if (!chunk || chunk.length === 0) break;
    got += dec.decode(chunk);
  }
  await p.exitCode;
  check(
    "sandboxed stdout streams to the caller",
    strip(got).includes("streaming") && strip(got).includes("more"),
    strip(got),
  );
}

{
  const p = Aike.spawn("/bin/sh", ["-c", "trap 'exit 0' TERM; cat"], {
    cwd: WD,
    sandbox: {},
    stdout: "pipe",
    stderr: "pipe",
  });
  await Aike.sleep(200);
  const stage = await p.shutdown({ eofTimeoutMs: 800, termTimeoutMs: 800 });
  check(
    "shutdown() ladder runs under the sandbox",
    stage === "eof" || stage === "term",
    String(stage),
  );
}

// A killed sandbox must take its whole tree: a surviving grandchild would keep
// running on the host with the sandbox gone.
{
  const p = Aike.spawn("/bin/sh", ["-c", "sleep 313 & echo up; wait"], {
    cwd: WD,
    sandbox: {},
    stdout: "pipe",
    stderr: "pipe",
  });
  await Aike.sleep(400);
  p.kill();
  await p.exitCode;
  await Aike.sleep(300);
  const survivors = sh("pgrep -f 'sleep 313' | wc -l", { cwd: WD });
  check(
    "a killed sandbox takes its grandchildren",
    strip(dec.decode(survivors.stdout)) === "0",
    strip(dec.decode(survivors.stdout)),
  );
}

// --- sandboxSupport failure shape -----------------------------------------
{
  const s = Aike.sandboxSupport();
  check(
    "sandboxSupport.reason is null when available",
    s.available ? s.reason === null : typeof s.reason === "string",
  );
}

console.log(`\n${passed} passed, ${failed} failed`);
if (failed > 0) process.exit(1);
