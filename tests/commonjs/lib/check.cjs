// Minimal assertion helper shared by the CommonJS tests.
// Runs unchanged on Node, so every test below is a Node-vs-aike comparison.

let pass = 0;
let fail = 0;

function record(ok, label, detail) {
    if (ok) {
        pass++;
        console.log(`  ✅ ${label}`);
    } else {
        fail++;
        console.log(`  ❌ ${label}${detail === undefined ? "" : " — " + detail}`);
    }
}

// Module records are cyclic (parent ↔ children), so a plain JSON.stringify
// would throw while *reporting* a failure.
function show(v) {
    if (typeof v === "string") return JSON.stringify(v);
    try {
        const s = JSON.stringify(v);
        return s === undefined ? String(v) : s.length > 120 ? `${s.slice(0, 117)}…` : s;
    } catch {
        return Object.prototype.toString.call(v);
    }
}

function same(a, b) {
    if (Object.is(a, b)) return true;
    if (typeof a !== "object" || typeof b !== "object" || a === null || b === null) return false;
    try {
        return JSON.stringify(a) === JSON.stringify(b);
    } catch {
        return false;
    }
}

exports.ok = (label, cond, detail) => record(!!cond, label, detail);

exports.eq = (label, actual, expected) =>
    record(same(actual, expected), label, `got ${show(actual)}, want ${show(expected)}`);

exports.throws = (label, fn, code) => {
    try {
        fn();
        record(false, label, "did not throw");
    } catch (e) {
        record(
            code === undefined || e.code === code,
            label,
            code === undefined ? String(e && e.message) : `code ${e && e.code}, want ${code}`
        );
    }
};

exports.done = () => {
    console.log(`=== ${pass} passed, ${fail} failed ===`);
    if (fail > 0) {
        process.exit(1);
    }
};
