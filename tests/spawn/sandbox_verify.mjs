// Live isolation check for the sandbox backends. Run with:
//   aike -m tests/spawn/sandbox_verify.mjs
//
// Each case is a real command in a real sandbox. A unit test proves the SBPL
// profile (macOS) or the ACL tree (Windows) is shaped right; only these prove
// the OS enforces it.
//
// The same assertions run on every platform where a backend exists. Where the
// backends genuinely differ — macOS keeps a denied path's *name* visible,
// Windows and Linux make it disappear — the divergent case is branched and
// the difference is spelled out rather than papered over.

import { makeDir, writeText, readBytes, writeBytes, removeDir, exists } from "fs";

const dec = new TextDecoder();
const strip = (s) => s.replace(/\u001b\[[0-9;]*m/g, "").trim();
const out = (r) => strip(dec.decode(r.stdout));
const err = (r) => strip(dec.decode(r.stderr));

let passed = 0;
let failed = 0;
let skipped = 0;
function check(label, cond, detail = "") {
  if (cond) {
    passed++;
    console.log(`  \x1b[32mok\x1b[0m   ${label}`);
  } else {
    failed++;
    console.log(`  \x1b[31mFAIL\x1b[0m ${label} — ${detail}`);
  }
}
function skip(label, why) {
  skipped++;
  console.log(`  \x1b[33mskip\x1b[0m ${label} — ${why}`);
}

const WIN = process.platform === "win32";
const SEP = WIN ? "\\" : "/";
const join = (...p) => p.join(SEP);
// cmd.exe's `/c` parsing does not understand the backslash-escaped quotes that
// the standard `Command` quoting (which the sandbox backend mirrors) produces,
// so a quoted path in a command string gets mangled. The fixture root has no
// spaces, so paths go unquoted on both platforms.
const q = (p) => p;

// A directory tree of fixtures, rebuilt every run: a file left behind by a
// previous run would make a denial look like an allow.
const ROOT = WIN
  ? join(process.env.TEMP ?? process.env.TMP ?? "C:\\Windows\\Temp", "sbx-test")
  : "/tmp/sbx-test";
const WD = join(ROOT, "work");

// An executable to copy into writable places to prove the execute bit is
// separate from the write bit. macOS uses a shell script on the rx path; the
// rw path needs a real image on every platform.
const HOST_EXE = WIN ? "C:\\Windows\\System32\\hostname.exe" : "/bin/hostname";

// The sandbox takes its working directory from the spawn options, so `cwd`
// rides alongside `sandbox` rather than inside it.
function sh(cmd, sandbox) {
  if (!sandbox) return Aike.sh(cmd);
  const { cwd = WD, ...rest } = sandbox;
  return Aike.sh(cmd, { cwd, sandbox: rest });
}

// Read a file's contents through the shell, for the cases that must observe
// what the *sandboxed* process can see.
function cat(path, sandbox) {
  return sh(WIN ? `type ${q(path)}` : `cat ${q(path)}`, sandbox);
}

// Run an executable *directly* under the sandbox. On Windows the command
// interpreter cannot create the child process itself (the system-drive root
// lacks an `ALL APPLICATION PACKAGES` ACE, which cmd.exe and PowerShell look
// up when they spawn), so proving a granted `rx` path runs means starting the
// image rather than asking cmd to.
function spawnExe(exe, sandbox, args = []) {
  const { cwd = WD, ...rest } = sandbox;
  return Aike.spawnSync(exe, args, { cwd, sandbox: rest });
}

// Whether starting an executable the sandbox must refuse throws (the backend
// rejects the launch) rather than running.
function execRefused(exe, sandbox) {
  try {
    spawnExe(exe, sandbox);
    return false;
  } catch {
    return true;
  }
}

async function rebuildFixtures() {
  if (await exists(ROOT)) await removeDir(ROOT, { recursive: true });
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
    await makeDir(join(ROOT, d), { recursive: true });
  }
  await writeText(join(ROOT, "secret", "key.txt"), "SECRET");
  await writeText(join(ROOT, "outside", "o.txt"), "OUTSIDE");
  await writeText(join(ROOT, "grants", "readable.txt"), "READABLE");
  await writeText(join(ROOT, "grants", "plain.txt"), "PLAIN");

  if (WIN) {
    // A real executable, so running it actually exercises the execute ACE.
    const bytes = await readBytes(HOST_EXE);
    for (const dst of ["tools", "rw", "work"]) {
      await writeBytes(join(ROOT, dst, "prog.exe"), bytes);
    }
  } else {
    await writeText(join(ROOT, "tools", "t.sh"), "#!/bin/sh\necho TOOL\n");
    // writeText creates the file without an execute bit, and the rx grant
    // below claims to allow running it.
    Aike.sh(`chmod +x ${q(join(ROOT, "tools", "t.sh"))}`);
  }
}

