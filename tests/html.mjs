// test_html.mjs — 验证 html 模块的 parseHTML / html2markdown
import { parseHTML, html2markdown } from "html";

let pass = 0;
let fail = 0;
function check(label, condition, detail) {
  if (condition) {
    console.log(`  \u2705 ${label}`);
    pass++;
  } else {
    console.log(`  \u274C ${label}${detail ? " \u2014 " + detail : ""}`);
    fail++;
  }
}

// ===================================================================
// parseHTML — 基础解析
// ===================================================================
console.log("\n=== parseHTML ===");

// 1. 简单文档解析
{
  const doc = parseHTML("<html><body><p>hello</p></body></html>");
  check("parseHTML 返回对象", typeof doc === "object" && doc !== null);
  check("doc 有 querySelectorAll", typeof doc.querySelectorAll === "function");
}

// 2. querySelectorAll 按标签名
{
  const doc = parseHTML("<ul><li>A</li><li>B</li><li>C</li></ul>");
  const items = doc.querySelectorAll("li");
  check("querySelectorAll li 返回数组", Array.isArray(items));
  check(
    "querySelectorAll li 数量 = 3",
    items.length === 3,
    `got ${items.length}`,
  );
}

// 3. querySelectorAll 按类名
{
  const doc = parseHTML('<div class="foo">X</div><div class="bar">Y</div>');
  const foos = doc.querySelectorAll(".foo");
  check(
    "querySelectorAll .foo 数量 = 1",
    foos.length === 1,
    `got ${foos.length}`,
  );
  const bars = doc.querySelectorAll(".bar");
  check(
    "querySelectorAll .bar 数量 = 1",
    bars.length === 1,
    `got ${bars.length}`,
  );
}

// 4. 无匹配
{
  const doc = parseHTML("<p>hello</p>");
  const items = doc.querySelectorAll("span");
  check(
    "querySelectorAll 无匹配返回 []",
    Array.isArray(items) && items.length === 0,
  );
}

// ===================================================================
// Element — tagName / textContent
// ===================================================================
console.log("\n=== Element — tagName / textContent ===");

// 5. tagName
{
  const doc = parseHTML("<div><h1>title</h1><p>text</p></div>");
  const headings = doc.querySelectorAll("h1");
  check(
    "tagName === h1",
    headings[0].tagName === "h1",
    `got ${headings[0].tagName}`,
  );
  const paras = doc.querySelectorAll("p");
  check("tagName === p", paras[0].tagName === "p", `got ${paras[0].tagName}`);
}

// 6. textContent
{
  const doc = parseHTML("<div><p>Hello <b>world</b></p></div>");
  const divs = doc.querySelectorAll("div");
  const text = divs[0].textContent;
  check("textContent 含 Hello", text.includes("Hello"), `got ${text}`);
  check("textContent 含 world", text.includes("world"), `got ${text}`);
}

// 7. textContent 中文
{
  const doc = parseHTML("<h1>\u4F60\u597D\u4E16\u754C</h1><p>\u6D4B\u8BD5</p>");
  const title = doc.querySelectorAll("h1");
  check(
    "中文 textContent",
    title[0].textContent === "\u4F60\u597D\u4E16\u754C",
    `got ${title[0].textContent}`,
  );
}

// ===================================================================
// Element — children
// ===================================================================
console.log("\n=== Element — children ===");

// 8. 嵌套子元素
{
  const doc = parseHTML("<ul><li>A</li><li>B</li></ul>");
  const uls = doc.querySelectorAll("ul");
  const kids = uls[0].children;
  check("children 返回数组", Array.isArray(kids));
  check("children 数量 = 2", kids.length === 2, `got ${kids.length}`);
  check(
    "children[0] tagName = li",
    kids[0].tagName === "li",
    `got ${kids[0].tagName}`,
  );
  check(
    "children[1] tagName = li",
    kids[1].tagName === "li",
    `got ${kids[1].tagName}`,
  );
}

// 9. 无子元素
{
  const doc = parseHTML("<br/>");
  const brs = doc.querySelectorAll("br");
  check("无子元素 children.length = 0", brs[0].children.length === 0);
}

// ===================================================================
// Element — getAttribute
// ===================================================================
console.log("\n=== Element — getAttribute ===");

// 10. 读取属性
{
  const doc = parseHTML('<a href="/test" class="link">click</a>');
  const links = doc.querySelectorAll("a");
  check(
    "href 属性",
    links[0].getAttribute("href") === "/test",
    `got ${links[0].getAttribute("href")}`,
  );
  check(
    "class 属性",
    links[0].getAttribute("class") === "link",
    `got ${links[0].getAttribute("class")}`,
  );
}

