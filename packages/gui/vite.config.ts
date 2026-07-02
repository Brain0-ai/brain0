import { defineConfig } from "vite";

// In dev, the GUI is served by Vite (default :5173) but the graph data + agent come from the
// brain0 server (default :8787). Proxy the data paths to it so the origin-relative fetches in
// HttpDataSource (`/graph.json`, `/api/debug`, `/health`) reach the backend. Override the
// target with BRAIN0_SERVER if the server runs elsewhere.
const backend = process.env.BRAIN0_SERVER ?? "http://localhost:8787";

export default defineConfig({
  build: {
    outDir: "build",
    sourcemap: true,
  },
  server: {
    proxy: {
      "/graph.json": backend,
      "/api": backend,
      "/health": backend,
    },
  },
});
