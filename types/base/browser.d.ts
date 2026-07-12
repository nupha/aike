// Type declarations for the "browser" module
// import { Browser } from "browser";

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
    newPage(): Promise<Page>;

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

  /** Mouse click options. */
  interface MouseClickOptions {
    /** Mouse button: "left" (default), "middle", "right". */
    button?: "left" | "middle" | "right";
    /** Click count: 1 (single), 2 (double). Default: 1. */
    clickCount?: number;
  }

  /** Key press options. */
  interface KeyPressOptions {
    /** Modifier bitmask: 1=Alt, 2=Ctrl, 4=Meta, 8=Shift. */
    modifiers?: number;
  }

  /** A single browser tab/page. Cannot be constructed directly. */
  interface Page {
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

    // -----------------------------------------------------------------------
    // Native CDP methods
    // -----------------------------------------------------------------------

    /** Layout metrics for the current page (content size, viewport, etc.). */
    getLayoutMetrics(): Promise<{
      contentSize: { width: number; height: number };
      layoutViewport: { x: number; y: number; width: number; height: number };
      visualViewport: {
        x: number;
        y: number;
        width: number;
        height: number;
        scale: number;
      };
    }>;

    /**
     * Inject a script that runs in every new document (before any other script).
     * @param source - JavaScript source to inject.
     * @returns The script identifier string.
     */
    addInitScript(source: string): Promise<string>;

    /** Get the iframe/frame hierarchy tree. */
    getFrameTree(): Promise<{
      frameTree: {
        frame: { id: string; parentId?: string; url: string; name?: string };
        childFrames?: any[];
      };
    }>;

    /** Enable Runtime domain events (consoleAPICalled, exceptionThrown, etc.). */
    enableRuntime(): Promise<void>;

    /** Unique target ID assigned by Chrome. */
    readonly targetId: string;

    // -----------------------------------------------------------------------
    // Input domain — mouse, keyboard, text
    // -----------------------------------------------------------------------

    /**
     * Click at pixel coordinates (x, y).
     * Performs mousePressed + mouseReleased via native CDP Input.dispatchMouseEvent.
     */
    mouseClick(
      x: number,
      y: number,
      options?: MouseClickOptions,
    ): Promise<void>;

    /**
     * Move mouse to pixel coordinates (x, y).
     * Uses native CDP Input.dispatchMouseEvent (mouseMoved).
     */
    mouseMove(x: number, y: number): Promise<void>;

    /**
     * Simulate pressing a key (keyDown → char → keyUp).
     * Uses native CDP Input.dispatchKeyEvent.
     */
    keyPress(key: string, options?: KeyPressOptions): Promise<void>;

    /**
     * Insert text directly into the focused element.
     * Uses native CDP Input.insertText — reliable for CJK / Unicode.
     */
    insertText(text: string): Promise<void>;

    // -----------------------------------------------------------------------
    // Network domain — request/response body, capture
    // -----------------------------------------------------------------------

    /**
     * Get the HTTP response body for a given request ID.
     * Requires Network domain to be enabled (see startNetworkCapture).
     * @returns `{ body: string, base64Encoded: boolean }`
     */
    getResponseBody(
      requestId: string,
    ): Promise<{ body: string; base64Encoded: boolean }>;

    /**
     * Start capturing network requests.
     * Enables the CDP Network domain and begins recording
     * Network.requestWillBeSent / responseReceived / loadingFinished events.
     * Captured data can be retrieved via getCapturedRequests().
     */
    startNetworkCapture(): Promise<void>;

    /** Stop the network capture background task. */
    stopNetworkCapture(): Promise<void>;

    /**
     * Get all captured network requests/events.
     * Each entry has `{ method, params }` from the raw CDP event.
     */
    getCapturedRequests(): Promise<Array<{ method: string; params: any }>>;

    /** Clear captured network requests. */
    clearCapturedRequests(): Promise<void>;
  }

  /** The Browser class. Launch a Chrome instance via the static `launch` factory. */
  export class Browser {
    /** Launch a Chrome browser instance. */
    static launch(options?: LaunchOptions): Promise<Browser>;
  }
}
