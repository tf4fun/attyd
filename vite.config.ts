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
          markdown: ["react-markdown", "remark-gfm"],
          terminal: ["@xterm/xterm", "@xterm/addon-fit"],
        },
      },
    },
  },
  server: {
    proxy: {
      "/ws": {
        target: "ws://127.0.0.1:7331",
        ws: true,
      },
    },
  },
});
