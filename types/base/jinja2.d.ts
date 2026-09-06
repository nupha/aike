// Type declarations for the "jinja2" module
// import { Jinja2 } from "jinja2";
// import jinja2 from "jinja2";

type Jinja2Value =
  | string
  | number
  | boolean
  | null
  | Jinja2Value[]
  | { [key: string]: Jinja2Value };

type Jinja2Functions = {
  [key: string]: (...args: any[]) => any;
};

type Jinja2Filters = {
  [key: string]: (value: any, ...args: any[]) => any;
};

type Jinja2Globals = {
  [key: string]: any;
};

declare module "jinja2" {
  /**
   * Jinja2 template renderer with custom JS callback, filter, global and include support.
   *
   * @example
   * ```typescript
   * import { Jinja2 } from "jinja2";
   *
   * const j2 = new Jinja2({
   *   basePath: "/templates",
   *   functions: {
   *     "getUserName": () => "Alice",
   *   },
   *   filters: {
   *     "currency": (v) => `$${v}`,
   *   },
   *   globals: {
   *     "APP_NAME": "MyApp",
   *   }
   * });
   *
   * // Render inline template
   * const result = j2.render("Welcome to {{ APP_NAME }}", {});
   *
   * // Render template file (with include support)
   * const result2 = j2.render("main.j2", {});
   * ```
   */
  export class Jinja2 {
    constructor(options?: {
      functions?: Jinja2Functions;
      filters?: Jinja2Filters;
      globals?: Jinja2Globals;
      /** Base path for template includes (default: cwd) */
      basePath?: string;
    });

    /**
     * Render a jinja2 template string or file with variables.
     *
     * @param template - jinja2 template string or template filename
     * @param variables - Template variables (object with string keys)
     * @returns Rendered string
     */
    render(template: string, variables?: Record<string, Jinja2Value>): string;
  }

  /**
   * Standalone renderTemplate function (no custom callbacks/filters/includes).
   *
   * @example
   * ```typescript
   * import jinja2 from "jinja2";
   * const { renderTemplate } = jinja2;
   *
   * const result = renderTemplate("Hello {{ name }}!", { name: "World" });
   * // result: "Hello World!"
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
