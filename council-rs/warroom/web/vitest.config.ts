import path from "node:path";
import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";

export default defineConfig({
  resolve: {
    // Mirror tsconfig's "@/*" path alias so node-safe modules under hooks/
    // (which import via "@/lib/...") can be unit-tested.
    alias: { "@": path.dirname(fileURLToPath(import.meta.url)) },
  },
  test: {
    include: [
      "lib/**/*.test.ts",
      "hooks/**/*.test.ts",
      "components/**/*.test.tsx",
    ],
    exclude: ["e2e/**", "node_modules/**"],
  },
});
