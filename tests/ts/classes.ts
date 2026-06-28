// TypeScript class/ OOP test suite.
import { check } from "./runner.ts";

export function test_class_basic(): void {
  class Point {
    x: number;
    y: number;
    constructor(x: number, y: number) { this.x = x; this.y = y; }
    add(other: Point): Point { return new Point(this.x + other.x, this.y + other.y); }
    magnitude(): number { return Math.sqrt(this.x * this.x + this.y * this.y); }
  }
  const p1 = new Point(3, 4);
  check("class: field access", p1.x === 3 && p1.y === 4);
  check("class: method", Math.abs(p1.magnitude() - 5) < 0.001);
  const p3 = p1.add(new Point(1, 2));
  check("class: returns typed", p3.x === 4 && p3.y === 6);
}

export function test_class_inheritance(): void {
  class Animal {
    name: string;
    constructor(name: string) { this.name = name; }
    speak(): string { return `${this.name} makes a sound`; }
  }
  class Dog extends Animal {
    breed: string;
    constructor(name: string, breed: string) { super(name); this.breed = breed; }
    speak(): string { return `${this.name} barks`; }
    fetch(): string { return `${this.name} fetches`; }
  }
  const dog = new Dog("Rex", "Husky");
  check("inheritance: parent field", dog.name === "Rex");
  check("inheritance: override", dog.speak() === "Rex barks");
  check("inheritance: child method", dog.fetch() === "Rex fetches");
  check("inheritance: instanceof", dog instanceof Dog && dog instanceof Animal);
}

export function test_class_abstract(): void {
  abstract class Logger {
    abstract log(msg: string): void;
    info(msg: string): void { this.log(`[INFO] ${msg}`); }
    warn(msg: string): void { this.log(`[WARN] ${msg}`); }
  }
  class ConsoleLogger extends Logger {
    messages: string[] = [];
    log(msg: string): void { this.messages.push(msg); }
  }
  const logger = new ConsoleLogger();
  logger.info("hello");
  logger.warn("world");
  check("abstract: info", logger.messages[0] === "[INFO] hello");
  check("abstract: warn", logger.messages[1] === "[WARN] world");
}

export function test_class_implements(): void {
  interface Drawable { draw(): string; resize(factor: number): void; }
  class Square implements Drawable {
    size: number;
    constructor(size: number) { this.size = size; }
    draw(): string { return `square(${this.size})`; }
    resize(f: number): void { this.size *= f; }
  }
  const sq = new Square(10);
  check("implements: draw", sq.draw() === "square(10)");
  sq.resize(2);
  check("implements: resize", sq.size === 20);
}

export function test_class_access_modifiers(): void {
  class Person {
    public name: string;
    private age: number;
    protected id: string;
    constructor(name: string, age: number, id: string) {
      this.name = name; this.age = age; this.id = id;
    }
    getAge(): number { return this.age; }
  }
  class Employee extends Person {
    getProtectedId(): string { return this.id; }
  }
  check("public: accessible", new Person("A", 30, "X1").name === "A");
  check("private: via method", new Person("A", 30, "X1").getAge() === 30);
  check("protected: via child", new Employee("B", 25, "X2").getProtectedId() === "X2");
}

export function test_class_getset(): void {
  class Temperature {
    private _celsius: number = 0;
    get celsius(): number { return this._celsius; }
    set celsius(v: number) { this._celsius = v; }
    get fahrenheit(): number { return this._celsius * 9 / 5 + 32; }
    set fahrenheit(v: number) { this._celsius = (v - 32) * 5 / 9; }
  }
  const t = new Temperature();
  t.celsius = 100;
  check("getter/setter: celsius", t.celsius === 100);
  check("getter/setter: fahrenheit value", t.fahrenheit === 212);
  t.fahrenheit = 32;
  check("getter/setter: set fahrenheit", t.celsius === 0);
}

export function test_class_static(): void {
  class MathUtils {
    static PI: number = 3.14159;
    static add(a: number, b: number): number { return a + b; }
    static counter: number = 0;
    static increment(): number { return ++MathUtils.counter; }
  }
  check("static: field", MathUtils.PI === 3.14159);
  check("static: method", MathUtils.add(2, 3) === 5);
  check("static: mutable", MathUtils.increment() === 1 && MathUtils.increment() === 2);
}
