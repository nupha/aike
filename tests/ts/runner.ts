// Shared test runner — exports check/ok without circular deps.
// First token must be `export` so ModuleDef::detect_module recognizes it as module.

export const state = { pass: 0, fail: 0 };

export function ok(condition: boolean, detail?: string): void {
  if (!condition) throw new Error(detail ?? "assertion failed");
}

export function check(label: string, condition: boolean, detail?: string): void {
  if (condition) {
    console.log(`  ✅ ${label}`);
    state.pass++;
  } else {
    console.log(`  ❌ ${label}${detail ? " — " + detail : ""}`);
    state.fail++;
  }
}

export function passed(): number { return state.pass; }
export function failed(): number { return state.fail; }
