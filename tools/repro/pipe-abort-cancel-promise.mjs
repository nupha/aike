// 最小复现：source.cancel() 返回外部 Promise，pipeTo abort 时返回值未 root
const error1 = new Error('error1');
error1.name = 'error1';
const rs = new ReadableStream({
  pull(controller) { controller.error('failed to abort'); },
  cancel() { return Promise.reject(error1); }
}, { highWaterMark: 0 });
const ws = new WritableStream();
const ac = new AbortController();
ac.abort();
rs.pipeTo(ws, { signal: ac.signal }).catch(() => {});
console.log('done');
