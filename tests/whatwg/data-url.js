// data: URL processing per the WHATWG fetch spec (MIME type, forgiving base64,
// percent-decoding order, fragment exclusion). Expectations come from WPT
// fetch/data-urls/processing.any.js and base64.any.js.
//
// Run: aike -m tests/whatwg/data-url.js

const mimeCases = [
  ["data:,X", "text/plain;charset=US-ASCII"],
  ["data:text/plain;Charset=UTF-8,%C2%B1", "text/plain;charset=UTF-8"],
  ["data:IMAGE/gif,%C2%B1", "image/gif"],
  ["data:IMAGE/gif;CHARSET=x,%C2%B1", "image/gif;charset=x"],
  ["data:;x=x;charset=x,X", "text/plain;x=x;charset=x"],
  ["data:;charset=\"x\",X", "text/plain;charset=x"],
  ["data:;charset= x,X", "text/plain;charset=\" x\""],
  ["data:;charset=,X", "text/plain"],
  ["data:text/plain;,X", "text/plain"],
  ["data: ,%FF", "text/plain;charset=US-ASCII"],
  ["data:%20,%FF", "text/plain;charset=US-ASCII"],
  ["data:text / html,X", "text/plain;charset=US-ASCII"],
  ["data://test/,X", "text/plain;charset=US-ASCII"],
  ["data:†/†,X", "%e2%80%a0/%e2%80%a0"],
  ["data:x/x;base64;base64x,WA", "x/x"],
  ["data:;base64;,WA", "text/plain"],
  ["data:%3Bbase64,WA", "text/plain;charset=US-ASCII"],
];

const bodyCases = [
  // [url, expected bytes]
  ["data:;base64,W%20A", [88]],
  ["data:;base64,W%0CA", [88]],
  ["data:; base64,WA", [88]],
  ["data:;base64 ,WA", [88]],
  ["data:;BASe64,WA", [88]],
  ["data:;base64,YQ", [97]],
  ["data:;base64,YQ==", [97]],
  ["data:;base64,ab\t\n\f\r cd", [105, 183, 29]],
  ["data:;base64,/A", [252]],
  ["data:;base64,A/", [3]],
];

let failures = 0;
function check(label, got, want) {
  if (got !== want) {
    failures++;
    console.error(`FAIL ${label}: got ${JSON.stringify(got)} want ${JSON.stringify(want)}`);
  }
}

for (const [url, want] of mimeCases) {
  const r = await fetch(url);
  check(url, r.headers.get("content-type"), want);
}

for (const [url, want] of bodyCases) {
  const r = await fetch(url);
  check(url, [...new Uint8Array(await r.arrayBuffer())].join(","), want.join(","));
}

// A missing comma and invalid base64 are network errors.
for (const url of ["data:text/plain", "data:;base64,a"]) {
  let rejected = false;
  try {
    await fetch(url);
  } catch {
    rejected = true;
  }
  check(`${url} rejects`, rejected, true);
}

if (failures > 0) {
  throw new Error(`${failures} data: URL check(s) failed`);
}
console.log("PASS data-url");
