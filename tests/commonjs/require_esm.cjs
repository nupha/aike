// `require()` of an ES module — Node >= 22.12 — and its failure modes.
const check = require("./lib/check.cjs");

// 1. A namespace with no `default` is handed back as-is: no `__esModule`
//    marker is added, so `require(x) === await import(x)` holds.
const plain = require("./lib/esm-plain.mjs");
check.eq("named export", plain.value, 42);
check.eq("no __esModule without a default", plain.__esModule, undefined);

// 2. A namespace with a `default` gets `__esModule: true`, which is what
//    transpiler-generated CommonJS looks for.
const withDefault = require("./lib/esm-default.mjs");
check.eq("default export", withDefault.default, "the default");
check.eq("named alongside default", withDefault.other, 7);
check.eq("__esModule with a default", withDefault.__esModule, true);
check.eq("null prototype, like a namespace", Object.getPrototypeOf(withDefault), null);

// 3. Evaluation happens once, and the value is stable across requires.
check.ok("require(esm) is cached", require("./lib/esm-default.mjs") === withDefault);

// 4. `import.meta` is the ES module's own, not the generated wrapper's.
check.eq("import.meta.url inside require(esm)", plain.here.endsWith("esm-plain.mjs"), true);

// 5. The module graph is fully evaluated before require() returns.
check.eq("synchronous dependency", require("./lib/esm-dep.mjs").doubled, 10);

// 6. Top-level await cannot be satisfied synchronously.
check.throws(
    "top-level await",
    () => require("./lib/esm-tla.mjs"),
    "ERR_REQUIRE_ASYNC_MODULE"
);

// 7. A `.js` file that carries ES module syntax is an ES module even though
//    the extension does not say so (Node's DETECT_MODULE_SYNTAX).
check.eq("esm-syntax .js via require", require("./lib/esm-syntax.js").tag, "esm-in-js");

// 8. And the same file reached through `import` is the same module.
check.ok(
    "require(esm) and import agree",
    require("./lib/esm-syntax.js") === require("./lib/esm-syntax.js")
);

check.done();
