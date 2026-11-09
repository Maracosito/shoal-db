import path from "node:path";
import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";

const here = path.dirname(fileURLToPath(import.meta.url));

export default defineConfig({
  resolve: {
    alias: {
      // Test the SDK source directly; no separate build step required.
      "@sdk": path.resolve(here, "../../sdks/typescript/src/index.ts"),
    },
  },
  test: {
    include: ["tests/**/*.test.ts"],
    testTimeout: 60_000,
    hookTimeout: 180_000,
    // Run files sequentially so a small dev deployment is not hammered.
    fileParallelism: false,
  },
});
