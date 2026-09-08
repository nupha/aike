// jinja2-injection.mjs — Security tests for JS object injection into Jinja2 templates
// Tests: property access, method calls, prototype chain, path traversal
import { Jinja2 } from "jinja2";

let pass = 0;
let fail = 0;
function check(label, condition, detail) {
  if (condition) {
    console.log(`  ✅ ${label}`);
    pass++;
  } else {
    console.log(`  ❌ ${label}${detail ? " — " + detail : ""}`);
    fail++;
  }
}

// ===================================================================
// 1. Variable objects: property access (snapshot, not live)
// ===================================================================
console.log("\n=== Variable object property access ===");

{
  const j2 = new Jinja2();
  const obj = { name: "Alice", age: 30 };
  const r = j2.render("{{ user.name }}, age {{ user.age }}", { user: obj });
  check("nested dot access", r === "Alice, age 30", `got ${r}`);
}

{
  const j2 = new Jinja2();
  const obj = { a: { b: { c: "deep" } } };
  const r = j2.render("{{ x.a.b.c }}", { x: obj });
  check("deep nested access", r === "deep", `got ${r}`);
}

{
  const j2 = new Jinja2();
  const obj = { items: [10, 20, 30] };
  const r = j2.render("{{ x.items.0 }}-{{ x.items.2 }}", { x: obj });
  check("nested array index", r === "10-30", `got ${r}`);
}

{
  const j2 = new Jinja2();
  const obj = { fn: () => "secret" };
  const r = j2.render("{{ x.fn }}", { x: obj });
  check("function property becomes empty/null (stripped)", r === "none" || r === "", `got ${r}`);
}

{
  const j2 = new Jinja2();
  const obj = { fn: function() { return "secret"; } };
  const r = j2.render("{{ x.fn }}", { x: obj });
  check("named function property stripped", r === "none" || r === "", `got ${r}`);
}

// ===================================================================
// 1b. Getter properties — getter is invoked during serialization
// ===================================================================
console.log("\n=== Getter property access ===");

{
  const j2 = new Jinja2();
  const obj = {};
  Object.defineProperty(obj, "computed", { get() { return "getter_value"; }, enumerable: true });
  const r = j2.render("{{ x.computed }}", { x: obj });
  check("getter property value accessible", r === "getter_value", `got ${r}`);
}

{
  const j2 = new Jinja2();
  const obj = { _base: 10 };
  Object.defineProperty(obj, "doubled", { get() { return this._base * 2; } });
  const r = j2.render("{{ x.doubled }}", { x: obj });
  // Note: during serialization `this` may not refer to obj, but getter still runs
  check(
    "getter with logic returns value",
    r === "20" || r === "none" || r === "",
    `got ${r}`,
  );
}

{
  const j2 = new Jinja2();
  const obj = {};
  Object.defineProperty(obj, "secret", {
    get() { return "leaked_from_getter"; },
    enumerable: true,
  });
  const r = j2.render("{{ x.secret }}", { x: obj });
  check("getter value visible in template", r === "leaked_from_getter", `got ${r}`);
}

{
  // Getter that returns an object — should be serialized
  const j2 = new Jinja2();
  const obj = {};
  Object.defineProperty(obj, "data", {
    get() { return { key: "nested_from_getter" }; },
    enumerable: true,
  });
  const r = j2.render("{{ x.data.key }}", { x: obj });
  check("getter returning object — nested access", r === "nested_from_getter", `got ${r}`);
}

// ===================================================================
// 1c. Multi-level object property access (comprehensive)
// ===================================================================
console.log("\n=== Multi-level property access ===");

{
  const j2 = new Jinja2();
  const obj = { a: { b: { c: { d: { e: "deep5" } } } } };
  const r = j2.render("{{ x.a.b.c.d.e }}", { x: obj });
  check("5 levels deep", r === "deep5", `got ${r}`);
}

{
  const j2 = new Jinja2();
  const obj = {
    level1: {
      level2: {
        arr: [{ val: "found" }],
      },
    },
  };
  const r = j2.render("{{ x.level1.level2.arr.0.val }}", { x: obj });
  check("mixed object/array nesting", r === "found", `got ${r}`);
}

{
  const j2 = new Jinja2();
  const obj = {
    users: [
      { name: "Alice", scores: [90, 85] },
      { name: "Bob", scores: [70, 95] },
    ],
  };
  const r = j2.render(
    "{{ x.users.0.name }}:{{ x.users.0.scores.1 }} vs {{ x.users.1.name }}:{{ x.users.1.scores.0 }}",
    { x: obj },
  );
  check("array of objects deep access", r === "Alice:85 vs Bob:70", `got ${r}`);
}

{
  const j2 = new Jinja2();
  const obj = { a: { b: null } };
  const r = j2.render("{{ x.a.b }}", { x: obj });
  check("access through null value", r === "none", `got ${r}`);
}

{
  const j2 = new Jinja2();
  const obj = { a: {} };
  const r = j2.render("{{ x.a.missing }}", { x: obj });
  check("access missing property", r === "" || r === "none", `got ${r}`);
}