// 11. 不存在的属性返回 null
{
  const doc = parseHTML("<span>text</span>");
  const spans = doc.querySelectorAll("span");
  check("不存在的属性返回 null", spans[0].getAttribute("id") === null);
}

// ===================================================================
// Element — querySelector / querySelectorAll
// ===================================================================
console.log("\n=== Element — querySelector / querySelectorAll ===");

// 12. querySelector 在元素内查找
{
  const doc = parseHTML("<div><p>A</p><p>B</p></div>");
  const divs = doc.querySelectorAll("div");
  const first = divs[0].querySelector("p");
  check(
    "querySelector 返回 Element",
    typeof first === "object" && first !== null,
  );
  check(
    "querySelector 找到第一个 p",
    first.tagName === "p",
    `got ${first.tagName}`,
  );
}

// 13. querySelector 无匹配
{
  const doc = parseHTML("<div><span>X</span></div>");
  const divs = doc.querySelectorAll("div");
  const miss = divs[0].querySelector("p");
  check("querySelector 无匹配返回 null", miss === null);
}

// 14. querySelectorAll 在元素内查找
{
  const doc = parseHTML("<div><p>A</p><p>B</p><span>C</span></div>");
  const divs = doc.querySelectorAll("div");
  const paras = divs[0].querySelectorAll("p");
  check(
    "快捷 querySelectorAll 数量 = 2",
    paras.length === 2,
    `got ${paras.length}`,
  );
  check("快捷 querySelectorAll[0] tagName", paras[0].tagName === "p");
  check("快捷 querySelectorAll[1] tagName", paras[1].tagName === "p");
}

// ===================================================================
// html2markdown
// ===================================================================
console.log("\n=== html2markdown ===");

// 15. 标题
{
  const md = html2markdown("<h1>Title</h1><h2>Subtitle</h2>");
  check("h1 转 #", md.includes("# Title"), `got ${md}`);
  check("h2 转 ##", md.includes("## Subtitle"), `got ${md}`);
}

// 16. 链接
{
  const md = html2markdown('<a href="https://example.com">Example</a>');
  check("链接保留 text", md.includes("Example"), `got ${md}`);
  check("链接保留 url", md.includes("example.com"), `got ${md}`);
}

// 17. 粗体 + 斜体
{
  const md = html2markdown("<p><b>bold</b> and <i>italic</i></p>");
  check("粗体", md.includes("bold"), `got ${md}`);
  check("斜体", md.includes("italic"), `got ${md}`);
}

// 18. 列表
{
  const md = html2markdown("<ul><li>A</li><li>B</li><li>C</li></ul>");
  check(
    "列表",
    md.includes("A") && md.includes("B") && md.includes("C"),
    `got ${md}`,
  );
}

// 19. 代码块
{
  const md = html2markdown("<pre><code>let x = 1;</code></pre>");
  check("代码块", md.includes("let x = 1;"), `got ${md}`);
}

// 20. 空字符串
{
  const md = html2markdown("");
  check("空字符串返回 ''", md === "", `got ${md.length} chars`);
}

// ===================================================================
// 综合场景
// ===================================================================
console.log("\n=== 综合场景 ===");

// 21. 提取页面标题
{
  const html =
    "<html><head><title>My Page</title></head><body><h1>Welcome</h1><p>Hello</p></body></html>";
  const doc = parseHTML(html);
  const titles = doc.querySelectorAll("title");
  check(
    "提取页面标题",
    titles.length === 1 && titles[0].textContent === "My Page",
  );
}

// 22. 提取所有链接文本
{
  const html = '<nav><a href="/">Home</a><a href="/about">About</a></nav>';
  const doc = parseHTML(html);
  const links = doc.querySelectorAll("a");
  check("2 个链接", links.length === 2);
  check("链接 1 text", links[0].textContent.trim() === "Home");
  check("链接 2 text", links[1].textContent.trim() === "About");
}

// 23. 带属性选择器
{
  const html = '<input type="text" disabled><input type="checkbox">';
  const doc = parseHTML(html);
  const disabled = doc.querySelectorAll("[disabled]");
  check("属性选择器匹配数量", disabled.length === 1, `got ${disabled.length}`);
}

// 24. html2markdown 嵌套结构
{
  const html =
    "<article><h1>News</h1><p>Today we <strong>launched</strong> v2.0.</p></article>";
  const md = html2markdown(html);
  check("嵌套转 md 含 h1", md.includes("# News"), `got ${md}`);
  check("嵌套转 md 含文本", md.includes("launched"), `got ${md}`);
  check("嵌套转 md 含 v2.0", md.includes("v2.0"), `got ${md}`);
}

// ===================================================================
// 结果
// ===================================================================
console.log(`\n=== \u7ED3\u679C: ${pass} passed, ${fail} failed ===\n`);

if (fail > 0) {
  console.error("Some tests FAILED");
  process.exit(1);
}
