globalThis.__counterRuns = (globalThis.__counterRuns || 0) + 1;
module.exports = { n: globalThis.__counterRuns };
