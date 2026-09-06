// jinja2.mjs — Test jinja2 template rendering
import jinja2 from "jinja2";
const { renderTemplate } = jinja2;

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
// Module basics
// ===================================================================
console.log("\n=== Module basics ===");

check("renderTemplate is function", typeof renderTemplate === "function");

// ===================================================================
// Variable substitution
// ===================================================================
console.log("\n=== Variable substitution ===");

{
  const r = renderTemplate("Hello {{ name }}!", { name: "World" });
  check("simple variable", r === "Hello World!", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate("{{ a }} + {{ b }} = {{ c }}", { a: 1, b: 2, c: 3 });
  check("multiple variables", r === "1 + 2 = 3", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate("Value: {{ x }}", { x: null });
  check("null renders none", r === "Value: none", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate("Value: {{ x }}", { x: undefined });
  check("undefined renders empty", r === "Value: ", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate("Bool: {{ flag }}", { flag: true });
  check("boolean true", r === "Bool: true", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate("Bool: {{ flag }}", { flag: false });
  check("boolean false", r === "Bool: false", `got ${JSON.stringify(r)}`);
}

// ===================================================================
// Conditions (if/elif/else)
// ===================================================================
console.log("\n=== Conditions ===");

{
  const r = renderTemplate("{% if admin %}Admin{% else %}User{% endif %}", {
    admin: true,
  });
  check("if true", r === "Admin", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate("{% if admin %}Admin{% else %}User{% endif %}", {
    admin: false,
  });
  check("if false → else", r === "User", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate(
    "{% if score >= 90 %}A{% elif score >= 80 %}B{% else %}C{% endif %}",
    { score: 85 },
  );
  check("elif branch", r === "B", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate(
    "{% if score >= 90 %}A{% elif score >= 80 %}B{% else %}C{% endif %}",
    { score: 95 },
  );
  check("first if branch", r === "A", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate(
    "{% if score >= 90 %}A{% elif score >= 80 %}B{% else %}C{% endif %}",
    { score: 70 },
  );
  check("else branch", r === "C", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate("{% if items %}Has items{% endif %}", {
    items: [1, 2, 3],
  });
  check("if array truthy", r === "Has items", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate("{% if items %}Has items{% endif %}", { items: [] });
  check("if empty array falsy", r === "", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate("{% if name %}Hello {{ name }}{% endif %}", {
    name: "",
  });
  check("if empty string falsy", r === "", `got ${JSON.stringify(r)}`);
}

// ===================================================================
// For loops
// ===================================================================
console.log("\n=== For loops ===");

{
  const r = renderTemplate("{% for item in items %}{{ item }} {% endfor %}", {
    items: ["a", "b", "c"],
  });
  check("simple loop", r.trim() === "a b c", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate("{% for i in range(3) %}{{ i }} {% endfor %}", {});
  check("range loop", r.trim() === "0 1 2", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate(
    "{% for k in dict %}{{ k }}={{ dict[k] }} {% endfor %}",
    { dict: { x: 1, y: 2 } },
  );
  check(
    "dict loop",
    r.includes("x=1") && r.includes("y=2"),
    `got ${JSON.stringify(r)}`,
  );
}

{
  const r = renderTemplate(
    "{% for item in items %}{% if not loop.first %}, {% endif %}{{ item }}{% endfor %}",
    { items: ["a", "b", "c"] },
  );
  check("loop.first flag", r === "a, b, c", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate(
    "{% for item in items %}{% if not loop.last %}{{ item }}, {% endif %}{% endfor %}",
    { items: ["a", "b", "c"] },
  );
  check("loop.last flag", r === "a, b, ", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate(
    "{% for item in items %}{{ loop.index }}. {{ item }} {% endfor %}",
    { items: ["x", "y"] },
  );
  check(
    "loop.index (1-based)",
    r.includes("1. x") && r.includes("2. y"),
    `got ${JSON.stringify(r)}`,
  );
}

{
  const r = renderTemplate(
    "{% for item in items %}{{ loop.index0 }}:{{ item }} {% endfor %}",
    { items: ["x", "y"] },
  );
  check(
    "loop.index0 (0-based)",
    r.includes("0:x") && r.includes("1:y"),
    `got ${JSON.stringify(r)}`,
  );
}

// ===================================================================
// Filters
// ===================================================================
console.log("\n=== Filters ===");

{
  const r = renderTemplate("{{ name|upper }}", { name: "hello" });
  check("upper filter", r === "HELLO", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate("{{ name|lower }}", { name: "HELLO" });
  check("lower filter", r === "hello", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate("{{ name|capitalize }}", { name: "hello" });
  check("capitalize filter", r === "Hello", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate("{{ items|join(', ') }}", {
    items: ["a", "b", "c"],
  });
  check("join filter", r === "a, b, c", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate("{{ text|trim }}", { text: "  hello  " });
  check("trim filter", r === "hello", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate("{{ items|length }}", { items: [1, 2, 3] });
  check("length filter", r === "3", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate("{{ text|replace('world', 'AI') }}", {
    text: "hello world",
  });
  check("replace filter", r === "hello AI", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate("{{ text|default('N/A') }}", { text: "hello" });
  check("default filter with value", r === "hello", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate("{{ text|default('N/A') }}", {});
  check("default filter missing var", r === "N/A", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate("{{ data|tojson }}", { data: { key: "value" } });
  check(
    "tojson filter",
    r.includes('"key"') && r.includes('"value"'),
    `got ${JSON.stringify(r)}`,
  );
}

// Note: rsplit filter is only available in llama.cpp chat template rendering, not in the base jinja2 module.

// ===================================================================
// Whitespace control
// ===================================================================
console.log("\n=== Whitespace control ===");

{
  const r = renderTemplate("{%- if true %}yes{% endif -%}", {});
  check("-%} whitespace stripped", r === "yes", `got ${JSON.stringify(r)}`);
}

// ===================================================================
// Built-in functions
// ===================================================================
console.log("\n=== Built-in functions ===");

{
  let threw = false;
  try {
    renderTemplate("{{ raise_exception('error') }}", {});
  } catch (_) {
    threw = true;
  }
  check("raise_exception throws", threw);
}

// ===================================================================
// Nested data access
// ===================================================================
console.log("\n=== Nested data access ===");

{
  const r = renderTemplate("{{ user.name }}", { user: { name: "Alice" } });
  check("dot notation", r === "Alice", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate("{{ user.age }}", { user: { age: 30 } });
  check("dot notation number", r === "30", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate("{{ items.0 }}", { items: ["first", "second"] });
  check("array index access", r === "first", `got ${JSON.stringify(r)}`);
}

// ===================================================================
// Template inheritance / includes (basic)
// ===================================================================
console.log("\n=== Template string concatenation ===");

{
  const base = "Hello {{ name }}!";
  const r = renderTemplate(base, { name: "Test" });
  check(
    "simple template string",
    r === "Hello Test!",
    `got ${JSON.stringify(r)}`,
  );
}

// ===================================================================
// Complex template (system prompt style)
// ===================================================================
console.log("\n=== Complex template (system prompt style) ===");

{
  const template = `You are {{ role }}.

{% if capabilities %}
## Capabilities
{% for cap in capabilities %}
- {{ cap }}
{% endfor %}
{% endif %}

{% if rules %}
## Rules
{% for rule in rules %}
{{ loop.index }}. {{ rule }}
{% endfor %}
{% endif %}

{% if not strict %}Be helpful.{% endif %}`;

  const result = renderTemplate(template, {
    role: "an AI assistant",
    capabilities: ["coding", "analysis", "writing"],
    rules: ["Be concise", "No guessing"],
    strict: false,
  });

  check("complex template contains role", result.includes("an AI assistant"));
  check("complex template contains capabilities", result.includes("coding"));
  check("complex template contains rules", result.includes("1. Be concise"));
  check("complex template contains fallback", result.includes("Be helpful."));
}

// ===================================================================
// Edge cases
// ===================================================================
console.log("\n=== Edge cases ===");

{
  const r = renderTemplate("no variables here", {});
  check("no variables", r === "no variables here", `got ${JSON.stringify(r)}`);
}

{
  const r = renderTemplate("", {});
  check("empty template", r === "", `got ${JSON.stringify(r)}`);
}

{
  let threw = false;
  try {
    renderTemplate("{{ }}", {});
  } catch (_) {
    threw = true;
  }
  check("empty variable expression throws", threw);
}

{
  let threw = false;
  try {
    renderTemplate("{% %}", {});
  } catch (_) {
    threw = true;
  }
  check("empty block throws", threw);
}

{
  const r = renderTemplate("{{ a.b.c.d }}", { a: { b: { c: { d: "deep" } } } });
  check("deep nested access", r === "deep", `got ${JSON.stringify(r)}`);
}

// ===================================================================
// Error handling
// ===================================================================
console.log("\n=== Error handling ===");

{
  let threw = false;
  try {
    renderTemplate("{{ invalid syntax }}", {});
  } catch (_) {
    threw = true;
  }
  check("syntax error throws", threw);
}

{
  let threw = false;
  try {
    renderTemplate("{% if %}", {});
  } catch (_) {
    threw = true;
  }
  check("incomplete if throws", threw);
}

// ===================================================================
// Summary
// ===================================================================
console.log(`\n=== Results: ${pass} passed, ${fail} failed ===\n`);

if (fail > 0) {
  console.error("Some tests FAILED");
  process.exit(1);
}
