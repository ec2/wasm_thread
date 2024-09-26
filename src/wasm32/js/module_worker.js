let _worker;
export function startWorker() {
  const worker = new Worker(
    new URL('./web_worker_module.js', import.meta.url),
    {
      type: 'module',
      name: 'wasm_thread'
    }
  );
  _worker = worker;
  return worker;
}