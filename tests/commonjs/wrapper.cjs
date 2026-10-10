// The module wrapper: the five CommonJS scope parameters, `this`, and the
// `module` record.
const check = require("./lib/check.cjs");

check.ok("require is a function", typeof require === "function");
check.ok("module is an object", typeof module === "object");
check.ok("exports is an object", typeof exports === "object");
check.ok("this === module.exports", this === module.exports);
check.ok("exports === module.exports", exports === module.exports);
check.ok("__filename is absolute", __filename.startsWith("/"));
check.eq("__filename basename", __filename.split("/").pop(), "wrapper.cjs");
check.ok(
    "__dirname is __filename's directory",
    __filename.startsWith(__dirname + "/"),
    `${__filename} vs ${__dirname}`
);

// Top-level declarations stay inside the module rather than leaking to the
// global object — the whole point of Node's wrapper.
const scopedToModule = 1;
function scopedHelper() {}
check.ok("var is module-scoped", typeof scopedToModule === "number");
check.eq("no global leak", typeof globalThis.scopedToModule, "undefined");
check.eq("no global function leak", typeof globalThis.scopedHelper, "undefined");

// The module record.
check.eq("module.id is '.' for the entry", module.id, ".");
check.eq("module.filename", module.filename, __filename);
check.eq("module.path", module.path, __dirname);
check.ok("module.loaded is false while running", module.loaded === false);
check.eq("module.parent is null for the entry", module.parent, null);
check.ok("module.children is an array", Array.isArray(module.children));
check.ok("module.paths is an array", Array.isArray(module.paths));
check.ok("module.require is a function", typeof module.require === "function");
check.ok("require.cache is an object", typeof require.cache === "object");
check.ok("require.extensions is an object", typeof require.extensions === "object");
check.ok("require.resolve is a function", typeof require.resolve === "function");
check.ok("require.resolve.paths is a function", typeof require.resolve.paths === "function");
check.ok("require.main === module", require.main === module);
check.ok("require.cache[__filename] === module", require.cache[__filename] === module);

// Children are listed in load order; `check.cjs` was required first.
check.eq("children count", module.children.length, 1);
check.eq("children[0] is check.cjs", module.children[0].id.endsWith("check.cjs"), true);

// module.require() loads as if from this module.
check.eq("module.require works", module.require("./lib/probe.cjs").id, "probe");

check.done();
