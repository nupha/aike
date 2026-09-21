// Child fixture shared by the signal delivery tests.
//
//   Windows: tests/signals/ctrlc-test.ps1 injects a console CTRL_C_EVENT.
//   Unix:    tests/signals/signal.mjs sends the signal via process.kill(pid).
//
// Each scenario registers handler(s) for `sig` and prints READY; the harness
// then delivers one signal and inspects this process's output/exit.
//
// Usage: aike -m tests/signals/fixture.mjs <scenario> [signal]
// process.argv = [aikeExe, scriptPath, scenario, signal?]
const scenario = process.argv[2];
// Windows CTRL_C_EVENT maps to SIGINT; Unix tests pass SIGUSR1 explicitly for
// the "restore default action" cases.
const sig = process.argv[3] || "SIGINT";

switch (scenario) {
  // no handler at all: the OS/default handler must act and terminate.
  case "control":
    break;

  // single handler must fire.
  case "simple":
    Aike.onSignal(sig, () => {
      console.log("HANDLER-fired");
      process.exit(0);
    });
    break;

  // handler must survive explicit GCs (it is traced through AikeRuntimeState).
  case "gc":
    Aike.onSignal(sig, () => {
      console.log("HANDLER-fired");
      process.exit(0);
    });
    for (let i = 0; i < 8; i++) {
      const junk = [];
      for (let k = 0; k < 5000; k++) junk.push({ x: k, s: "s" + k });
      Aike.gc();
    }
    break;

  // last registration wins.
  case "overwrite":
    Aike.onSignal(sig, () => {
      console.log("HANDLER-first");
      process.exit(0);
    });
    Aike.onSignal(sig, () => {
      console.log("HANDLER-second");
      process.exit(0);
    });
    break;

  // unregister via null: user handler must NOT fire; default action takes over.
  case "unregister":
    Aike.onSignal(sig, () => {
      console.log("HANDLER-should-not-fire");
    });
    Aike.onSignal(sig, null);
    break;

  // register -> unregister -> register: the second handler must fire.
  case "reregister":
    Aike.onSignal(sig, () => {
      console.log("HANDLER-first");
    });
    Aike.onSignal(sig, null);
    Aike.onSignal(sig, () => {
      console.log("HANDLER-second");
      process.exit(0);
    });
    break;

  default:
    throw new Error("unknown scenario: " + scenario);
}

console.log("READY");
setInterval(() => {}, 1000);