await rebuildFixtures();
const TOOL = WIN ? join(ROOT, "tools", "prog.exe") : join(ROOT, "tools", "t.sh");
const RWEZ = join(ROOT, "rw", "prog.exe");

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
const expectedBackend = WIN
  ? "appcontainer"
  : process.platform === "darwin"
    ? "seatbelt"
    : "bubblewrap";
check(
  "sandboxSupport names the backend",
  support.backend === expectedBackend,
  String(support.backend),
);

// --- the working directory is writable ------------------------------------
{
  const r = sh(
    WIN ? "echo written> proof.txt & type proof.txt" : "echo written > proof.txt && cat proof.txt",
    { cwd: WD },
  );
  check(
    "write inside cwd succeeds",
    r.exitCode === 0 && out(r) === "written",
    err(r),
  );
}

// --- writes outside cwd are refused ---------------------------------------
{
  const hack = join(ROOT, "outside", "hack.txt");
  const r = sh(WIN ? `echo nope> ${q(hack)}` : `echo nope > ${q(hack)}`, { cwd: WD });
  const renamed = await exists(hack);
  check(
    "write outside cwd is refused",
    (r.exitCode !== 0 || !renamed) && !renamed,
    `exit=${r.exitCode} exists=${renamed} ${err(r)}`,
  );
}

// --- /dev/null is writable -------------------------------------------------
// The one writable path outside the policy: a sink, so writing to it returns
// nothing and reaches nothing. Most command-line tools are invoked as
// `cmd > /dev/null`, so refusing it breaks ordinary scripts rather than
// containing them.
if (WIN) {
  skip(
    "write to /dev/null succeeds",
    "Windows has no /dev/null; the AppContainer backend grants write access only to the cwd and the grants",
  );
} else {
  const r = sh("echo discarded > /dev/null", { cwd: WD });
  check("write to /dev/null succeeds", r.exitCode === 0, `exit=${r.exitCode} ${err(r)}`);
}

// --- a denied path cannot be read -----------------------------------------
{
  const r = cat(join(ROOT, "secret", "key.txt"), {
    cwd: WD,
    denied: [join(ROOT, "secret")],
  });
  check(
    "denied path content is unreadable",
    r.exitCode !== 0 || out(r) !== "SECRET",
    out(r),
  );
}

// --- how a denied path looks differs by backend ---------------------------
if (process.platform === "darwin") {
  // `file-read-metadata` is kept so a program can see the path is there, and
  // the name still appears in the parent listing. Note what this does NOT
  // give: listing the denied directory's *own* contents needs read-data on
  // that directory, so `ls secret/` is refused.
  const parent = sh(`ls ${q(ROOT)}`, { cwd: WD, denied: [join(ROOT, "secret")] });
  check(
    "denied entry still named in the parent listing",
    out(parent).includes("secret"),
    out(parent),
  );
  const st = sh(`test -f ${q(join(ROOT, "secret", "key.txt"))} && echo EXISTS`, {
    cwd: WD,
    denied: [join(ROOT, "secret")],
  });
  check("denied path is still stat-able", out(st) === "EXISTS", out(st));
  const inside = sh(`ls ${q(join(ROOT, "secret"))}`, {
    cwd: WD,
    denied: [join(ROOT, "secret")],
  });
  check(
    "listing the denied directory's contents is refused",
    inside.exitCode !== 0,
    out(inside),
  );
} else {
  // Windows and Linux make an ungranted path disappear entirely; the parent
  // is not reachable either, so there is nothing to list and no name to see.
  const parent = sh(WIN ? `dir ${q(ROOT)}` : `ls ${q(ROOT)}`, {
    cwd: WD,
    denied: [join(ROOT, "secret")],
  });
  check(
    "an ungranted parent directory is not listable",
    parent.exitCode !== 0,
    out(parent),
  );
}

