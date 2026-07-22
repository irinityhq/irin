const SAFE_PROTOCOLS = new Set(["http:", "https:", "mailto:"]);
const RELATIVE_URL_BASE = "https://relative.invalid";

export function safeMarkdownUrl(url: string | undefined): string {
  if (!url) return "";

  const trimmed = url.trim();
  if (!trimmed) return "";

  try {
    const protocol = new URL(trimmed, RELATIVE_URL_BASE).protocol.toLowerCase();
    return SAFE_PROTOCOLS.has(protocol) ? trimmed : "";
  } catch {
    return "";
  }
}
