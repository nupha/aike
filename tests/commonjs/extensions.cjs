// `require.extensions` drives loading, and a replaced loader really is used —
// the documented way to teach `require()` a new file type.
const check = require("./lib/check.cjs");

// aike's default set. Node ships `.js`, `.json` and `.node`.
check.ok("has .js", typeof require.extensions[".js"] === "function");
check.ok("has .json", typeof require.extensions[".json"] === "function");

// Registering an extension under a new name and pointing it at the `.js`
// loader — Node's own documented recipe.
require.extensions[".sjs"] = require.extensions[".js"];
const sjs = require("./lib/probe.sjs");
check.eq("custom extension loads", sjs.id, "probe");
check.eq(
    "custom extension key in cache",
    typeof require.cache[require.resolve("./lib/probe.sjs")],
    "object"
);

// A replaced `.js` loader takes over, and the replacement is restored
// afterwards so the rest of this file still loads normally.
const jsLoader = require.extensions[".js"];
require.extensions[".js"] = (module, filename) => {
    module.exports = { hijacked: filename.split("/").pop() };
};
check.eq("replaced loader used", require("./lib/obj-export.cjs").hijacked, "obj-export.cjs");
require.extensions[".js"] = jsLoader;
check.eq("restored loader works", require("./lib/probe.cjs").id, "probe");

// A module loaded by the custom loader is cached like any other.
check.ok("custom-loaded module is cached", require("./lib/probe.sjs") === sjs);

// An unregistered extension falls back to the `.js` loader (Node's
// `Module._extensions[ext] || Module._extensions['.js']`), so replacing `.js`
// governs `.cjs` files too.
check.eq("unregistered ext uses .js loader", require("./lib/probe.weird").id, "probe");

check.done();
