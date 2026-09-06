// Type declarations for the "jinja2" module
// import jinja2 from "jinja2";
// const { renderTemplate } = jinja2;

type Jinja2Value =
  | string
  | number
  | boolean
  | null
  | Jinja2Value[]
  | { [key: string]: Jinja2Value };

declare module "jinja2" {
  /**
   * Render a jinja2 template string with variables.
   * Uses aike's minijinja engine (Python-compatible syntax).
   *
   * @example
   * ```typescript
   * import jinja2 from "jinja2";
   * const { renderTemplate } = jinja2;
   *
   * const result = renderTemplate(
   *   "Hello {{ name }}!{% if admin %} Admin{% endif %}",
   *   { name: "World", admin: true }
   * );
   * // result: "Hello World! Admin"
   * ```
   *
   * @param template - jinja2 template string
   * @param variables - Template variables (object with string keys)
   * @returns Rendered string
   */
  export default {
    renderTemplate(
      template: string,
      variables?: Record<string, Jinja2Value>,
    ): string;
  };
}
