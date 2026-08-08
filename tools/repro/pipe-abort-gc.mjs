// 最小复现：zuqe debug_gc 特性下，pipeTo(abort) 的 async 驱动链返回值未 root → GC 回收 → bad node
//
// 触发条件：
//   - aike debug 构建（debug_assertions 生效）
//   - zuqe 启用 debug_gc feature（每步 bytecode 都 gc_collect，暴露隐藏 GC bug）
//   - 运行本脚本
//
// 触发原理：pipeTo 以 async（call_async）驱动；abort 路径在 async resume 时访问
// 属性，属性值（返回值）未登记为 GC root，被 debug_gc 的频繁收集回收 → use-after-free。
//
// 验证命令（debug_gc 版 aike，稳定 3/3 触发）：
//   cd aike && ./target/debug/aike tools/repro/pipe-abort-gc.mjs
//
// 普通 debug 版（无 debug_gc）不触发（0/3），作对照。

const rs = new ReadableStream();
const ws = new WritableStream();
const ac = new AbortController();
ac.abort();
rs.pipeTo(ws, { signal: ac.signal });
console.log('done');