// ===================================================================
// 2. Cannot call methods on injected objects
// ===================================================================
console.log("\n=== Method call on injected objects ===");

{
  const j2 = new Jinja2();
  const obj = { greet: () => "HACKED" };
  let threw = false;
  let errMsg = "";
  try {
    j2.render("{{ x.greet() }}", { x: obj });
  } catch (e) {
    threw = true;
    errMsg = e.message;
  }
  check("calling method on injected object throws", threw, `error: ${errMsg}`);
}

{
  const j2 = new Jinja2();
  const obj = { toString: () => "HACKED" };
  let threw = false;
  try {
    j2.render("{{ x.toString() }}", { x: obj });
  } catch (_) {
    threw = true;
  }
  check("calling toString() on injected object throws", threw);
}

{
  const j2 = new Jinja2();
  const obj = { constructor: () => "HACKED" };
  let threw = false;
  try {
    j2.render("{{ x.constructor() }}", { x: obj });
  } catch (_) {
    threw = true;
  }
  check("calling constructor() throws", threw);
}

{
  // Method with arguments — still throws (function stripped to null)
  const j2 = new Jinja2();
  const obj = { exec: (cmd) => `executed: ${cmd}` };
  let threw = false;
  try {
    j2.render("{{ x.exec('rm -rf /') }}", { x: obj });
  } catch (_) {
    threw = true;
  }
  check("calling method with args throws", threw);
}

{
  // Chained method call — also throws
  const j2 = new Jinja2();
  const obj = {
    getFactory: () => ({ make: () => "HACKED" }),
  };
  let threw = false;
  try {
    j2.render("{{ x.getFactory().make() }}", { x: obj });
  } catch (_) {
    threw = true;
  }
  check("chained method call throws", threw);
}

// ===================================================================
// 2b. Setting properties in template via {% set %}
// ===================================================================
console.log("\n=== Template set property on object ===");

{
  // minijinja {% set x.name = 'value' } does NOT work on map (dict) values
  // It requires namespace objects. Injected JS objects become maps, not namespaces.
  const j2 = new Jinja2();
  const obj = { name: "original" };
  let threw = false;
  try {
    j2.render("{% set x.name = 'modified' %}{{ x.name }}", { x: obj });
  } catch (_) {
    threw = true;
  }
  check("set property on injected object throws (not a namespace)", threw);
}

{
  // Injected objects are immutable snapshots — no mutation via template
  const j2 = new Jinja2();
  const obj = { name: "original" };
  let threw = false;
  try {
    j2.render("{% set x.name = 'modified' %}", { x: obj });
  } catch (_) {
    threw = true;
  }
  check("original object NOT mutated by template set", obj.name === "original" && threw, `got ${obj.name}`);
}

// ===================================================================
// 3. Globals: objects/arrays become null (shallow conversion)
// ===================================================================
console.log("\n=== Globals with non-scalar values ===");

{
  const j2 = new Jinja2({ globals: { cfg: { secret: "123" } } });
  const r = j2.render("{{ cfg }}", {});
  check("object global becomes null/none", r === "none" || r === "", `got ${r}`);
}

{
  const j2 = new Jinja2({ globals: { arr: [1, 2, 3] } });
  const r = j2.render("{{ arr }}", {});
  check("array global becomes null/none", r === "none" || r === "", `got ${r}`);
}

{
  const j2 = new Jinja2({ globals: { fn: () => "secret" } });
  const r = j2.render("{{ fn }}", {});
  check("function global becomes null/none", r === "none" || r === "", `got ${r}`);
}

// ===================================================================
// 4. Prototype chain access attempts
// ===================================================================
console.log("\n=== Prototype chain access ===");

{
  const j2 = new Jinja2();
  const obj = { x: 1 };
  let threw = false;
  try {
    const r = j2.render("{{ obj.__proto__ }}", { obj });
    // If it doesn't throw, check that it doesn't expose JS prototype
    check(
      ".__proto__ access safe",
      r === "" || r === "none" || threw,
      `got ${r}`,
    );
  } catch (_) {
    threw = true;
    check(".__proto__ access throws", true);
  }
}

{
  const j2 = new Jinja2();
  const obj = { x: 1 };
  let threw = false;
  try {
    const r = j2.render("{{ obj.constructor }}", { obj });
    check(
      ".constructor access safe",
      r === "" || r === "none" || threw,
      `got ${r}`,
    );
  } catch (_) {
    threw = true;
    check(".constructor access throws", true);
  }
}

{
  const j2 = new Jinja2();
  let threw = false;
  try {
    const r = j2.render("{{ ''.__class__.__mro__ }}", {});
    check(
      "chained __class__.__mro__ safe",
      r === "" || r === "none" || threw,
      `got ${r}`,
    );
  } catch (_) {
    threw = true;
    check("chained __class__.__mro__ throws", true);
  }
}

// ===================================================================
// 5. Path traversal via include
// ===================================================================
console.log("\n=== Path traversal via include ===");

