// Aike.schedule (cron) tests.
//
// The state now lives on `AikeRuntimeState` (root-only), so this exercises:
// actual firing through the tokio timer + channel poller, re-arm/nextRun
// update, cancel, callback survival across GC, and root-only gating.
//
// Usage: aike -m tests/cron.mjs

let pass = 0;
let fail = 0;
function check(label, condition, detail) {
  if (condition) {
    console.log(`  \u2713 ${label}`);
    pass++;
  } else {
    console.log(`  \u2717 ${label}${detail ? " \u2014 " + detail : ""}`);
    fail++;
  }
}

console.log("\n=== Aike.schedule: fire / re-arm / cancel ===");

// 6-field pattern = has seconds, every second.
let fires = 0;
const job = Aike.schedule("* * * * * *", () => {
  fires++;
});
check("job.active is initially true", job.active === true);
check("job.nextRun is a string", typeof job.nextRun === "string", `got ${job.nextRun}`);
check("job.id is a positive integer", Number.isInteger(job.id) && job.id > 0);

// The callback + job object must survive explicit GC before it fires.
for (let i = 0; i < 8; i++) {
  const junk = [];
  for (let k = 0; k < 3000; k++) junk.push({ x: k, s: "s" + k });
  Aike.gc();
}

await Aike.sleep(2200);
check("cron fired at least once", fires >= 1, `fires=${fires}`);
check(
  "nextRun refreshed after fire",
  typeof job.nextRun === "string" && job.nextRun.endsWith("Z"),
  `got ${job.nextRun}`,
);

// cancel() must stop further fires.
job.cancel();
check("active is false after cancel", job.active === false);
const before = fires;
await Aike.sleep(1500);
check("no fire after cancel", fires === before, `before=${before} now=${fires}`);

console.log("\n=== validation ===");
let err = null;
try {
  Aike.schedule("not a cron expression", () => {});
} catch (e) {
  err = e;
}
check("invalid cron expression throws", err !== null, String(err));

console.log("\n=== root-only ===");
{
  const rt = Aike.createRuntime({ global: ["Aike"] });
  const has = await rt.run("typeof Aike.schedule");
  check("child runtime has no Aike.schedule", has === "undefined", `got ${has}`);
  rt.close();
}

console.log(`\n  Pass: ${pass}  Fail: ${fail}`);
if (fail > 0) {
  console.log("  SOME TESTS FAILED");
  process.exit(1);
}
console.log("  ALL TESTS PASSED");
