// A `.cjs` file is CommonJS even when it *looks* like an ES module to a
// scanner — the extension is authoritative (Node's rule).

// `export` is a syntax error inside a CommonJS wrapper, so the file cannot
// both carry these tokens and load. What is verifiable here is that the `.cjs`
// extension alone selects the CommonJS loader: no `import`/`export` syntax
// appears anywhere in this file, yet `require`/`module` are present.

const check = require("./lib/check.cjs");

check.ok(".cjs entry is CommonJS", typeof module === "object");
check.ok(".cjs entry has require", typeof require === "function");
check.eq(".cjs entry is require.main", require.main === module, true);
check.eq(".cjs entry can require", require("./lib/probe.cjs").id, "probe");

check.done();
