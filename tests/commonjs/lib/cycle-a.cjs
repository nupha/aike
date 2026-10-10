exports.done = false;
const b = require("./cycle-b.cjs");
exports.bSawADone = b.sawADone;
exports.done = true;
