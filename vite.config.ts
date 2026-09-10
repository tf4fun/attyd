import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { fileURLToPath } from "node:url";
import { devPort } from "./scripts/dev-options";

export default defineConfig(({ command }) => ({
  root: fileURLToPath(new URL("./web", import.meta.url)),
  plugins: [react()],
  server: {
    host: "127.0.0.1",
    port: command === "serve" ? devPort(process.env.ATTYD_DEV_PORT) : undefined,
    strictPort: true,
  },
  build: {
    outDir: "../dist/client",
    emptyOutDir: true,
    rollupOptions: {
      output: {
        manualChunks: {
          "react-vendor": ["react", "react-dom"],
          i18n: ["i18next", "react-i18next", "i18next-browser-languagedetector"],
          markdown: ["react-markdown", "remark-gfm"],
          terminal: ["@xterm/xterm", "@xterm/addon-fit"],
        },
      },
    },
  },
}));
