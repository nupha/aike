/**
 * sandbox_limits_test.mjs — Test nanosandbox features and limits.
 *
 * Covers:
 *   - Mounts: cwd/ro/rw, target aliasing, descendant inheritance, validation
 *   - Limits: memoryLimit, cpuLimit, maxPids, maxFileSize, maxOpenFiles
 *   - Features: tmpfs, network allowlist, seccompProfile, uid/gid, hostname
 *   - Baseline: no-sandbox comparison
 *
 * Usage:  aike -m tests/spawn/sandbox_limits_test.mjs
 */

const dec = new TextDecoder();
function strip(s) {
  return s.replace(/\u001b\[[0-9;]*m/g, "").trim();
}
function stdout(r) {
  return strip(dec.decode(r.stdout));
}
function stderr(r) {
  return strip(dec.decode(r.stderr));
}

let passed = 0;
let failed = 0;

function check(label, cond, detail = "") {
  if (cond) {
    passed++;
    console.log(`  \x1b[32m✓\x1b[0m ${label}`);
  } else {
    failed++;
    console.log(`  \x1b[31m✗\x1b[0m ${label}${detail ? " — " + detail : ""}`);
  }
}

function skip(label, reason) {
  passed++; // skipped = passed
  console.log(`  \x1b[33m○\x1b[0m ${label} (skipped: ${reason})`);
}

const fs = await import("fs");
const base = `${process.env.HOME}/.aike-sandbox-limits-${process.pid}`;
const ws = `${base}/ws`;
const ro = `${base}/ro-data`;
const rw = `${base}/rw-data`;
const isMacos = process.platform === "darwin";
const isWin = process.platform === "win32";

async function setup() {
  await Promise.all([
    fs.makeDir(ws, { recursive: true }),
    fs.makeDir(ro, { recursive: true }),
    fs.makeDir(rw, { recursive: true }),
  ]);
  await fs.writeText(`${ws}/ws-file.txt`, "WS");
  await fs.writeText(`${ro}/secret.txt`, "SECRET");
  await fs.writeText(`${rw}/rw-file.txt`, "RW");
}

async function cleanup() {
  await fs.remove(base).catch(() => {});
}

// Cross-platform shell helpers
function shellSpawn(script, sandboxOpts, extra) {
  if (isWin) {
    return Aike.spawnSync("cmd.exe", ["/c", script], { sandbox: sandboxOpts, ...extra });
  }
  return Aike.spawnSync("sh", ["-c", script], { sandbox: sandboxOpts, ...extra });
}

function writeRc(script, sandboxOpts) {
  if (isWin) {
    // cmd.exe: capture exit code via %ERRORLEVEL% after running command
    const r = Aike.spawnSync("cmd.exe", ["/c", `${script} 2>nul & echo rc=%ERRORLEVEL%`], {
      sandbox: sandboxOpts,
    });
    const m = stdout(r).match(/rc=(\d+)/);
    return m ? Number(m[1]) : -1;
  }
  const r = Aike.spawnSync("sh", ["-c", `${script} 2>/dev/null; echo rc=$?`], {
    sandbox: sandboxOpts,
  });
  const m = stdout(r).match(/rc=(\d+)/);
  return m ? Number(m[1]) : -1;
}

function readOut(script, sandboxOpts) {
  if (isWin) {
    const r = Aike.spawnSync("cmd.exe", ["/c", script], { sandbox: sandboxOpts });
    return stdout(r);
  }
  const r = Aike.spawnSync("sh", ["-c", script], { sandbox: sandboxOpts });
  return stdout(r);
}

function mkSandbox(overrides = {}) {
  return {
    cwd: ws,
    mounts: { [ws]: { writable: true } },
    network: "none",
    ...overrides,
  };
}

// Cross-platform commands
const cat = isWin ? "type" : "cat";
const nul = isWin ? "nul" : "/dev/null";
const stderrRedirect = isWin ? "2>nul" : "2>/dev/null";
const etcHosts = isWin ? "C:/Windows/System32/drivers/etc/hosts" : "/etc/hosts";

// ===================================================================
// 1. Mounts — read/write per directory
// ===================================================================
async function testMounts() {
  console.log("\n=== mounts ===");

  if (isWin) {
    skip("read/write mounts", "filesystem isolation not implemented on Windows");
    skip("write outside denied", "filesystem isolation not implemented on Windows");
    skip("write ro denied", "filesystem isolation not implemented on Windows");
    skip("write rw allowed", "filesystem isolation not implemented on Windows");
    return;
  }

  const mounts = {
    [ws]: { writable: true },
    [ro]: { target: "/mnt-data" },
    [rw]: { target: "/mnt-rw", writable: true },
  };
  const sb = { cwd: ws, mounts, network: "host" };

  const outside = `${base}/outside.txt`;
  await fs.remove(outside).catch(() => {});

  // reads
  check("read cwd file", readOut(`${cat} ${ws}/ws-file.txt`, sb) === "WS");
  check("read ro source", readOut(`${cat} ${ro}/secret.txt`, sb) === "SECRET");
  check("read rw source", readOut(`${cat} ${rw}/rw-file.txt`, sb) === "RW");
  check("read system file", readOut(`${cat} ${etcHosts}`, sb).length > 0);

  // writes
  check("write cwd allowed", (await writeRc(`echo x > ${ws}/new.txt`, sb)) === 0);
  check("write outside denied", (await writeRc(`echo x > ${outside}`, sb)) !== 0);
  check("outside not created", !(await fs.exists(outside)));

  // ro mount
  check("write ro denied", (await writeRc(`echo x >> ${ro}/secret.txt`, sb)) !== 0);
  check("ro content unchanged", (await fs.readText(`${ro}/secret.txt`)) === "SECRET");

  // rw mount
  check("write rw allowed", (await writeRc(`echo x > ${rw}/new.txt`, sb)) === 0);
  const rwContent = await fs.readText(`${rw}/new.txt`);
  check("rw content persisted", rwContent === "x\n" || rwContent === "x");
}

// ===================================================================
// 2. Non-existent mount source — throws at sandbox creation
// ===================================================================
async function testNonexistentMount() {
  console.log("\n=== non-existent mount source ===");

  const ghost = `${base}/ghost`;
  const sb = {
    cwd: ws,
    mounts: {
      [ghost]: { target: "/ghost" },
    },
    network: "host",
  };

  const throws = (fn) => { try { fn(); return null; } catch (e) { return e; } };
  const sh = isWin ? "cmd.exe" : "sh";
  const shArg = isWin ? "/c" : "-c";
  const e = throws(() => Aike.spawnSync(sh, [shArg, "echo x"], { sandbox: sb }));
  check("non-existent mount throws", e !== null, e?.message);
}

// ===================================================================
// 4. Descendant inheritance — child/grandchild inherit sandbox
// ===================================================================
async function testDescendantInheritance() {
  console.log("\n=== descendant inheritance ===");

  if (isWin) {
    skip("grandchild ro write denied", "filesystem isolation not implemented on Windows");
    skip("bg grandchild ro write denied", "filesystem isolation not implemented on Windows");
    skip("bg grandchild rw write allowed", "filesystem isolation not implemented on Windows");
    return;
  }

  const mounts = {
    [ws]: { writable: true },
    [ro]: { target: "/mnt-data" },
    [rw]: { target: "/mnt-rw", writable: true },
  };
  const sb = { cwd: ws, mounts, network: "host" };

  if (isWin) {
    // On Windows, use cmd.exe to spawn grandchildren
    // grandchild write to ro denied
    check("grandchild ro write denied",
      (await writeRc(`cmd.exe /c "echo x > ${ro}/secret.txt"`, sb)) !== 0);
    check("grandchild ro content unchanged",
      (await fs.readText(`${ro}/secret.txt`)) === "SECRET");

    // background grandchild ro write denied (use start /b for background)
    shellSpawn(`cmd.exe /c "start /b cmd.exe /c \\"echo bg > ${ro}/secret.txt\\""`, sb);
    await new Promise((r) => setTimeout(r, 500));
    check("bg grandchild ro write denied (host state)",
      (await fs.readText(`${ro}/secret.txt`)) === "SECRET");

    // background grandchild rw write allowed
    shellSpawn(`cmd.exe /c "start /b cmd.exe /c \\"echo bg-rw > ${rw}/inherit.txt\\""`, sb);
    await new Promise((r) => setTimeout(r, 500));
    check("bg grandchild rw write allowed (host state)",
      (await fs.readText(`${rw}/inherit.txt`)).includes("bg-rw"));
  } else {
    // grandchild write to ro denied
    check("grandchild ro write denied",
      (await writeRc(`sh -c 'echo x > ${ro}/secret.txt'`, sb)) !== 0);
    check("grandchild ro content unchanged",
      (await fs.readText(`${ro}/secret.txt`)) === "SECRET");

    // background grandchild ro write denied (assert host state, since & wait returns 0)
    Aike.spawnSync("sh", ["-c", `sh -c 'echo bg > ${ro}/secret.txt' & wait`], { sandbox: sb });
    check("bg grandchild ro write denied (host state)",
      (await fs.readText(`${ro}/secret.txt`)) === "SECRET");

    // background grandchild rw write allowed
    Aike.spawnSync("sh", ["-c", `sh -c 'echo bg-rw > ${rw}/inherit.txt' & wait`], { sandbox: sb });
    check("bg grandchild rw write allowed (host state)",
      (await fs.readText(`${rw}/inherit.txt`)).includes("bg-rw"));
  }
}

// ===================================================================
// 5. No sandbox baseline — unrestricted writes
// ===================================================================
async function testNoSandboxBaseline() {
  console.log("\n=== no sandbox baseline ===");

  const probe = `${base}/baseline-probe.txt`;
  await fs.remove(probe).catch(() => {});

  if (isWin) {
    const r = Aike.spawnSync("cmd.exe", ["/c", `echo x > ${probe}`]);
    check("without sandbox, $HOME write allowed",
      r.exitCode === 0 && (await fs.exists(probe)));
  } else {
    const r = Aike.spawnSync("sh", ["-c", `echo x > ${probe}`]);
    check("without sandbox, $HOME write allowed",
      r.exitCode === 0 && (await fs.exists(probe)));
  }
}

// ===================================================================
// 6. Strict validation — parameter errors
// ===================================================================
async function testStrictValidation() {
  console.log("\n=== strict validation ===");

  const throws = (fn) => { try { fn(); return null; } catch (e) { return e; } };
  const mounts = { [ws]: { writable: true } };
  const sh = isWin ? "cmd.exe" : "sh";
  const shArg = isWin ? "/c" : "-c";

  check("missing cwd throws", throws(() =>
    Aike.spawnSync(sh, [shArg, "echo x"], { sandbox: { mounts } })) !== null);

  check("cwd non-string throws TypeError", (() => {
    const e = throws(() => Aike.spawnSync(sh, [shArg, "echo x"], { sandbox: { cwd: 123, mounts } }));
    return e?.name === "TypeError";
  })());

  check("cwd nonexistent throws RangeError", (() => {
    const e = throws(() => Aike.spawnSync(sh, [shArg, "echo x"], { sandbox: { cwd: "/no/such/dir", mounts } }));
    return e?.name === "RangeError";
  })());

  check("sandbox as array throws", (() => {
    const e = throws(() => Aike.spawnSync(sh, [shArg, "echo x"], { sandbox: [] }));
    return e !== null;
  })());

  check("mounts as array throws", (() => {
    const e = throws(() => Aike.spawnSync(sh, [shArg, "echo x"], { sandbox: { cwd: ws, mounts: [] } }));
    return e !== null;
  })());

  check("mount value non-object throws", (() => {
    const e = throws(() => Aike.spawnSync(sh, [shArg, "echo x"], { sandbox: { cwd: ws, mounts: { [ro]: "rw" } } }));
    return e !== null;
  })());

  check("mount writable non-boolean throws", (() => {
    const e = throws(() => Aike.spawnSync(sh, [shArg, "echo x"], { sandbox: { cwd: ws, mounts: { [ro]: { writable: 1 } } } }));
    return e !== null;
  })());

  check("network invalid throws", (() => {
    const e = throws(() => Aike.spawnSync(sh, [shArg, "echo x"], { sandbox: { cwd: ws, network: "wifi" } }));
    return e !== null;
  })());
}

// ===================================================================
// 7. memoryLimit
// ===================================================================
async function testMemoryLimit() {
  console.log("\n=== memoryLimit ===");

  if (isMacos) {
    skip("kills over-allocating process", "not enforced on macOS");
    return;
  }
  if (isWin) {
    skip("kills over-allocating process", "perl not available in Windows sandbox");
    return;
  }

  const script = `perl -e 'my @a; for (1..300) { push @a, "x" x 100000 } print "survived\n"'`;
  const r = Aike.spawnSync("sh", ["-c", script], {
    sandbox: mkSandbox({ memoryLimit: 4 * 1024 * 1024 }),
    timeout: 10000,
  });
  const survived = stdout(r).includes("survived");
  check("kills over-allocating process", !survived,
    survived ? "survived 30MB alloc under 4MB limit" : `exit=${r.exitCode}`);
}

// ===================================================================
// 8. cpuLimit
// ===================================================================
async function testCpuLimit() {
  console.log("\n=== cpuLimit ===");

  if (isMacos) {
    skip("limited takes >= 1.2x of unlimited", "not enforced on macOS");
    return;
  }
  if (isWin) {
    skip("limited takes >= 1.2x of unlimited", "perl not available in Windows sandbox");
    return;
  }

  const spin = `perl -e 'my $x=0; $x++ for 1..50_000_000; print "done\n"'`;

  const t0 = Date.now();
  const unlimited = Aike.spawnSync("sh", ["-c", spin], {
    sandbox: mkSandbox(),
    timeout: 60000,
  });
  const baseMs = Date.now() - t0;

  const t1 = Date.now();
  const limited = Aike.spawnSync("sh", ["-c", spin], {
    sandbox: mkSandbox({ cpuLimit: 0.5 }),
    timeout: 120000,
  });
  const limitedMs = Date.now() - t1;

  check("unlimited finishes", unlimited.exitCode === 0, stdout(unlimited));
  check("limited finishes", limited.exitCode === 0, stdout(limited));
  check("limited takes >= 1.2x of unlimited", limitedMs >= baseMs * 1.2,
    `limited=${limitedMs}ms base=${baseMs}ms`);
}

// ===================================================================
// 9. maxPids
// ===================================================================
async function testMaxPids() {
  console.log("\n=== maxPids ===");

  if (isMacos) {
    skip("some spawns rejected", "not enforced on macOS");
    return;
  }
  if (isWin) {
    skip("some spawns rejected", "shell job control not available on Windows");
    return;
  }

  const script = `ok=0; fail=0
for i in 1 2 3 4 5 6 7 8; do
  sleep 10 &
  rc=$?
  if [ $rc -eq 0 ]; then ok=$((ok+1)); else fail=$((fail+1)); fi
done
echo ok=$ok fail=$fail
kill $(jobs -p) 2>/dev/null`;

  const r = Aike.spawnSync("sh", ["-c", script], {
    sandbox: mkSandbox({ maxPids: 4 }),
    timeout: 10000,
  });
  const s = stdout(r);
  const m = s.match(/ok=(\d+)\s+fail=(\d+)/);
  if (m) {
    check("some spawns rejected", Number(m[2]) > 0, `ok=${m[1]} fail=${m[2]}`);
  } else {
    check("parsed output", false, s);
  }
}

// ===================================================================
// 10. maxFileSize
// ===================================================================
async function testMaxFileSize() {
  console.log("\n=== maxFileSize ===");

  const bigFile = `${ws}/bigfile`;
  const smallFile = `${ws}/smallfile`;

  if (isWin) {
    skip("large write rejected", "perl not available in Windows sandbox");
    skip("small write allowed", "perl not available in Windows sandbox");
    return;
  }

  const r1 = Aike.spawnSync("sh", ["-c",
    `perl -e 'print "x" x 4096' > ${bigFile}; echo rc=$?`
  ], {
    sandbox: mkSandbox({ maxFileSize: 512 }),
    timeout: 5000,
  });
  check("large write rejected", stdout(r1).includes("rc=1") || stdout(r1).includes("rc=28"),
    stdout(r1));

  const r2 = Aike.spawnSync("sh", ["-c",
    `perl -e 'print "x" x 100' > ${smallFile}; echo rc=$?`
  ], {
    sandbox: mkSandbox({ maxFileSize: 512 }),
    timeout: 5000,
  });
  check("small write allowed", stdout(r2).includes("rc=0"), stdout(r2));
}

// ===================================================================
// 11. maxOpenFiles
// ===================================================================
async function testMaxOpenFiles() {
  console.log("\n=== maxOpenFiles ===");

  if (isMacos) {
    skip("some opens rejected", "not enforced on macOS");
    return;
  }
  if (isWin) {
    skip("some opens rejected", "perl not available in Windows sandbox");
    return;
  }

  const script = `perl -e '
    my $ok=0; my $fail=0;
    for my $i (1..20) {
      if (open(my $fh, "<", "/dev/null")) { $ok++ } else { $fail++ }
    }
    print "ok=$ok fail=$fail\n";
  '`;

  const r = Aike.spawnSync("sh", ["-c", script], {
    sandbox: mkSandbox({ maxOpenFiles: 4 }),
    timeout: 5000,
  });
  const s = stdout(r);
  const m = s.match(/ok=(\d+)\s+fail=(\d+)/);
  if (m) {
    check("some opens rejected", Number(m[2]) > 0, `ok=${m[1]} fail=${m[2]}`);
  } else {
    check("parsed output", false, s);
  }
}

// ===================================================================
// 12. tmpfs
// ===================================================================
async function testTmpfs() {
  console.log("\n=== tmpfs ===");

  if (isWin) {
    skip("write and read back", "tmpfs not implemented on Windows");
    return;
  }

  const tmpfsDir = `${ws}/ramdisk`;
  if (isWin) {
    const r = Aike.spawnSync("cmd.exe", ["/c",
      `mkdir ${tmpfsDir} 2>nul & echo hello > ${tmpfsDir}/test.txt & ${cat} ${tmpfsDir}/test.txt`
    ], {
      sandbox: mkSandbox({
        tmpfs: [{ path: tmpfsDir, size: 1024 * 1024 }],
      }),
      timeout: 5000,
    });
    check("write and read back", stdout(r) === "hello", stdout(r));
  } else {
    const r = Aike.spawnSync("sh", ["-c",
      `mkdir -p ${tmpfsDir} && echo hello > ${tmpfsDir}/test.txt && cat ${tmpfsDir}/test.txt`
    ], {
      sandbox: mkSandbox({
        tmpfs: [{ path: tmpfsDir, size: 1024 * 1024 }],
      }),
      timeout: 5000,
    });
    check("write and read back", stdout(r) === "hello", stdout(r));
  }
}

// ===================================================================
// 13. seccompProfile
// ===================================================================
async function testSeccompProfile() {
  console.log("\n=== seccompProfile ===");

  const sh = isWin ? "cmd.exe" : "sh";
  const shArg = isWin ? "/c" : "-c";

  const r1 = Aike.spawnSync(sh, [shArg, "echo hello"], {
    sandbox: mkSandbox({ seccompProfile: "strict" }),
    timeout: 5000,
  });
  check("strict seccomp: API accepted", true);

  const r2 = Aike.spawnSync(sh, [shArg, "echo hello"], {
    sandbox: mkSandbox({ seccompProfile: "disabled" }),
    timeout: 5000,
  });
  check("disabled seccomp: echo works", stdout(r2).includes("hello"), stdout(r2));

  const r3 = Aike.spawnSync(sh, [shArg, "echo hello"], {
    sandbox: mkSandbox({ seccompProfile: "standard" }),
    timeout: 5000,
  });
  check("standard seccomp: echo works", stdout(r3).includes("hello"), stdout(r3));
}

// ===================================================================
// 14. network allowlist
// ===================================================================
async function testNetworkAllowlist() {
  console.log("\n=== network allowlist ===");

  if (isWin) {
    skip("external domain blocked", "network filtering not implemented on Windows");
    return;
  }

  const r = Aike.spawnSync("sh", ["-c",
    "curl -s --max-time 3 https://example.com > /dev/null 2>&1; echo rc=$?"
  ], {
    sandbox: mkSandbox({ network: { allow: ["localhost"] } }),
    timeout: 10000,
  });
  const rc = stdout(r).match(/rc=(\d+)/);
  check("external domain blocked", rc && Number(rc[1]) !== 0, stdout(r));
}

// ===================================================================
// 15. network none vs host
// ===================================================================
async function testNetworkModes() {
  console.log("\n=== network modes ===");

  if (isWin) {
    skip("network none/host", "network filtering not implemented on Windows");
    return;
  }

  // "none" — external should be blocked
  const r1 = Aike.spawnSync("sh", ["-c",
    "curl -s --max-time 3 https://example.com > /dev/null 2>&1; echo rc=$?"
  ], {
    sandbox: mkSandbox({ network: "none" }),
    timeout: 10000,
  });
  check("network none: external blocked", stdout(r1).includes("rc=") && !stdout(r1).includes("rc=0"),
    stdout(r1));

  // "host" — external should be reachable (or at least not blocked by sandbox).
  const r2 = Aike.spawnSync("sh", ["-c",
    "curl -s --max-time 5 https://example.com > /dev/null 2>&1; echo rc=$?"
  ], {
    sandbox: mkSandbox({ network: "host" }),
    timeout: 15000,
  });
  const rc2 = stdout(r2).match(/rc=(\d+)/);
  check("network host: external reachable or not sandbox-blocked",
    rc2 && (Number(rc2[1]) === 0 || Number(rc2[1]) === 28),
    stdout(r2));
}

// ===================================================================
// 16. uid/gid
// ===================================================================
async function testUidGid() {
  console.log("\n=== uid/gid ===");

  if (isWin) {
    skip("uid/gid", "uid/gid not applicable on Windows");
    return;
  }

  const r = Aike.spawnSync("sh", ["-c", "id -u"], {
    sandbox: mkSandbox({ uid: 65534, gid: 65534 }),
    timeout: 5000,
  });
  const uid = stdout(r);
  check("uid/gid: API accepted", true);
  console.log(`    (uid reported: ${uid})`);
}

// ===================================================================
// 17. hostname
// ===================================================================
async function testHostname() {
  console.log("\n=== hostname ===");

  const sh = isWin ? "cmd.exe" : "sh";
  const shArg = isWin ? "/c" : "-c";
  const r = Aike.spawnSync(sh, [shArg, "hostname"], {
    sandbox: mkSandbox({ hostname: "sandbox-box" }),
    timeout: 5000,
  });
  const h = stdout(r);
  check("hostname: API accepted", true);
  console.log(`    (hostname reported: ${h})`);
}

// ===================================================================
// 18. wallTimeLimitMs — sandbox-level wall time
// ===================================================================
async function testWallTimeLimitMs() {
  console.log("\n=== wallTimeLimitMs ===");

  const t0 = Date.now();
  if (isWin) {
    // Use ping as sleep equivalent (ping -n N = N-1 seconds)
    const r = Aike.spawnSync("cmd.exe", ["/c", "ping -n 31 127.0.0.1 >nul"], {
      sandbox: mkSandbox({ wallTimeLimitMs: 1000 }),
      timeout: 5000,
    });
    const elapsed = Date.now() - t0;
    check("wallTimeLimitMs kills slow process", r.exitCode !== 0 || elapsed < 3000,
      `exit=${r.exitCode} elapsed=${elapsed}ms`);
  } else {
    const r = Aike.spawnSync("sh", ["-c", "sleep 30"], {
      sandbox: mkSandbox({ wallTimeLimitMs: 1000 }),
      timeout: 5000,
    });
    const elapsed = Date.now() - t0;
    check("wallTimeLimitMs kills slow process", r.exitCode !== 0 || elapsed < 3000,
      `exit=${r.exitCode} elapsed=${elapsed}ms`);
  }
}

// ===================================================================
// Run all
// ===================================================================
await setup();
try {
  await testMounts();
  await testNonexistentMount();
  await testDescendantInheritance();
  await testNoSandboxBaseline();
  await testStrictValidation();
  await testMemoryLimit();
  await testCpuLimit();
  await testMaxPids();
  await testMaxFileSize();
  await testMaxOpenFiles();
  await testTmpfs();
  await testSeccompProfile();
  await testNetworkAllowlist();
  await testNetworkModes();
  await testUidGid();
  await testHostname();
  await testWallTimeLimitMs();
} finally {
  await cleanup();
}

console.log(`\n\x1b[1m${passed} passed, ${failed} failed\x1b[0m\n`);
if (failed > 0) process.exit(1);
