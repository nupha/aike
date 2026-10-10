// The module cache and the parent/children graph.
const check = require("./lib/check.cjs");

// A module runs once, no matter how many times it is required.
const a1 = require("./lib/counter.cjs");
const a2 = require("./lib/counter.cjs");
check.eq("counter runs once", a1.n, 1);
check.ok("same object identity", a1 === a2);

// The record is reachable from the cache under its absolute filename.
const counterFile = require.resolve("./lib/counter.cjs");
const rec = require.cache[counterFile];
check.ok("cache holds the record", typeof rec === "object");
check.eq("record.exports", rec.exports, a1);
check.eq("record.loaded", rec.loaded, true);
check.eq("record.id is the filename", rec.id, counterFile);
check.eq("record.parent is this module", rec.parent, module);
check.ok("record is in children", module.children.includes(rec));

// A child is listed once even when required twice.
require("./lib/counter.cjs");
check.eq(
    "children not duplicated",
    module.children.filter((m) => m.id === counterFile).length,
    1
);

// Nested graph: parent/children link all the way down.
const mid = require("./lib/mid.cjs");
check.eq("nested child value", mid.loadedChild, "leaf");
check.eq(
    "nested child.parent",
    mid.rec.parent.filename,
    require.resolve("./lib/mid.cjs")
);

// The cache is keyed by the resolved filename, so two different specifiers
// that reach one file share a single record.
const viaDot = require("./lib/./counter.cjs");
const viaParent = require("./lib/deep/../counter.cjs");
check.ok("specifier aliases share the record", viaDot === a1 && viaParent === a1);

// Deleting a cache entry makes the next require re-run the module.
delete require.cache[counterFile];
check.eq("entry deleted", require.cache[counterFile], undefined);
const a3 = require("./lib/counter.cjs");
check.eq("counter re-ran", a3.n, 2);
check.ok("re-run yields a fresh object", a3 !== a1);

// Setting a cache entry to undefined also forces a reload (Node allows this).
require.cache[require.resolve("./lib/probe.cjs")] = undefined;
check.eq("undefined cache entry forces reload", require("./lib/probe.cjs").id, "probe");

// `require.main` is the entry.
check.ok("require.main === module", require.main === module);

check.done();
