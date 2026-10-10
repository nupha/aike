exports.loadedChild = require("./deep/nested/leaf.cjs").leaf;
exports.rec = require.cache[require.resolve("./deep/nested/leaf.cjs")];
