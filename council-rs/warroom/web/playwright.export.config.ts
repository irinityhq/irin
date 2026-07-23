import path from "node:path";
import { existsSync } from "node:fs";
import { defineConfig } from "@playwright/test";

const RUNTIME_CONFIG_STORAGE_KEY = "warroom.runtime-config.v1";

function resolvePort(name: string, fallback: number): number {
  const raw = process.env[name]?.trim();
  if (!raw) return fallback;
  const port = Number(raw);
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    throw new Error(`${name} must be a TCP port, got ${JSON.stringify(raw)}`);
  }
  return port;
}

function resolveCouncilBinary(councilRoot: string): string {
  const configured = process.env.PW_COUNCIL_BIN?.trim();
  const candidates = [
    configured,
    path.join(councilRoot, "target/release/council"),
    path.join(councilRoot, "../target/release/council"),
  ].filter((candidate): candidate is string => Boolean(candidate));
  const resolved = candidates.find((candidate) => existsSync(candidate));
  if (!resolved) {
    throw new Error(`Council release binary not found; checked: ${candidates.join(", ")}`);
  }
  return resolved;
}

const councilRoot = path.resolve(__dirname, "../..");
const exportRoot = path.resolve(__dirname, "../../warroom-tauri/warroom-web-dist");
const councilPort = resolvePort("PW_EXPORT_COUNCIL_PORT", 8766);
const webPort = resolvePort("PW_EXPORT_WEB_PORT", 3011);
// The E2E fixtures consume the hosted-suite names. Export aliases remain the
// public knobs, but normalize them before Playwright forks test workers so
// route mocks, direct health checks, and runtime storage all target one stack.
process.env.PW_COUNCIL_PORT = String(councilPort);
process.env.PW_WEB_PORT = String(webPort);
const webBase = `http://127.0.0.1:${webPort}`;
const apiBase = `http://127.0.0.1:${councilPort}`;
const wsBase = `ws://127.0.0.1:${councilPort}`;
const councilBin = resolveCouncilBinary(councilRoot);

const providerKeys = [
  "ANTHROPIC_API_KEY",
  "DEEPSEEK_API_KEY",
  "GEMINI_API_KEY",
  "GOOGLE_API_KEY",
  "GROQ_API_KEY",
  "MISTRAL_API_KEY",
  "NVIDIA_API_KEY",
  "NOUS_API_KEY",
  "OPENAI_API_KEY",
  "OPENROUTER_API_KEY",
  "TOGETHER_API_KEY",
  "XAI_API_KEY",
];
const unsetProviders = providerKeys.map((key) => `-u ${key}`).join(" ");

export default defineConfig({
  testDir: "./e2e",
  timeout: 30_000,
  retries: 0,
  // The no-provider Council smoke backend is deliberately small. Serializing
  // the export suite makes this a stable product gate instead of a load test.
  workers: 1,
  use: {
    baseURL: webBase,
    headless: true,
    storageState: {
      cookies: [],
      origins: [
        {
          origin: webBase,
          localStorage: [
            {
              name: RUNTIME_CONFIG_STORAGE_KEY,
              value: JSON.stringify({ apiBase, wsBase }),
            },
          ],
        },
      ],
    },
  },
  projects: [{ name: "embedded-export", use: { browserName: "chromium" } }],
  webServer: [
    {
      command: `env ${unsetProviders} "${councilBin}" --serve --port ${councilPort}`,
      cwd: councilRoot,
      port: councilPort,
      reuseExistingServer: false,
      timeout: 15_000,
      env: {
        COUNCIL_DEV_NO_AUTH: "1",
        COUNCIL_WS_SMOKE_ONLY: "1",
        COUNCIL_SESSIONS_DIR: path.join(councilRoot, "sessions"),
        COUNCIL_RUNS_DIR: path.join(councilRoot, "runs"),
      },
    },
    {
      command: `python3 -m http.server ${webPort} --bind 127.0.0.1 --directory "${exportRoot}"`,
      port: webPort,
      reuseExistingServer: false,
      timeout: 10_000,
    },
  ],
});
