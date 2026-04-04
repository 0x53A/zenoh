// Worker bootstrap — loads the WASM module and starts the zenoh worker.
import init, { start_worker } from './pkg/zenoh_wasm_client_example.js';

async function run() {
    // Initialize the WASM module in the worker context
    await init();
    // Start the zenoh worker event loop
    await start_worker();
}

run().catch(e => {
    console.error("Worker failed to start:", e);
    self.postMessage(JSON.stringify({ type: "error", message: "Worker failed: " + e }));
});
