// `require()` of aike's native modules — the role Node's `require('fs')`
// builtins play.
const check = require("./lib/check.cjs");

const fs = require("fs");
check.ok("fs is an object", typeof fs === "object");
check.ok("fs.readText exists", typeof fs.readText === "function");
check.ok("fs.glob exists", typeof fs.glob === "function");

const path = require("path");
check.eq("path.sep", path.sep, "/");
check.eq("path.join", path.join("a", "b", "c"), "a/b/c");
check.eq("path.basename", path.basename("/x/y/z.txt"), "z.txt");

// The `node:` prefix is accepted and answers to the same module.
check.ok("node: prefix resolves", typeof require("node:fs").readText === "function");
check.ok("node: prefix yields the same object", require("node:fs") === fs);

// Native modules are not files, so they stay out of `require.cache`.
check.ok(
    "native module is not in the cache",
    !Object.keys(require.cache).includes("fs")
);
check.ok(
    "cache holds only files",
    Object.keys(require.cache).every((k) => k.startsWith("/"))
);

// A native module reached from an ES module is the same object.
check.done();
