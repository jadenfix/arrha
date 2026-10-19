import { defineConfig, loadEnv } from "vite";

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), "");
  return {
    define: {
      "globalThis.__AETHER_RPC_ENDPOINT__": JSON.stringify(
        env.VITE_AETHER_RPC_ENDPOINT ?? "http://127.0.0.1:8545",
      ),
    },
    server: { host: "127.0.0.1", port: 3001, strictPort: true },
  };
});
