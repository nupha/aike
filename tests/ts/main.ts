// TypeScript transpilation integration test suite — Entry point.
// Run with: cargo run -- tests/ts/main.ts

import { passed, failed, check } from "./runner.ts";
import { greet, PI, add, VERSION, HelperCounter, isPositive, createPair } from "./helper.ts";

import { test_basic_types, test_interfaces, test_enums, test_generics, test_unions, test_tuples, test_literals, test_keyof_typeof } from "./basics.ts";
import { test_class_basic, test_class_inheritance, test_class_abstract, test_class_implements, test_class_access_modifiers, test_class_getset, test_class_static } from "./classes.ts";
import { test_type_guards, test_type_assertions, test_unknown_any, test_generators, test_rest_spread, test_optional_chaining } from "./advanced.ts";
import { test_as_const, test_definite_assignment, test_symbol, test_arrow_complex, test_indexed_access } from "./edge.ts";

// ===================================================================
// Basics
// ===================================================================
console.log("\n=== Type Annotations & Functions ==="); test_basic_types();
console.log("\n=== Interfaces ==="); test_interfaces();
console.log("\n=== Enums ==="); test_enums();
console.log("\n=== Generics ==="); test_generics();
console.log("\n=== Unions & Intersections ==="); test_unions();
console.log("\n=== Tuples ==="); test_tuples();
console.log("\n=== Literal Types ==="); test_literals();
console.log("\n=== Keyof / Typeof ==="); test_keyof_typeof();

// ===================================================================
// Classes
// ===================================================================
console.log("\n=== Class: Basic ==="); test_class_basic();
console.log("\n=== Class: Inheritance ==="); test_class_inheritance();
console.log("\n=== Class: Abstract ==="); test_class_abstract();
console.log("\n=== Class: Implements ==="); test_class_implements();
console.log("\n=== Class: Access Modifiers ==="); test_class_access_modifiers();
console.log("\n=== Class: Getter/Setter ==="); test_class_getset();
console.log("\n=== Class: Static ==="); test_class_static();

// ===================================================================
// Advanced
// ===================================================================
console.log("\n=== Type Guards ==="); test_type_guards();
console.log("\n=== Type Assertions ==="); test_type_assertions();
console.log("\n=== unknown & any ==="); test_unknown_any();
console.log("\n=== Generators ==="); test_generators();
console.log("\n=== Rest & Spread ==="); test_rest_spread();
console.log("\n=== Optional Chaining ==="); test_optional_chaining();

// ===================================================================
// Edge cases
// ===================================================================
console.log("\n=== as const ==="); test_as_const();
console.log("\n=== Definite Assignment ==="); test_definite_assignment();
console.log("\n=== Symbol ==="); test_symbol();
console.log("\n=== Arrow Complex Types ==="); test_arrow_complex();
console.log("\n=== Indexed Access Types ==="); test_indexed_access();

// ===================================================================
// Helper module import
// ===================================================================
console.log("\n=== Module Import (helper.ts) ===");
check("helper: PI", PI === 3.14159);
check("helper: VERSION", VERSION === "2.0.0");
check("helper: greet()", greet("World", { prefix: "<<", suffix: ">>" }) === "<<World>>");
check("helper: add()", add(10, 20) === 30);
check("helper: isPositive", isPositive(5) === true);
check("helper: isPositive false", isPositive(-1) === false);

const pair = createPair("key", 99);
check("helper: createPair", pair[0] === "key" && pair[1] === 99);

const hc = new HelperCounter(5);
hc.tick(3);
check("helper: class", hc.value === 8);

// ===================================================================
// Summary
// ===================================================================
const totalPass = passed();
const totalFail = failed();
console.log(`\n=== TypeScript transpile test suite: ${totalPass} passed, ${totalFail} failed ===`);
if (totalFail > 0) {
  console.log("Some tests FAILED");
  process.exit(1);
} else {
  console.log("All tests PASSED");
}
