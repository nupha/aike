// How the entry file's format is chosen, mirroring Node: an explicit
// extension wins, otherwise the source decides, and a non-module file is
// CommonJS.
//
// This file is CommonJS — a `.js` entry with no ES module syntax.

const check = require("./lib/check.cjs");

check.ok("entry runs as CommonJS", typeof module === "object" && typeof require === "function");
check.ok("this === module.exports", this === module.exports);
check.ok("require.main === module", require.main === module);
check.eq("module.id is '.' for the entry", module.id, ".");
check.ok("entry is cached under its filename", require.cache[__filename] === module);

// It can require a sibling `.cjs` file, which proves the loader is reachable
// from a plain `.js` entry.
check.eq("require from a .js entry", require("./lib/probe.cjs").id, "probe");

check.done();
