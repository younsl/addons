import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { tanstackRouter } from "@tanstack/router-plugin/vite";
import tailwindcss from "@tailwindcss/vite";
import { fileURLToPath, URL } from "node:url";

// The build output is embedded into the forklift binary (src/webui/dist).
export default defineConfig({
  plugins: [
    tanstackRouter({
      target: "react",
      autoCodeSplitting: true,
      generatedRouteTree: "./src/generated/route-tree.gen.ts",
    }),
    tailwindcss(),
    react(),
  ],
  build: {
    outDir: "../src/webui/dist",
    emptyOutDir: true,
  },
  resolve: {
    alias: {
      "@": fileURLToPath(new URL("./src", import.meta.url)),
    },
  },
  server: {
    host: "127.0.0.1",
    // During `pnpm dev`, proxy API and package routes to the forklift server. The
    // target is configurable so the browser tests can point a second dev server
    // at their own throwaway backend instead of the one being developed
    // against - otherwise `make e2e` would write into .data.
    proxy: Object.fromEntries(
      ["/api", "/api-docs", "/auth", "/openapi.yaml", "/maven", "/npm", "/cargo", "/go", "/pypi", "/raw", "/v2"].map(
        (path) => [path, {
          target: process.env.FORKLIFT_API_TARGET ?? "http://localhost:8080",
          // Keep the browser's Host so upload Origin/CSRF validation sees the
          // same public origin on both sides of the development proxy.
          changeOrigin: false,
        }],
      ),
    ),
  },
});