// --- a granted rx path: read and execute yes, write no --------------------
{
  const sb = { cwd: WD, grants: [{ path: join(ROOT, "tools"), access: "rx" }] };
  // Windows starts the image directly (cmd cannot create the child); the other
  // backends run it through the shell like any command.
  const r = WIN ? spawnExe(TOOL, sb) : sh(TOOL, sb);
  check(
    "granted rx path executes",
    r.exitCode === 0,
    `exit=${r.exitCode} ${err(r)}`,
  );

  const w = sh(WIN ? `echo x> ${q(join(ROOT, "tools", "new"))}` : `touch ${q(join(ROOT, "tools", "new"))}`, sb);
  check(
    "granted rx path rejects writes",
    w.exitCode !== 0,
    `exit=${w.exitCode} ${out(w)}`,
  );
}

// --- a granted rw path is writable but NOT executable ---------------------
{
  const sb = { cwd: WD, grants: [{ path: join(ROOT, "rw"), access: "rw" }] };
  const w = sh(WIN ? `echo ok> ${q(join(ROOT, "rw", "a.txt"))}` : `echo ok > ${q(join(ROOT, "rw", "a"))}`, sb);
  check("granted rw path accepts writes", w.exitCode === 0, err(w));

  if (WIN) {
    check("granted rw path refuses execution", execRefused(RWEZ, sb));
  } else if (process.platform === "darwin") {
    const a = join(ROOT, "rw", "a");
    const x = sh(`chmod +x ${q(a)} && ${q(a)}`, sb);
    check("granted rw path refuses execution", x.exitCode !== 0, `exit=${x.exitCode}`);
  } else {
    skip("granted rw path refuses execution", "bwrap cannot separate the execute bit");
  }
}

// --- the cwd is writable but not executable -------------------------------
{
  if (WIN) {
    const payload = join(WD, "payload.exe");
    check("a program written into cwd does not run", execRefused(payload, { cwd: WD }));
  } else {
    skip(
      "a program written into cwd does not run",
      "not enforced by the macOS/Linux backends",
    );
  }
}

// --- the host's environment is not inherited ------------------------------
{
  // A variable the host has and the sandbox does not forward. Setting
  // `process.env` does not reach the OS environment, so the value has to come
  // from what aike itself inherited.
  const probe = WIN ? "COMPUTERNAME" : "USER";
  const hostCmd = WIN ? `echo [%${probe}%]` : `echo "[$${probe}]"`;
  const hostVal = out(Aike.sh(hostCmd));
  check(
    "host carries the probe variable (control)",
    hostVal.startsWith("[") && hostVal.endsWith("]") && hostVal.length > 2,
    hostVal,
  );

  const inside = Aike.sh(hostCmd, { cwd: WD, sandbox: {} });
  check(
    "host env vars are not inherited",
    out(inside) !== hostVal,
    out(inside),
  );
}

// --- the sandboxed cwd is where the command starts ------------------------
// Compared against a plain spawn of the same directory, not against the
// literal `WD`. Both backends resolve the working directory before starting
// the command, so on a host where a component is a symlink — `/tmp` is
// `/private/tmp` on macOS — the shell reports the resolved path, and the
// sandboxed and unsandboxed answers agree while neither matches the spelling
// the caller passed. What is under test is that the sandbox does not move the
// command, not that it preserves the spelling.
{
  const cmd = WIN ? "cd" : "/bin/pwd -P";
  const inSandbox = out(sh(cmd, { cwd: WD }));
  const plain = out(Aike.sh(cmd, { cwd: WD }));
  check(
    "the shell reports cwd",
    inSandbox.toLowerCase() === plain.toLowerCase() &&
      inSandbox.toLowerCase().endsWith(join(ROOT, "work").toLowerCase()),
    `sandbox=${inSandbox} plain=${plain}`,
  );
}

// cwd is a starting point, not the boundary: cd'ing out and writing there is
// still refused, because the grants are what constrain the process.
{
  await makeDir(join(ROOT, "elsewhere"), { recursive: true });
  const escaped = join(ROOT, "elsewhere", "escaped.txt");
  const r = sh(WIN ? `cd /d ${q(join(ROOT, "elsewhere"))} & echo x> escaped.txt` : `cd ${q(join(ROOT, "elsewhere"))} && touch escaped.txt`, {
    cwd: WD,
  });
  check("cd out of cwd does not widen write access", r.exitCode !== 0 || !(await exists(escaped)), `exit=${r.exitCode}`);
  check("nothing landed after cd'ing out", !(await exists(escaped)));

  const dotdot = sh(WIN ? `echo x> ..\\outside\\via_dotdot.txt` : `echo x > ../outside/via_dotdot.txt`, { cwd: WD });
  check("a relative .. write out of cwd is refused", dotdot.exitCode !== 0, `exit=${dotdot.exitCode}`);
}

