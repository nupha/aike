// Circular requires: the record is published before the body runs, so a
// cycle sees the partially-filled `module.exports` instead of looping.
const check = require("./lib/check.cjs");

const a = require("./lib/cycle-a.cjs");

// a finished, and picked up what b set on the way through.
check.eq("cycle: a.exports.done", a.done, true);
// b ran while a was still mid-body, so it observed a.done === false — the
// partial exports object, exactly as Node does.
check.eq("cycle: b saw partial a", a.bSawADone, false);

check.done();
