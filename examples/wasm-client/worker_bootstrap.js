// Worker bootstrap — buffers messages while WASM loads, then replays them.
importScripts('./pkg/zenoh_wasm_client_example.js');

// Buffer messages that arrive before WASM is ready
const pendingMessages = [];
self.onmessage = function(e) {
    pendingMessages.push(e.data);
};

async function run() {
    await wasm_bindgen('./pkg/zenoh_wasm_client_example_bg.wasm');

    // start_worker sets up the real onmessage handler via Rust
    // We need to start it, then replay buffered messages
    const workerStarted = wasm_bindgen.start_worker();

    // Give start_worker a microtask to set up its handler
    await new Promise(r => setTimeout(r, 0));

    // Replay any messages that arrived during init
    for (const msg of pendingMessages) {
        self.dispatchEvent(new MessageEvent('message', { data: msg }));
    }
    pendingMessages.length = 0;

    await workerStarted;
}

run().catch(e => {
    console.error("Worker failed to start:", e);
    self.postMessage(JSON.stringify({ type: "error", message: "Worker failed: " + e }));
});
