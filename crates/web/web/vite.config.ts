import { defineConfig } from "vite";
import preact from "@preact/preset-vite";

export default defineConfig({
  plugins: [preact()],
  server: {
    proxy: {
      // Dev: `lob serve` runs alongside on :8080; Vite serves the SPA on :5173
      // and proxies API calls through so same-origin fetches Just Work.
      "/api": "http://127.0.0.1:8080",
    },
  },
  build: {
    target: "es2022",
    sourcemap: false,
  },
});
