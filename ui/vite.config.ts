import { defineConfig } from "vitest/config";

export default defineConfig({
  base: "./",
  build: {
    lib: {
      entry: "src/index.ts",
      formats: ["es"],
      fileName: () => "index.js",
    },
  },
});