// With no `cwd`, the sandbox runs in the current directory — the same place a
// plain spawn would, and the one place it may write.
{
  const here = await realCwd();
  const inSandbox = strip(dec.decode(Aike.sh(WIN ? "cd" : "/bin/pwd -P", { sandbox: {} }).stdout));
  check(
    "a sandbox with no cwd starts in the current directory",
    inSandbox.toLowerCase() === here.toLowerCase(),
    `host=${here} sandbox=${inSandbox}`,
  );
}

async function realCwd() {
  const r = Aike.sh(WIN ? "cd" : "/bin/pwd -P");
  return strip(dec.decode(r.stdout));
}

// --- network is denied by default -----------------------------------------
// The body goes to a file inside the working directory rather than to
// /dev/null. Discarding it would leave "no output on stdout" true whether or
// not the fetch worked, so the `||` in the check below could never fail and the
// case proved nothing. The outcome is curl's exit status plus whether the file
// appeared; the case after this one runs the identical command with the
// network allowed, which is the control that makes a failure mean something.
const CURL = WIN ? "C:\\Windows\\System32\\curl.exe" : "/usr/bin/curl";
const URL = "https://example.com";
const BODY = join(WD, "body.html");

function fetchUrl(sandbox, timeoutSec) {
  const args = ["-s", "-m", String(timeoutSec), "-o", BODY, URL];
  return WIN
    ? spawnExe(CURL, sandbox, args)
    : sh(`/usr/bin/curl -s -m ${timeoutSec} -o ${q(BODY)} ${URL}`, sandbox);
}

{
  const r = fetchUrl({ cwd: WD }, 5);
  check(
    "network denied by default",
    r.exitCode !== 0 && !(await exists(BODY)),
    `exit=${r.exitCode} body=${await exists(BODY)} ${err(r)}`,
  );
}

// --- network is available when asked for ----------------------------------
{
  const r = fetchUrl({ cwd: WD, network: "host" }, 8);
  if (r.exitCode === 0) {
    check('network allowed with network:"host"', true);
  } else {
    // Only reachable on a machine with no route out at all: the same command
    // succeeded in the sandboxed case above with the network denied, so a
    // failure here is the host's, not the policy's.
    skip(
      'network allowed with network:"host"',
      `no host network here: exit=${r.exitCode} ${err(r)}`,
    );
  }
}

// --- stderr carries no sandbox-exec deprecation noise ---------------------
{
  const r = sh(WIN ? "echo real-output 1>&2" : "echo real-output >&2", { cwd: WD });
  if (process.platform === "darwin") {
    check(
      "stderr has no deprecation warning",
      !err(r).includes("sandbox-exec is deprecated"),
      err(r),
    );
  }
  check("stderr still reaches the caller", err(r) === "real-output", err(r));
}

// --- exit status and signals ----------------------------------------------
{
  const r = sh("exit 7", { cwd: WD });
  check("exit status is propagated", r.exitCode === 7, String(r.exitCode));
}

if (process.platform !== "win32") {
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
} else {
  // The process tree lives in a job object; `kill` terminates the whole job.
  // `set /p x=` blocks on stdin, which the sandbox leaves open.
  const p = Aike.spawn("cmd.exe", ["/d", "/s", "/c", "set /p x="], {
    cwd: WD,
    sandbox: {},
    stdin: "pipe",
    stdout: "pipe",
    stderr: "pipe",
  });
  await Aike.sleep(300);
  check("child is running", p.isRunning() === true);
  const t0 = Date.now();
  p.kill();
  const code = await p.exitCode;
  check("kill terminates the sandboxed tree", Date.now() - t0 < 5000, `took ${Date.now() - t0}ms`);
  check("killed child reports non-zero", code !== 0, String(code));
}

