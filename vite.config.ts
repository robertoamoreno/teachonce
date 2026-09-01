import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri drives this dev server, so the port is fixed and failures must be loud:
// silently moving to 1421 would leave the app pointed at nothing.
export default defineConfig({
  plugins: [react()],
  root: "ui",
  build: { outDir: "dist", emptyOutDir: true, target: "safari15" },
  clearScreen: false,
  server: { port: 1420, strictPort: true },
});
