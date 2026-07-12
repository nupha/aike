// Type declarations for the "browser" module
// import { Browser } from "browser";
//
// Keep this file in sync with the JS API registered in aike/src/browser/*.rs
// (see BROWSER_PROTO in class_browser.rs and PAGE_PROTO in class_page.rs).

declare module "browser" {
  // =========================================================================
  // Launch options
  // =========================================================================

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

  // =========================================================================
  // Browser
  // =========================================================================

  /** A Chrome browser instance. Cannot be constructed directly. */
  interface Browser {
    // ---- Target domain ----
    /**
     * Create a new tab (page).
     * @param urlOrOpts - URL string, or `{ url?, contextId? }`.
     */
    newPage(urlOrOpts?: string | { url?: string; contextId?: string }): Promise<Page>;

    /** Close the browser and release all resources. */
    close(): Promise<void>;

    /** Whether the browser process is still running. */
    readonly isRunning: boolean;

    // ---- Browser domain ----
    /** Browser/platform version info. */
    getVersion(): Promise<{
      protocolVersion: string;
      product: string;
      revision: string;
      userAgent: string;
      jsVersion: string;
    }>;

    /** Current browser window bounds. */
    getWindowBounds(): Promise<any>;

    /** Set the browser window bounds `{ left?, top?, width?, height?, windowState? }`. */
    setWindowBounds(bounds: {
      left?: number;
      top?: number;
      width?: number;
      height?: number;
      windowState?: string;
    }): Promise<void>;

    /** Set global download behavior. Empty path → "deny", otherwise "allow". */
    setDownloadBehavior(path: string): Promise<void>;

    // ---- Target domain ----
    /** List page-type targets `[{ targetId, url, title, type }, ...]`. */
    listPages(): Promise<Array<{ targetId: string; url: string; title: string; type: string }>>;

    /** List all targets `[{ targetId, url, title, type, ... }, ...]`. */
    getTargets(): Promise<Array<{ targetId: string; url: string; title: string; type: string }>>;

    /** Create a browser context. Returns the new context id (string). */
    newContext(): Promise<string>;

    /** Dispose a browser context created via `newContext()`. */
    disposeContext(contextId: string): Promise<void>;

    // ---- Security domain ----
    /** Ignore HTTPS certificate errors. */
    setIgnoreHTTPSErrors(enabled: boolean): Promise<void>;

    /** Handle a pending certificate error. */
    handleCertificateError(payload: any): Promise<void>;
  }

  // =========================================================================
  // Page
  // =========================================================================

  /** A single browser tab/page. Cannot be constructed directly. */
  interface Page {
    /** Unique target ID assigned by Chrome. */
    readonly targetId: string;

    // ---- Navigation ----
    /** Navigate to a URL. Resolves when the page starts loading. */
    navigate(url: string): Promise<void>;

    /** Reload the page. */
    reload(options?: { ignoreCache?: boolean; scriptToEvaluateOnLoad?: string }): Promise<void>;

    /** Stop the current page load. */
    stopLoading(): Promise<void>;

    /** Navigate back in history. */
    back(): Promise<void>;

    /** Navigate forward in history. */
    forward(): Promise<void>;

    /** Get the navigation history. */
    getNavigationHistory(): Promise<any>;

    /** Bring the tab to the front (focus). */
    bringToFront(): Promise<void>;

    /**
     * Wait for a CSS selector to appear on the page.
     * @param selector  - CSS selector string.
     * @param timeoutMs - Timeout in milliseconds. Default: 30000.
     */
    waitForSelector(selector: string, timeoutMs?: number): Promise<void>;

    /**
     * Wait for the page to finish navigating / fully load.
     * @param timeoutMs - Timeout in milliseconds. Default: 30000.
     */
    waitForNavigation(timeoutMs?: number): Promise<void>;

    // ---- Content / evaluate ----
    /**
     * Execute JavaScript code in the page context.
     * @param jsCode - JavaScript source to evaluate.
     * @returns The JSON-serialized result.
     */
    evaluate(jsCode: string): Promise<any>;

    /** Get the full outerHTML of the document. */
    getOuterHTML(): Promise<string>;

    /** Set the page content (HTML) directly. */
    setContent(html: string): Promise<void>;

    /** Capture an accessibility / MHTML-style snapshot of the page. */
    captureSnapshot(options?: { format?: string; markUnchanged?: boolean }): Promise<string>;

    /** Layout metrics (content size, viewports, etc.). */
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

    /** Inject a script that runs in every new document (before other scripts). */
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

    // ---- Runtime: remote objects ----
    /** Evaluate an expression and return a handle `{ objectId }` referencing the result. */
    evaluateHandle(expression: string): Promise<{ objectId: string }>;

