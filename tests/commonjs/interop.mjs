// An ES module importing CommonJS files — Node's `cjs-module-lexer` view:
// `default` is `module.exports`, plus the named exports a static scan of the
// file can see.
import def, { name, answer } from "./lib/cjsexp.cjs";
import * as ns from "./lib/cjsexp.cjs";
import fn from "./lib/cjsfn.cjs";
import obj from "./lib/cjsobj.cjs";

let pass = 0;
let fail = 0;
function ok(label, cond, detail) {
    if (cond) {
        pass++;
        console.log(`  ✅ ${label}`);
    } else {
        fail++;
        console.log(`  ❌ ${label}${detail === undefined ? "" : " — " + detail}`);
    }
}
const eq = (label, actual, expected) =>
    ok(label, JSON.stringify(actual) === JSON.stringify(expected),
       `got ${JSON.stringify(actual)}, want ${JSON.stringify(expected)}`);

// The default export *is* module.exports.
eq("default is module.exports", def, { name: "exported", answer: 42 });

// Named exports found by the static scan.
eq("named: exports.name", name, "exported");
eq("named: exports.answer", answer, 42);

// The namespace mirrors it.
eq("namespace keys", Object.keys(ns).sort(), ["answer", "default", "name"]);
eq("namespace default", ns.default, def);
eq("namespace named", ns.answer, 42);

// `exports.x` seen only inside a function body is still detected, matching
// cjs-module-lexer.
import { lazy } from "./lib/cjslazy.cjs";
eq("named from a nested assignment", lazy, "lazy-value");

// `module.exports = <value>` shapes.
ok("function default is callable", typeof fn === "function");
eq("function default works", fn(4), 8);
eq("object default", obj.deep, { a: 1 });

// Assignment-style exports on `module.exports` are found too.
import { viaModule } from "./lib/cjsmod.cjs";
eq("named from module.exports.x", viaModule, "mod");

// An `Object.defineProperty(exports, name, { value })` descriptor is found;
// a getter-only descriptor is not (matching Node).
import { valued } from "./lib/cjsdefined.cjs";
eq("named from defineProperty value", valued, 1);

console.log(`=== ${pass} passed, ${fail} failed ===`);
if (fail > 0) {
    process.exit(1);
}
