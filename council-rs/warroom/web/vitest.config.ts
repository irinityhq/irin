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
    // Include both .test.ts (pure hooks/helpers under components/) and
    // .test.tsx (component markup). The prior .tsx-only components glob
    // silently skipped components/**/*.test.ts (idle hook unit tests).
    include: [
      "lib/**/*.{test,spec}.{ts,tsx}",
      "hooks/**/*.{test,spec}.{ts,tsx}",
      "components/**/*.{test,spec}.{ts,tsx}",
    ],
    exclude: ["e2e/**", "node_modules/**"],
  },
});