    /**
     * Enumerate an object's own properties by its CDP `objectId`.
     * @returns A plain object mapping each property name to its JSON value.
     */
    getProperties(objectId: string): Promise<Record<string, any>>;

    /**
     * Call a function on a remote object.
     * @param objectId - CDP objectId returned by `evaluateHandle`.
     * @param functionDeclaration - Full function source, e.g. `"function (x) { return this.a + x; }"`.
     * @param args - Optional call arguments (JSON-serializable).
     * @returns The JSON-serialized return value.
     */
    callFunctionOn(objectId: string, functionDeclaration: string, args?: any[]): Promise<any>;

    /**
     * Register a callback invoked for every `Runtime.consoleAPICalled` event
     * (e.g. `console.log` in the page). The callback receives the raw event
     * params object (`{ type, args, ... }`).
     */
    onConsole(callback: (msg: any) => void): void;

    // ---- Screenshot / PDF ----
    /** Capture a screenshot of the current page. */
    captureScreenshot(options?: ScreenshotOptions): Promise<Uint8Array>;

    /** Export the current page as PDF (Chromium-specific). */
    printToPDF(options?: PdfOptions): Promise<Uint8Array>;

    // ---- Viewport / emulation ----
    /** Set the browser window / device viewport size. */
    setViewport(width: number, height: number): Promise<void>;

    /** Override geolocation. */
    setGeolocation(latitude: number, longitude: number, accuracy?: number): Promise<void>;

    /** Override supported media features (e.g. `[{ name: "prefers-color-scheme", value: "dark" }]`). */
    setMediaFeatures(features: Array<{ name: string; value: string }>): Promise<void>;

    /** Override the timezone (IANA name, e.g. "Asia/Shanghai"). */
    setTimezone(timezoneId: string): Promise<void>;

    /** Override the locale (e.g. "en-US"). */
    setLocale(locale: string): Promise<void>;

    /** Enable / disable touch emulation. */
    setTouchEnabled(enabled: boolean): Promise<void>;

    /** Throttle CPU (rate multiplier, e.g. 4 = 4x slower). */
    setCPUThrottling(rate: number): Promise<void>;

    // ---- Dialogs ----
    /**
     * Accept or dismiss a JavaScript dialog (alert/confirm/prompt).
     * @param accept    - true=accept/OK, false=dismiss/cancel.
     * @param promptText - Text to enter for prompt dialogs.
     */
    handleDialog(accept: boolean, promptText?: string): Promise<void>;

    // ---- Download ----
    /** Set per-page download behavior. Empty path → "deny", otherwise "allow". */
    setDownloadBehavior(path: string): Promise<void>;

    /** Set whether to bypass Content-Security-Policy. */
    setBypassCSP(enabled: boolean): Promise<void>;

    // ---- Files ----
    /**
     * Upload files to an <input type="file"> element.
     * @param selector  - CSS selector for the file input element.
     * @param filePaths - Absolute paths to the files to upload.
     */
    uploadFile(selector: string, filePaths: string[]): Promise<void>;

    /** Close this tab. */
    close(): Promise<void>;

    // ---- DOM ----
    /** Resolve a selector to a DOM nodeId (number), or 0 if not found. */
    querySelector(selector: string): Promise<number>;

    /** Resolve a selector to an array of DOM nodeIds (numbers). */
    querySelectorAll(selector: string): Promise<number[]>;

    /** Get an attribute value of the first element matching the selector. */
    getAttribute(selector: string, name: string): Promise<string | null>;

    /** Focus the first element matching the selector. */
    focus(selector: string): Promise<void>;

    /** Get the box model (quads) of the first element matching the selector. */
    getBoxModel(selector: string): Promise<any>;

    /** Remove the first element matching the selector. */
    remove(selector: string): Promise<void>;

    /** Get the outerHTML of the first element matching the selector. */
    getNodeValue(selector: string): Promise<string>;

    /** Click the first element matching the selector. */
    click(selector: string): Promise<void>;

    /** Type text into the first element matching the selector. */
    type(selector: string, text: string): Promise<void>;

    // ---- Input ----
    /**
     * Click at pixel coordinates (x, y).
     * Performs mousePressed + mouseReleased via native CDP Input.dispatchMouseEvent.
     */
    mouseClick(x: number, y: number, options?: MouseClickOptions): Promise<void>;

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

    // ---- Request interception (Fetch domain) ----
    /** Enable / disable request interception. */
    setRequestInterception(enabled: boolean): Promise<void>;

