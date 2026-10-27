// Test: Page-class Page domain — printToPDF
//
//   Page.printToPDF({ printBackground }) -> Uint8Array (PDF bytes)
//
// Run as a module (.mjs) so `import` is parsed in module mode:
//   cargo run -- tests/browser/page_pdf.mjs

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

const PAGE =
  "data:text/html," +
  encodeURIComponent(
    "<!DOCTYPE html><html><head><title>pdf</title></head><body>" +
      "<h1>Hello PDF</h1><p>some content</p></body></html>",
  );

const PDF_SIG = [0x25, 0x50, 0x44, 0x46]; // %PDF

function isPdf(buf) {
  return (
    buf &&
    buf.length >= 4 &&
    buf[0] === PDF_SIG[0] &&
    buf[1] === PDF_SIG[1] &&
    buf[2] === PDF_SIG[2] &&
    buf[3] === PDF_SIG[3]
  );
}

async function testPrintToPDF(page) {
  console.log("\n=== Page.printToPDF() ===");
  await page.navigate(PAGE);
  await page.waitForSelector("h1", 5000);

  const pdf = await page.printToPDF();
  check("printToPDF returns Uint8Array", pdf instanceof Uint8Array, "type=" + typeof pdf);
  check("printToPDF returns a valid PDF", isPdf(pdf), "sig mismatch, len=" + (pdf && pdf.length));

  const pdf2 = await page.printToPDF({ printBackground: true });
  check(
    "printToPDF({ printBackground: true }) returns a valid PDF",
    isPdf(pdf2),
    "sig mismatch, len=" + (pdf2 && pdf2.length),
  );
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

  const tests = [testPrintToPDF];
  for (const fn of tests) {
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
