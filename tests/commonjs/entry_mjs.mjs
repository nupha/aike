// A `.mjs` entry stays an ES module — and a global `require` is still there
// for the CommonJS files it pulls in.
//
// The last three assertions are an intentional aike extension: Node has no
// global `require` in ES module scope. aike injects one because it evaluates
// scripts in global scope and the generated ESM<->CJS interop source needs it.
import { name } from "./lib/cjsexp.cjs";
import def from "./lib/cjsfn.cjs";

let pass = 0;
let fail = 0;
const ok = (label, cond) => {
    if (cond) { pass++; console.log(`  ✅ ${label}`); }
    else { fail++; console.log(`  ❌ ${label}`); }
};

ok("esm entry has no CommonJS module", typeof module === "undefined");
ok("esm entry has no exports", typeof exports === "undefined");
ok("esm entry has no __filename", typeof __filename === "undefined");
ok("global require exists", typeof require === "function");
ok("imported named export", name === "exported");
ok("imported default export", typeof def === "function");
ok("require reaches CommonJS", require("./lib/probe.cjs").id === "probe");

console.log(`=== ${pass} passed, ${fail} failed ===`);
if (fail > 0) {
    process.exit(1);
}
