// Bare specifiers, which aike resolves through its module search path rather
// than Node's `node_modules` walk: the distribution modules directory, the
// importing module's own directory, the CWD, and `$AIKE_LIB_PATH` — in that
// order (see `module_loader::module_search_dirs`).
//
// Aike-specific: Node would look these up under `node_modules`, so this file
// is deliberately not part of the Node cross-check.
const check = require("./lib/check.cjs");

// A subpath of the importing module's directory.
check.eq("bare subpath from the importer dir", require("lib/probe.cjs").id, "probe");

// The importer's directory is searched relative to *this* module, not the
// process entry, so the same specifier resolves from inside a subdirectory.
const sub = require("./lib/sub-require.cjs");
check.eq("bare subpath from a nested module", sub.id, "probe");

// require.resolve agrees with require.
check.ok(
    "resolve agrees",
    require.resolve("lib/probe.cjs") === require.resolve("./lib/probe.cjs")
);

// require.resolve.paths lists the search directories, importer dir first
// among the file-backed ones.
const paths = require.resolve.paths("anything");
check.ok("resolve.paths is an array", Array.isArray(paths));
check.ok("resolve.paths includes the importer dir", paths.includes(__dirname));

// A bare name that matches no file is a plain resolution failure, and it does
// not disturb modules that already loaded.
check.throws("unknown bare specifier", () => require("no-such-package"), "MODULE_NOT_FOUND");
check.eq("earlier module still cached", require("lib/probe.cjs").id, "probe");

check.done();
