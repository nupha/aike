// The CommonJS export shapes a real package uses.
const check = require("./lib/check.cjs");

// 1. `exports.x = …` — the shortcut, and the reason it stops working once
//    `module.exports` is reassigned.
const shortcut = require("./lib/shortcut.cjs");
check.eq("shortcut.a", shortcut.a, 1);
check.ok("shortcut exports is the shared object", shortcut.self === shortcut);

// 2. `module.exports = <object>`
const obj = require("./lib/obj-export.cjs");
check.eq("object export keys", Object.keys(obj).sort(), ["x", "y"]);
check.eq("object export value", obj.x + obj.y, 3);

// 3. `module.exports = <function>` — the "export a constructor" pattern.
const Square = require("./lib/fn-export.cjs");
check.eq("function export is callable", typeof Square, "function");
check.eq("function export works", new Square(3).area(), 9);
check.eq("function export keeps statics", Square.units, "px");

// 4. `module.exports = <class>`
const Point = require("./lib/class-export.cjs");
check.eq("class export is a class", typeof Point, "function");
check.eq("class export works", new Point(1, 2).len(), Math.sqrt(5));

// 5. Nothing exported at all → the default `{}`.
const empty = require("./lib/empty.cjs");
check.eq("empty export keys", Object.keys(empty), []);

// 6. `Object.defineProperty(exports, …)`
const defined = require("./lib/defined-export.cjs");
check.eq("defineProperty export", defined.v, 1);

// 7. Reassigning the local `exports` binding is inert.
const rebound = require("./lib/rebound.cjs");
check.eq("exports reassignment is inert", Object.keys(rebound), []);

check.done();