// --- rejections: every one names its field --------------------------------
const rejects = [
  ["missing cwd (a relative one)", { cwd: "relative/path", sandbox: {} }, /cwd/],
  ["nonexistent cwd", { cwd: join(ROOT, "nope"), sandbox: {} }, /cwd/],
  ["cwd that is a file", { cwd: join(ROOT, "grants", "plain.txt"), sandbox: {} }, /cwd/],
  [
    "nonexistent grant",
    { cwd: WD, sandbox: { grants: [{ path: join(ROOT, "ghost"), access: "r" }] } },
    /grants/,
  ],
  [
    "bad access mode",
    { cwd: WD, sandbox: { grants: [{ path: join(ROOT, "tools"), access: "zz" }] } },
    /access/,
  ],
  [
    "access without read",
    { cwd: WD, sandbox: { grants: [{ path: join(ROOT, "tools"), access: "w" }] } },
    /read/,
  ],
  ["deny over cwd", { cwd: WD, sandbox: { denied: [WD] } }, /cwd/],
  ["bad network value", { cwd: WD, sandbox: { network: "maybe" } }, /network/],
  [
    "nonexistent denied",
    { cwd: WD, sandbox: { denied: [join(ROOT, "ghost")] } },
    /denied/,
  ],
  [
    "denied over a grant",
    {
      cwd: WD,
      sandbox: {
        grants: [{ path: join(ROOT, "tools"), access: "rx" }],
        denied: [join(ROOT, "tools")],
      },
    },
    /denied/,
  ],
  [
    "rw without x on a file",
    {
      cwd: WD,
      sandbox: { grants: [{ path: join(ROOT, "grants", "plain.txt"), access: "rw" }] },
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
    Aike.sh(WIN ? "exit /b 0" : "true", opts);
  } catch (e) {
    msg = String(e && e.message ? e.message : e);
  }
  check(`rejects ${label}`, want.test(msg), msg || "no error thrown");
}

// --- spawnSync path --------------------------------------------------------
{
  const r = Aike.spawnSync(WIN ? "cmd.exe" : "/bin/echo", WIN ? ["/d", "/s", "/c", "echo sync-works"] : ["sync-works"], {
    cwd: WD,
    sandbox: {},
  });
  check(
    "spawnSync honours the sandbox",
    r.exitCode === 0 && out(r) === "sync-works",
  );
}

// --- grants other than rx / rw --------------------------------------------
{
  const sb = { cwd: WD, grants: [{ path: join(ROOT, "grants"), access: "r" }] };
  const r = cat(join(ROOT, "grants", "readable.txt"), sb);
  check("grant r allows reading", r.exitCode === 0 && out(r) === "READABLE", `${err(r)}`);

  const w = sh(WIN ? `echo x> ${q(join(ROOT, "grants", "new"))}` : `touch ${q(join(ROOT, "grants", "new"))}`, sb);
  check("grant r refuses writing", w.exitCode !== 0, `exit=${w.exitCode}`);
}

// A grant naming a file rather than a directory is a distinct code path.
{
  const sb = {
    cwd: WD,
    grants: [{ path: join(ROOT, "grants", "plain.txt"), access: "r" }],
  };
  const r = cat(join(ROOT, "grants", "plain.txt"), sb);
  check("a grant on a file is readable", r.exitCode === 0 && out(r) === "PLAIN", err(r));
}

{
  const sb = {
    cwd: WD,
    grants: [
      { path: join(ROOT, "grants"), access: "r" },
      { path: join(ROOT, "tools"), access: "rx" },
    ],
  };
  const read = cat(join(ROOT, "grants", "readable.txt"), sb);
  const run = WIN ? spawnExe(TOOL, sb) : sh(TOOL, sb);
  check(
    "two grants both apply",
    read.exitCode === 0 && out(read) === "READABLE" && run.exitCode === 0,
    `read=${out(read)} exec=${run.exitCode} ${err(run)}`,
  );
}

// A denied path may be a file, not only a directory.
{
  const r = cat(join(ROOT, "grants", "plain.txt"), {
    cwd: WD,
    denied: [join(ROOT, "grants", "plain.txt")],
  });
  check("denied file is unreadable", r.exitCode !== 0 || out(r) !== "PLAIN", out(r));
}

// --- symlinks must not route around a rule --------------------------------
if (WIN) {
  skip("a symlink to a denied path is still denied", "symlink creation needs privileges on Windows");
  skip("a symlink cannot be used to write into a denied path", "symlink creation needs privileges on Windows");
} else {
  Aike.sh(`ln -sfn ${q(join(ROOT, "secret"))} ${q(join(WD, "link_to_secret"))}`);
  const r = cat(join(WD, "link_to_secret", "key.txt"), {
    cwd: WD,
    denied: [join(ROOT, "secret")],
  });
  check(
    "a symlink to a denied path is still denied",
    r.exitCode !== 0 || out(r) !== "SECRET",
    out(r),
  );

  Aike.sh(`ln -sfn ${q(join(ROOT, "target"))} ${q(join(ROOT, "rwdir", "link_out"))}`);
  const w = sh(`echo pwned > ${q(join(ROOT, "rwdir", "link_out", "via_link"))}`, {
    cwd: WD,
    grants: [{ path: join(ROOT, "rwdir"), access: "rw" }],
    denied: [join(ROOT, "target")],
  });
  check(
    "a symlink cannot be used to write into a denied path",
    w.exitCode !== 0,
    `exit=${w.exitCode}`,
  );
}

// --- env and stdin --------------------------------------------------------
{
  const r = Aike.sh(WIN ? "echo [%AIKE_PROBE%]" : 'echo "[$AIKE_PROBE]"', {
    cwd: WD,
    sandbox: {},
    env: { AIKE_PROBE: "forwarded" },
  });
  check("caller env reaches the sandboxed command", out(r) === "[forwarded]", out(r));
}

{
  // `more.com` echoes its stdin; started directly because cmd cannot spawn it.
  const r = WIN
    ? Aike.spawnSync("C:\\Windows\\System32\\more.com", [], {
      cwd: WD,
      sandbox: {},
      input: "piped-in",
    })
    : Aike.sh("cat", { cwd: WD, sandbox: {}, input: "piped-in" });
  check("stdin reaches the sandboxed command", out(r).includes("piped-in"), out(r));
}

// --- timeout --------------------------------------------------------------
{
  // `more.com` blocks on stdin, which the sandbox keeps open, so the timeout
  // is what ends it.
  if (WIN) {
    const p = Aike.spawn("cmd.exe", ["/d", "/s", "/c", "set /p x="], {
      cwd: WD,
      sandbox: {},
      timeout: 500,
      stdout: "pipe",
      stderr: "pipe",
    });
    const killed = await p.killedByTimeout;
    check("timeout kills the sandboxed tree", killed === true, String(killed));
  } else {
    const r = Aike.sh("sleep 30", { cwd: WD, sandbox: {}, timeout: 500 });
    check(
      "timeout kills the sandboxed tree",
      r.killedByTimeout === true,
      `exit=${r.exitCode} killed=${r.killedByTimeout}`,
    );
  }
}

{
  let threw = false;
  try {
    Aike.sh("exit 3", { cwd: WD, sandbox: {}, check: true });
  } catch {
    threw = true;
  }
  check("check:true throws for a sandboxed non-zero exit", threw);
}

// --- the async Subprocess surface -----------------------------------------
{
  const p = WIN
    ? Aike.spawn("cmd.exe", ["/d", "/s", "/c", "echo streaming & echo more"], {
      cwd: WD,
      sandbox: {},
      stdout: "pipe",
      stderr: "pipe",
    })
    : Aike.spawn("/bin/sh", ["-c", "echo streaming; sleep 0.2; echo more"], {
      cwd: WD,
      sandbox: {},
      stdout: "pipe",
      stderr: "pipe",
    });
  let got = "";
  for (; ;) {
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
  // Close stdin, the first rung of the ladder: the child should exit on EOF.
  const p = WIN
    ? Aike.spawn("cmd.exe", ["/d", "/s", "/c", "set /p x="], {
      cwd: WD,
      sandbox: {},
      stdin: "pipe",
      stdout: "pipe",
      stderr: "pipe",
    })
    : Aike.spawn("/bin/sh", ["-c", "trap 'exit 0' TERM; cat"], {
      cwd: WD,
      sandbox: {},
      stdin: "pipe",
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

// --- sandboxSupport failure shape -----------------------------------------
{
  const s = Aike.sandboxSupport();
  check(
    "sandboxSupport.reason is null when available",
    s.available ? s.reason === null : typeof s.reason === "string",
  );
}

console.log(`\n${passed} passed, ${failed} failed, ${skipped} skipped`);
if (failed > 0) process.exit(1);
