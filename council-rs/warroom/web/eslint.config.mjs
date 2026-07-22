import nextConfig from "eslint-config-next";
import tsPlugin from "@typescript-eslint/eslint-plugin";

const config = [
  {
    ignores: ["node_modules/**", ".next/**", ".next-*/**", "out/**"],
  },
  ...nextConfig,
  {
    plugins: {
      "@typescript-eslint": tsPlugin,
    },
    rules: {
      "@typescript-eslint/no-explicit-any": "warn",
      "@typescript-eslint/no-unused-vars": ["warn", { argsIgnorePattern: "^_" }],
      "react-hooks/exhaustive-deps": "warn",
      "react-hooks/set-state-in-effect": "off",
    },
  },
];

export default config;