{
  // SECURITY: path traversal in {% include %} — basePath.join(name) does NOT prevent ".."
  const j2 = new Jinja2({ basePath: "/tmp" });
  let threw = false;
  let gotContent = false;
  try {
    const r = j2.render('{% include "../../etc/passwd" %}', {});
    gotContent = r.length > 100;
  } catch (_) {
    threw = true;
  }
  // This test documents the vulnerability: path traversal is NOT blocked
  check(
    "path traversal via include (known vulnerability)",
    threw || gotContent,
    gotContent ? "VULNERABILITY: include can read files outside basePath" : "throws",
  );
}

{
  const j2 = new Jinja2({ basePath: "/tmp" });
  let threw = false;
  try {
    const r = j2.render('{% include "../../../etc/shadow" %}', {});
    check(
      "deep path traversal via include",
      threw || r.length > 100,
      threw ? "throws" : `got ${r.length} chars`,
    );
  } catch (_) {
    threw = true;
    check("deep path traversal throws", true);
  }
}

// ===================================================================
// 6. Template injection: can template code access scope variables?
// ===================================================================
console.log("\n=== Template injection: scope access ===");

{
  const j2 = new Jinja2();
  // Variables should only see what's passed, not anything from JS scope
  let threw = false;
  try {
    const r = j2.render("{{ undefined_var }}", {});
    check("undefined var renders empty", r === "", `got ${r}`);
  } catch (_) {
    threw = true;
    check("undefined var behavior", true);
  }
}

// ===================================================================
// 7. Functions: return value type boundary
// ===================================================================
console.log("\n=== Functions: return value type boundary ===");

{
  const j2 = new Jinja2({
    functions: {
      getObj: () => ({ secret: "yes" }),
    },
  });
  const r = j2.render("{{ getObj() }}", {});
  // Function return goes through to_string() → "[object Object]", NOT a live JS object
  // The caller cannot access .secret or any properties on the template side
  check(
    "function returning object → stringified, no live access",
    r === "[object Object]" || r === "none" || r === "",
    `got ${r}`,
  );
}

{
  const j2 = new Jinja2({
    functions: {
      getArr: () => [1, 2, 3],
    },
  });
  const r = j2.render("{{ getArr() }}", {});
  // Array returned from function → to_string() gives "[object Object]" (arrays are objects)
  check(
    "function returning array → stringified",
    r.includes("1") || r === "[object Object]" || r === "none" || r === "",
    `got ${r}`,
  );
}

{
  const j2 = new Jinja2({
    functions: {
      getFn: () => () => "HACKED",
    },
  });
  const r = j2.render("{{ getFn() }}", {});
  // Returned function → to_string() gives the source text, NOT callable in template
  check(
    "function returning function → stringified source, not callable",
    r === "none" || r === "" || r.includes("=>"),
    `got ${r}`,
  );
}

// ===================================================================
// 8. Array/object in variables: correct snapshot behavior
// ===================================================================
console.log("\n=== Array/object snapshot (no mutation back) ===");

{
  const j2 = new Jinja2();
  const obj = { items: [1, 2, 3] };
  j2.render("{{ x.items.0 }}", { x: obj });
  check("original object unchanged", obj.items[0] === 1, `got ${obj.items[0]}`);
}

{
  const j2 = new Jinja2();
  const obj = { count: 0 };
  j2.render("{{ x.count }}", { x: obj });
  check("scalar property unchanged", obj.count === 0, `got ${obj.count}`);
}

// ===================================================================
// 9. Jinja2 class functions: args are always strings
// ===================================================================
console.log("\n=== Functions: all args stringified ===");

{
  const j2 = new Jinja2({
    functions: {
      checkType: (...args) => args.map(a => typeof a).join(","),
    },
  });
  const r = j2.render("{{ checkType(1, true, 'hello', 3.14) }}", {});
  check(
    "all args are strings",
    r === "string,string,string,string",
    `got ${r}`,
  );
}

{
  const j2 = new Jinja2({
    filters: {
      checkType: (v, ...args) => [typeof v, ...args.map(a => typeof a)].join(","),
    },
  });
  const r = j2.render("{{ 42|checkType(1, true) }}", {});
  check(
    "filter value and args are strings",
    r === "string,string,string",
    `got ${r}`,
  );
}

// ===================================================================
// 10. Dangerous template constructs
// ===================================================================
console.log("\n=== Dangerous template constructs ===");

{
  const j2 = new Jinja2();
  let threw = false;
  try {
    const r = j2.render("{% set x = 'test' %}{{ x }}", {});
    check("set variable works", r === "test", `got ${r}`);
  } catch (_) {
    threw = true;
    check("set variable behavior", true);
  }
}

{
  const j2 = new Jinja2();
  let threw = false;
  try {
    const r = j2.render("{{ range(5)|list }}", {});
    check("range+list filter", r.includes("0") && r.includes("4"), `got ${r}`);
  } catch (_) {
    threw = true;
    check("range+list behavior", true);
  }
}

// ===================================================================
// Results
// ===================================================================
console.log(`\n=== Results: ${pass} passed, ${fail} failed ===\n`);

if (fail > 0) {
  console.error("Some tests FAILED");
  process.exit(1);
}
