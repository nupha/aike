// Type declarations for the "html" module
// import { parseHTML, html2markdown } from "html";

declare module "html" {
  export function parseHTML(html: string): Document;
  export function html2markdown(html: string): string;

  interface Document {
    /** Find all elements matching a CSS selector. */
    querySelectorAll(selector: string): Element[];
  }

  interface Element {
    /** The tag name (e.g. "div", "a"). */
    readonly tagName: string;
    /** All descendant text content concatenated. */
    readonly textContent: string;
    /** Immediate child elements. */
    readonly children: Element[];
    /** Get the value of an HTML attribute, or null if absent. */
    getAttribute(name: string): string | null;
    /** Find the first descendant matching a CSS selector, or null. */
    querySelector(selector: string): Element | null;
    /** Find all descendants matching a CSS selector. */
    querySelectorAll(selector: string): Element[];
  }
}
