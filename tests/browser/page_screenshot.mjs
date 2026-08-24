// Test: extended captureScreenshot() options on Page.
//
//   - default PNG capture
//   - clip {x,y,width,height,scale?} -> exact output dimensions (PNG IHDR)
//   - fullPage (-> CDP captureBeyondViewport) -> taller than the viewport capture
//   - captureBeyondViewport / fromSurface / optimizeForSpeed flags forwarded
//
// Run as a module (.mjs) so `import` is parsed in module mode:
//   cargo run -- tests/browser/page_screenshot.mjs

import { Browser } from "browser";

let pass = 0;
let fail = 0;

function check(label, condition, detail) {
  if (condition) {
    console.log("  [ok] " + label);
    pass++;
  } else {
    console.log("  [FAIL] " + label + (detail ? " -- " + detail : ""));
    fail++;
  }
}

// PNG: 8-byte signature, then IHDR length(4)+"IHDR"(4)+width(4 BE)+height(4 BE).
const PNG_SIG = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

function pngSize(buf) {
  if (!buf || buf.length < 24) return null;
  for (let i = 0; i < 8; i++) if (buf[i] !== PNG_SIG[i]) return null;
  const w =
    ((buf[16] << 24) | (buf[17] << 16) | (buf[18] << 8) | buf[19]) >>> 0;
  const h =
    ((buf[20] << 24) | (buf[21] << 16) | (buf[22] << 8) | buf[23]) >>> 0;
  return { w, h };
}

function isJpeg(buf) {
  return (
    buf &&
    buf.length >= 3 &&
    buf[0] === 0xff &&
    buf[1] === 0xd8 &&
    buf[2] === 0xff
  );
}

const SIMPLE =
  "data:text/html," +
  encodeURIComponent(
    "<!DOCTYPE html><html><body style='margin:0'>" +
      "<div style='width:200px;height:200px;background:#f80'>x</div></body></html>",
  );

const TALL =
  "data:text/html," +
  encodeURIComponent(
    "<!DOCTYPE html><html><head><style>html,body{margin:0;padding:0}</style></head>" +
      "<body><div style='width:120px;height:3000px;background:#08f'></div>" +
      "<h1 id=t>hi</h1></body></html>",
  );

async function testDefault(page) {
  console.log("\n=== captureScreenshot() default (PNG) ===");
  await page.navigate(SIMPLE);
  await page.waitForSelector("div", 5000);
  const png = await page.captureScreenshot();
  check(
    "default returns Uint8Array",
    png instanceof Uint8Array,
    "type=" + typeof png,
  );
  const s = pngSize(png);
  check(
    "default is a valid PNG",
    s !== null,
    "sig mismatch, len=" + (png && png.length),
  );
  check(
    "default PNG has reasonable dimensions",
    s && s.w > 0 && s.h > 0,
    s ? s.w + "x" + s.h : "n/a",
  );
}

async function testClip(page) {
  console.log("\n=== captureScreenshot({ clip }) ===");
  await page.navigate(SIMPLE);
  await page.waitForSelector("div", 5000);
  const png = await page.captureScreenshot({
    clip: { x: 0, y: 0, width: 50, height: 30, scale: 1 },
  });
  const s = pngSize(png);
  check(
    "clip yields a valid PNG",
    s !== null,
    "sig mismatch, len=" + (png && png.length),
  );
  check("clip width == 50", s && s.w === 50, s ? "w=" + s.w : "n/a");
  check("clip height == 30", s && s.h === 30, s ? "h=" + s.h : "n/a");
}

async function testClipScale(page) {
  console.log("\n=== captureScreenshot({ clip, scale: 2 }) ===");
  await page.navigate(SIMPLE);
  await page.waitForSelector("div", 5000);
  const png = await page.captureScreenshot({
    clip: { x: 0, y: 0, width: 50, height: 30, scale: 2 },
  });
  const s = pngSize(png);
  check("clip+scale yields a valid PNG", s !== null, "sig mismatch");
  check(
    "clip scale:2 -> width == 100",
    s && s.w === 100,
    s ? "w=" + s.w : "n/a",
  );
  check(
    "clip scale:2 -> height == 60",
    s && s.h === 60,
    s ? "h=" + s.h : "n/a",
  );
}

