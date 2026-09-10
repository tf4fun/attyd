import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  root: "web",
  plugins: [react()],
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
});