    /** Continue a paused request by its interceptionId. */
    continueRequest(interceptionId: string): Promise<void>;

    /** Abort a paused request by its interceptionId. */
    abortRequest(interceptionId: string): Promise<void>;

    /** Fulfill a paused request by its interceptionId with a response. */
    respondRequest(response: {
      interceptionId: string;
      status?: number;
      headers?: Record<string, string>;
      body?: string;
      base64Encoded?: boolean;
    }): Promise<void>;

    /** Register a callback invoked when a request is paused (interception). */
    onRequestPaused(callback: (event: any) => void): Promise<void>;

    // ---- Network ----
    /** Get all cookies for the current page. */
    getCookies(): Promise<Cookie[]>;

    /** Set a single cookie for the current page. */
    setCookie(cookie: Cookie): Promise<void>;

    /** Set multiple cookies for the current page. */
    setCookies(cookies: Cookie[]): Promise<void>;

    /** Delete a cookie by URL + name. */
    deleteCookie(url: string, name: string): Promise<void>;

    /** Clear all browser cookies. */
    clearBrowserCookies(): Promise<void>;

    /** Override the User-Agent string. */
    setUserAgent(userAgent: string): Promise<void>;

    /** Set extra HTTP headers sent with every request. */
    setExtraHTTPHeaders(headers: Record<string, string>): Promise<void>;

    /** Enable / disable offline mode. */
    setOfflineMode(enabled: boolean): Promise<void>;

    /** Enable / disable the HTTP cache. */
    setCacheDisabled(disabled: boolean): Promise<void>;

    /**
     * Get the HTTP response body for a given request ID.
     * Requires Network domain to be enabled (see startNetworkCapture).
     * @returns `{ body: string, base64Encoded: boolean }`
     */
    getResponseBody(requestId: string): Promise<{ body: string; base64Encoded: boolean }>;

    /**
     * Get the POST body of a request by its (Network-domain) request ID.
     * Requires the request to be intercepted (Fetch domain) so the body is
     * buffered; pass `event.networkId` from `onRequestPaused`.
     */
    getRequestPostData(requestId: string): Promise<string>;

    /**
     * Start capturing network requests.
     * Enables the CDP Network domain and begins recording
     * Network.requestWillBeSent / responseReceived / loadingFinished events.
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

    // ---- Console / Log ----
    /** Register a callback for console / log messages. */
    onLog(callback: (message: ConsoleMessage) => void): Promise<void>;

    /** Return buffered console/log messages. Pass `clear=true` to flush the buffer. */
    getConsoleMessages(clear?: boolean): Promise<ConsoleMessage[]>;

    // ---- Storage ----
    /** Clear cache/storage for an origin. `storageTypes` defaults to `"all"`. */
    clearData(origin: string, storageTypes?: string): Promise<void>;

    /** Resolve a frame's storage key (string) by its CDP frameId. */
    getStorageKeyForFrame(frameId: string): Promise<string>;

    // ---- Overlay (debug highlight) ----
    /** Highlight the first element matching the selector. */
    highlight(selector: string, options?: any): Promise<void>;

    /** Remove any active overlay highlight. */
    hideHighlight(): Promise<void>;

    // ---- CSS ----
    /** Get the computed style of the first element matching the selector. */
    getComputedStyle(selector: string): Promise<Record<string, string>>;

    /** Get the matched CSS rules of the first element matching the selector. */
    getMatchedStyles(selector: string): Promise<any>;

    // ---- IO ----
    /** Read a CDP IO stream handle to completion and return its text. */
    readStream(handle: string): Promise<string>;

    // ---- Tracing ----
    /** Start a Chrome trace. `categories` is a comma-separated CDP filter. */
    startTracing(categories?: string): Promise<void>;

    /** Stop tracing and return the captured trace as a `{ traceEvents: [...] }` JSON string. */
    stopTracing(): Promise<string>;
  }

  // =========================================================================
  // Supporting types
  // =========================================================================

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

  /** A buffered console / log message. */
  interface ConsoleMessage {
    /** Source: "console-api", "log", "network", etc. */
    source?: string;
    /** Log level: "log", "warning", "error", "debug", "info". */
    level?: string;
    /** Message text. */
    text?: string;
    /** Raw CDP parameters. */
    [key: string]: any;
  }

  // =========================================================================
  // Module export
  // =========================================================================

  /** The Browser class. Launch a Chrome instance via the static `launch` factory. */
  export class Browser {
    /**
     * Launch a Chrome browser instance.
     * @returns A `Browser` instance (NOT a Page — call `browser.newPage()` next).
     */
    static launch(options?: LaunchOptions): Promise<Browser>;
  }
}
