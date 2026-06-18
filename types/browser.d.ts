// Type declarations for the "browser" module
// import { launch } from "browser";

declare module "browser" {
  interface LaunchOptions {
    /** Run browser in headless mode. Default: true */
    headless?: boolean;
    /** Proxy server URL (e.g. "http://127.0.0.1:7890"). */
    proxyUrl?: string;
    /** Custom Chrome/Chromium executable path. Auto-detected when omitted. */
    chromePath?: string;
    /** Remote debugging port. Default: 9222 */
    port?: number;
  }

  /** A Chrome browser instance. Cannot be constructed directly. */
  interface Browser {
    /** Create a new tab (page). */
    newTab(): Promise<Tab>;

    /** Close the browser and release all resources. */
    close(): Promise<void>;

    /** Whether the browser process is still running. */
    readonly isRunning: boolean;
  }

  /** Screenshot capture options. */
  interface ScreenshotOptions {
    /** Image format. Default: "png" */
    format?: "png" | "jpeg";
    /** Image quality (1-100, JPEG only). Default: 80 */
    quality?: number;
    /** Capture full page (scrollable content). Default: false */
    fullPage?: boolean;
  }

  /** A single browser cookie. */
  interface Cookie {
    name: string;
    value: string;
    domain?: string;
    path?: string;
    httpOnly?: boolean;
    secure?: boolean;
    sameSite?: "Strict" | "Lax" | "None";
    expires?: number;
  }

  /** PDF export options. */
  interface PdfOptions {
    landscape?: boolean;
    paperWidth?: number;
    paperHeight?: number;
    /** Print background graphics. Default: true */
    printBackground?: boolean;
  }

  /** A single browser tab/page. Cannot be constructed directly. */
  interface Tab {
    /** Navigate to a URL. Resolves when the page starts loading. */
    navigate(url: string): Promise<void>;

    /**
     * Wait for a CSS selector to appear on the page.
     * @param selector  - CSS selector string.
     * @param timeoutMs - Timeout in milliseconds. Default: 30000.
     */
    waitForSelector(selector: string, timeoutMs?: number): Promise<void>;

    /**
     * Execute JavaScript code in the page context.
     * @param jsCode - JavaScript source to evaluate.
     * @returns The JSON-serialized result.
     */
    evaluate(jsCode: string): Promise<any>;

    /** Get the full outerHTML of the document. */
    getOuterHTML(): Promise<string>;

    /** Capture a screenshot of the current page. */
    captureScreenshot(options?: ScreenshotOptions): Promise<Uint8Array>;

    /** Set the browser window / device viewport size. */
    setViewport(width: number, height: number): Promise<void>;

    /** Get all cookies for the current page. */
    getCookies(): Promise<Cookie[]>;

    /** Set a cookie for the current page. */
    setCookie(cookie: Cookie): Promise<void>;

    /** Wait for the page to finish navigating / fully load. */
    waitForNavigation(timeoutMs?: number): Promise<void>;

    /**
     * Accept or dismiss a JavaScript dialog (alert/confirm/prompt).
     * @param accept    - true=accept/OK, false=dismiss/cancel.
     * @param promptText - Text to enter for prompt dialogs.
     */
    handleDialog(accept: boolean, promptText?: string): Promise<void>;

    /** Export the current page as PDF (Chromium-specific). */
    printToPDF(options?: PdfOptions): Promise<Uint8Array>;

    /**
     * Upload files to an <input type="file"> element.
     * @param selector  - CSS selector for the file input element.
     * @param filePaths - Absolute paths to the files to upload.
     */
    uploadFile(selector: string, filePaths: string[]): Promise<void>;

    /** Close this tab. */
    close(): Promise<void>;

    /** Unique target ID assigned by Chrome. */
    readonly targetId: string;
  }

  /** Launch a Chrome browser instance. */
  export function launch(options?: LaunchOptions): Promise<Browser>;
}
