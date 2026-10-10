exports.sawADone = null;
// a is already in the cache here, so this is the same (partial) exports.
const a = require("./cycle-a.cjs");
exports.sawADone = a.done;
