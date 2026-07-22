import path from "node:path";
import { defineConfig } from "@playwright/test";

const DEFAULT_COUNCIL_PORT = 8765;
const DEFAULT_WEB_PORT = 3010;
const RUNTIME_CONFIG_STORAGE_KEY = "warroom.runtime-config.v1";

const PROVIDER_ENV_KEYS = [
  "ANTHROPIC_API_KEY",
  "DEEPSEEK_API_KEY",
  "EXA_API_KEY",
  "FIRECRAWL_API_KEY",
  "FIREWORKS_API_KEY",
  "GEMINI_API_KEY",
  "GOOGLE_API_KEY",
  "GROQ_API_KEY",
  "MISTRAL_API_KEY",
  "NVIDIA_API_KEY",
  "NOUS_API_KEY",
  "OPENAI_API_KEY",
  "OPENROUTER_API_KEY",
  "SEMANTIC_SCHOLAR_API_KEY",
  "TAVILY_API_KEY",
  "TOGETHER_API_KEY",
  "XAI_API_KEY",
];

function resolveCouncilRsRoot(): string {
  const fromEnv = process.env.COUNCIL_RS_DIR?.trim();
  if (fromEnv) {
    return path.resolve(fromEnv);
  }
  // warroom/web → council-rs root
  return path.resolve(__dirname, "../..");
}

function resolvePort(name: string, fallback: number): number {
  const raw = process.env[name]?.trim();
  if (!raw) return fallback;
  const port = Number(raw);
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    throw new Error(`${name} must be a TCP port, got ${JSON.stringify(raw)}`);
  }
  return port;
}

const councilRsRoot = resolveCouncilRsRoot();
const councilBin = path.join(councilRsRoot, "target/release/council");
const sessionsDir = path.join(councilRsRoot, "sessions");
const runsDir = path.join(councilRsRoot, "runs");
const councilPort = resolvePort("PW_COUNCIL_PORT", DEFAULT_COUNCIL_PORT);
const webPort = resolvePort("PW_WEB_PORT", DEFAULT_WEB_PORT);
const webBaseUrl = `http://127.0.0.1:${webPort}`;
const apiBase = `http://127.0.0.1:${councilPort}`;
const wsBase = `ws://127.0.0.1:${councilPort}`;
const hasPortOverride =
  Boolean(process.env.PW_COUNCIL_PORT?.trim()) ||
  Boolean(process.env.PW_WEB_PORT?.trim());
const providerEnvUnsetArgs = PROVIDER_ENV_KEYS.map((key) => `-u ${key}`).join(
  " ",
);

export default defineConfig({
  testDir: "./e2e",
  timeout: 30_000,
  retries: 0,
  use: {
    baseURL: webBaseUrl,
    headless: true,
    ...(hasPortOverride
      ? {
          storageState: {
            cookies: [],
            origins: [
              {
                origin: webBaseUrl,
                localStorage: [
                  {
                    name: RUNTIME_CONFIG_STORAGE_KEY,
                    value: JSON.stringify({
                      apiBase,
                      wsBase,
                    }),
                  },
                ],
              },
            ],
          },
        }
      : {}),
  },
  projects: [{ name: "chromium", use: { browserName: "chromium" } }],
  webServer: [
    {
      command: `env ${providerEnvUnsetArgs} "${councilBin}" --serve --port ${councilPort}`,
      cwd: councilRsRoot,
      port: councilPort,
      reuseExistingServer: true,
      timeout: 10_000,
      env: {
        COUNCIL_DEV_NO_AUTH: "1",
        COUNCIL_WS_SMOKE_ONLY: "1",
        COUNCIL_SESSIONS_DIR: sessionsDir,
        COUNCIL_RUNS_DIR: runsDir,
      },
    },
    {
      command: `npm run start -- --hostname 127.0.0.1 --port ${webPort}`,
      port: webPort,
      reuseExistingServer: true,
      timeout: 10_000,
      env: {
        NEXT_PUBLIC_API_BASE: apiBase,
        NEXT_PUBLIC_WS_BASE: wsBase,
      },
    },
  ],
});
