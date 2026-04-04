#!/usr/bin/env python3
"""HTTP server with COOP/COEP headers for SharedArrayBuffer support."""
import http.server
import sys

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8081

class COEPHandler(http.server.SimpleHTTPRequestHandler):
    def end_headers(self):
        self.send_header("Cross-Origin-Opener-Policy", "same-origin")
        self.send_header("Cross-Origin-Embedder-Policy", "require-corp")
        super().end_headers()

    def guess_type(self, path):
        if path.endswith(".wasm"):
            return "application/wasm"
        return super().guess_type(path)

print(f"Serving on http://localhost:{PORT} with COOP/COEP headers")
print("Open this URL in a browser to run the threaded WASM test.")
http.server.HTTPServer(("", PORT), COEPHandler).serve_forever()
