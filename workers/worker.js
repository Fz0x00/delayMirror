import initWasm from "./pkg/delay_mirror.js";
import wasmUrl from "./pkg/delay_mirror_bg.wasm";

let wasmInstance = null;

async function ensureInitialized() {
  if (!wasmInstance) {
    const wasmResponse = await fetch(wasmUrl);
    wasmInstance = await initWasm(await wasmResponse.arrayBuffer());
  }
  return wasmInstance;
}

export default {
  async fetch(request, env, ctx) {
    try {
      const wasm = await ensureInitialized();
      return wasm.fetch(request, env, ctx);
    } catch (e) {
      return new Response(JSON.stringify({
        error: "Worker initialization failed",
        message: e.message || String(e),
        stack: e.stack,
      }), {
        status: 500,
        headers: { "Content-Type": "application/json" },
      });
    }
  },
};
