// Fixture: one spawn scenario driven by argv flags, run under a nested aike.
//   --abort          spawn with abortOnParentExit:true (default: no option)
//   --wait           await r.exitCode after printing pid (keeps process alive)
//   --process-exit   process.exit(0) after printing pid
//   --seconds <n>    child sleep seconds (default 100)
import { sleepCmd } from "./platform.mjs";

const argv = process.argv.slice(2);
const seconds = (() => {
  const i = argv.indexOf("--seconds");
  return i >= 0 ? Number(argv[i + 1]) : 100;
})();
const opts = argv.includes("--abort") ? { abortOnParentExit: true } : {};

const [cmd, args] = sleepCmd(seconds);
const r = Aike.spawn(cmd, args, opts);
console.log(r.pid);
if (argv.includes("--process-exit")) process.exit(0);
if (argv.includes("--wait")) console.log("code", await r.exitCode);