async function testJpegClip(page) {
  console.log("\n=== captureScreenshot({ format: jpeg, clip }) ===");
  await page.navigate(SIMPLE);
  await page.waitForSelector("div", 5000);
  const jpeg = await page.captureScreenshot({
    format: "jpeg",
    quality: 60,
    clip: { x: 0, y: 0, width: 40, height: 20, scale: 1 },
  });
  check(
    "jpeg+clip is Uint8Array",
    jpeg instanceof Uint8Array,
    "type=" + typeof jpeg,
  );
  check(
    "jpeg+clip is a valid JPEG",
    isJpeg(jpeg),
    "bad sig, len=" + (jpeg && jpeg.length),
  );
  check(
    "jpeg+clip has data",
    jpeg && jpeg.length > 50,
    "len=" + (jpeg && jpeg.length),
  );
}

async function testFullPage(page) {
  console.log("\n=== captureScreenshot({ fullPage: true }) ===");
  await page.navigate(TALL);
  await page.waitForSelector("#t", 5000);

  const normal = pngSize(await page.captureScreenshot());
  const full = pngSize(await page.captureScreenshot({ fullPage: true }));

  check("normal capture is a valid PNG", normal !== null, "sig mismatch");
  check("fullPage capture is a valid PNG", full !== null, "sig mismatch");

  check(
    "fullPage is taller than the viewport capture",
    normal && full && full.h > normal.h,
    "normal=" + (normal ? normal.h : "?") + ", full=" + (full ? full.h : "?"),
  );
  check(
    "fullPage captured the full 3000px content",
    full && full.h >= 2500,
    full ? "h=" + full.h : "n/a",
  );
}

async function testFlags(page) {
  console.log("\n=== captureScreenshot flags (CDP forwarding) ===");
  await page.navigate(SIMPLE);
  await page.waitForSelector("div", 5000);

  // Each flag must be accepted by CDP and still return a valid image.
  const cbv = pngSize(
    await page.captureScreenshot({ captureBeyondViewport: true }),
  );
  check(
    "captureBeyondViewport:true -> valid PNG",
    cbv !== null,
    "sig mismatch",
  );

  const fs = pngSize(await page.captureScreenshot({ fromSurface: true }));
  check("fromSurface:true -> valid PNG", fs !== null, "sig mismatch");

  const ofs = pngSize(await page.captureScreenshot({ optimizeForSpeed: true }));
  check("optimizeForSpeed:true -> valid PNG", ofs !== null, "sig mismatch");

  const combo = pngSize(
    await page.captureScreenshot({
      fromSurface: true,
      optimizeForSpeed: true,
      captureBeyondViewport: true,
    }),
  );
  check("all flags combined -> valid PNG", combo !== null, "sig mismatch");
}

async function main() {
  let browser;
  try {
    browser = await Browser.launch({ headless: true });
  } catch (e) {
    console.log("\n  Failed to launch browser: " + (e && (e.message || e)));
    console.log("  Make sure Google Chrome or Chromium is installed.");
    process.exit(1);
    return;
  }

  check("Browser launched", browser.isRunning === true);
  const page = await browser.newPage();

  const fns = [
    testDefault,
    testClip,
    testClipScale,
    testJpegClip,
    testFullPage,
    testFlags,
  ];
  for (const fn of fns) {
    try {
      await fn(page);
    } catch (e) {
      console.log("  [FAIL] " + fn.name + " threw: " + (e && (e.message || e)));
      fail++;
    }
  }

  try {
    await page.close();
  } catch (e) {}
  try {
    await browser.close();
  } catch (e) {}

  console.log("\n" + "=".repeat(50));
  console.log("pass: " + pass + ", fail: " + fail);
  if (fail > 0) process.exit(1);
  else process.exit(0);
}

main();
