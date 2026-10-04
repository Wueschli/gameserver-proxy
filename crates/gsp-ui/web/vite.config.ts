import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// Built assets are served by gsp-ui itself (tower-http::ServeDir) from the
// same origin as the API, so relative asset paths and no dev-server proxy
// config are needed for production. `server.proxy` below is only for
// `npm run dev` against a real gsp-ui process during frontend development.
export default defineConfig({
  plugins: [react(), tailwindcss()],
  base: "./",
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
  test: {
    environment: "jsdom",
    setupFiles: ["./src/test/setup.ts"],
    css: false,
    exclude: ["e2e/**", "node_modules/**"],
  },
  server: {
    proxy: {
      "/ui": "http://127.0.0.1:9903",
      "/api": "http://127.0.0.1:9903",
      "/ws": { target: "ws://127.0.0.1:9903", ws: true },
      "/healthz": "http://127.0.0.1:9903",
    },
  },
});
