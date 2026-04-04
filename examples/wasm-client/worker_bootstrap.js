// Worker bootstrap — loads the WASM module using importScripts (works in all browsers).
importScripts('./pkg/zenoh_wasm_client_example.js');

async function run() {
    // wasm_bindgen is the global init function in no-modules mode
    await wasm_bindgen('./pkg/zenoh_wasm_client_example_bg.wasm');
    // Start the zenoh worker event loop
    await wasm_bindgen.start_worker();
}

run().catch(e => {
    console.error("Worker failed to start:", e);
    self.postMessage(JSON.stringify({ type: "error", message: "Worker failed: " + e }));
});
