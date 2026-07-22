const DEFAULT_COUNCIL_PORT = "8765";
const DEFAULT_WEB_PORT = "3010";

function envPort(name: string, fallback: string): string {
  const raw = process.env[name]?.trim();
  return raw || fallback;
}

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

export const COUNCIL_PORT = envPort("PW_COUNCIL_PORT", DEFAULT_COUNCIL_PORT);
export const WEB_PORT = envPort("PW_WEB_PORT", DEFAULT_WEB_PORT);
export const BACKEND = `http://127.0.0.1:${COUNCIL_PORT}`;
export const WS_DELIBERATE_URL = `ws://127.0.0.1:${COUNCIL_PORT}/ws/deliberate`;
export const WEB_ORIGIN_RE = new RegExp(
  `(?:127\\.0\\.0\\.1|localhost):${escapeRegExp(WEB_PORT)}`,
);
