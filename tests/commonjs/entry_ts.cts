// TypeScript through the CommonJS loader.
//
// `.cts` is CommonJS by definition (Node's rule), so `require`/`module` are
// in scope even though the file is TypeScript. Node runs these same files, so
// every assertion below is a real Node-vs-aike comparison.

const check = require("./lib/check.cjs");

check.ok(".cts entry is CommonJS", typeof module === "object");
check.ok(".cts entry has require", typeof require === "function");
check.eq(".cts entry is require.main", require.main === module, true);
check.eq(".cts __filename is this file", __filename.endsWith("/entry_ts.cts"), true);

const mod = require("./lib/tsmod.ts");
check.eq("required .ts module", mod.label, "ts-module");
check.eq("required .ts function", mod.sum(2, 3), 5);
check.eq("required .ts types are gone", mod.origin, { x: 0, y: 0 });

// Requiring the same `.ts` twice returns the identical cached object.
check.eq("ts module is cached", require("./lib/tsmod.ts") === mod, true);

// TypeScript is *not* a registered extension: Node strips the types inside the
// ordinary loader, and `require.extensions` only ever lists `.js`/`.json`/`.node`.
check.eq("no .ts extension entry", require.extensions[".ts"], undefined);

// A `.ts` file that *is* an ES module is reachable through `require(esm)` —
// Node >= 22.12 — and comes back as the namespace itself.
const esm = require("./lib/tsesm.ts");
check.eq("require of ts esm yields namespace", esm.name, "ts-esm");
check.eq("ts esm namespace has null proto", Object.getPrototypeOf(esm), null);
// No `default` export, so Node's `__esModule` marker is absent.
check.eq("ts esm has no __esModule", esm.__esModule, undefined);

check.done();
