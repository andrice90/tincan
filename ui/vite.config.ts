import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Cross-origin isolation, matching app.security.headers in tauri.conf.json.
// Desktop dev loads the dev server directly, so the headers have to be set here too.
const isolation = {
  "Cross-Origin-Opener-Policy": "same-origin",
  "Cross-Origin-Embedder-Policy": "require-corp",
};

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 5173, strictPort: true, headers: isolation },
  preview: { port: 5173, strictPort: true, headers: isolation },
  envPrefix: ["VITE_", "TAURI_ENV_"],
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "chrome120",
    sourcemap: false,
  },
});
