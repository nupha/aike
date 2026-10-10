// Module resolution: relative and absolute specifiers, extension probing,
// directory indexes, JSON, and aike's bare-specifier search path.
const path = require("path");
const check = require("./lib/check.cjs");

// Exact filename.
check.eq(
    "explicit .cjs",
    require("./lib/probe.cjs").id,
    "probe"
);

// Extension probing — Node tries `.js`, `.json`, `.node`; aike also knows
// `.cjs` / `.ts` / `.cts`, and `.js` is probed first.
check.eq("probed .js", require("./lib/probe").id, "probe");
check.eq("probed .json", require("./lib/data").name, "aike");

// Directory with an index file (Node's LOAD_INDEX).
check.eq("directory index", require("./lib/dir-index"), "dir-index");

// `../` traversal.
check.eq("parent-relative", require("./lib/deep/../probe.cjs").id, "probe");
check.eq(
    "parent-relative dir",
    require("./lib/deep/nested/leaf.cjs").where,
    "nested/leaf.cjs"
);

// Absolute specifier.
const abs = path.join(__dirname, "lib", "probe.cjs");
check.eq("absolute", require(abs).id, "probe");

// JSON: parsed once, cached, and the same object on every require.
const json = require("./lib/data.json");
check.eq("json name", json.name, "aike");
check.eq("json nested", json.nested.ok, true);
check.eq("json list", json.list.length, 3);
check.ok("json is cached", require("./lib/data.json") === json);

// require.resolve / require.resolve.paths
check.eq("resolve exact", require.resolve("./lib/probe.cjs"), abs);
check.eq("resolve probed", require.resolve("./lib/probe"), abs);
check.ok("resolve.paths is a non-empty array", require.resolve.paths("./x").length > 0);

// Resolution is by resolved filename, so equivalent specifiers agree.
check.eq(
    "./lib/./x normalizes",
    require.resolve("./lib/./probe.cjs"),
    abs
);
check.eq(
    "../lib/x normalizes",
    require.resolve("./lib/dir-index/../probe.cjs"),
    abs
);

// Resolution failures.
check.throws("missing module", () => require("./nope"), "MODULE_NOT_FOUND");
check.throws("empty specifier", () => require(""), "MODULE_NOT_FOUND");
check.throws("missing dir index", () => require("./lib/dir-empty"), "MODULE_NOT_FOUND");
check.throws("non-string specifier", () => require(42), undefined);
check.throws("resolve of a missing module", () => require.resolve("./nope"), "MODULE_NOT_FOUND");

// The failure names the specifier and lists the require stack.
try {
    require("./nope");
} catch (e) {
    check.eq("error name is Error", e.name, "Error");
    check.ok("message names the specifier", e.message.startsWith("Cannot find module './nope'"));
    check.ok("message has a require stack", e.message.includes("Require stack:"));
    check.ok("require stack lists the requirer", e.message.includes("resolve.cjs"));
}

check.done();
