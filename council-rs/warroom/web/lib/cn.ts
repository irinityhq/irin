import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

export function fmtCost(usd: number): string {
  if (!Number.isFinite(usd) || usd === 0) return "—";
  if (usd < 0.001) return `$${usd.toFixed(5)}`;
  if (usd < 0.01) return `$${usd.toFixed(4)}`;
  return `$${usd.toFixed(3)}`;
}

export function fmtLatency(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

export function fmtTokens(n: number): string {
  if (n < 1000) return n.toString();
  if (n < 1_000_000) return `${(n / 1000).toFixed(1)}k`;
  return `${(n / 1_000_000).toFixed(2)}M`;
}

export function providerColor(p: string): string {
  switch (p) {
    case "grok":
    case "grok_cli":
    case "grok_api":
    case "grok_build":
    case "grok_hermes":
      return "magenta";
    case "claude":
    case "claude_api":
    case "claude_code":
      return "amber";
    case "gpt":
    case "openai_api":
    case "codex_cli":
      return "success";
    case "gemini":
    case "gemini_agy":
    case "gemini_vertex":
    case "gemini_cli":
      return "cyan";
    default:
      return "muted";
  }
}

/** Command-grade seat ledger left-bar class (History / Live). */
export function providerLedgerClass(provider: string): string {
  switch (provider) {
    case "gpt":
    case "codex_cli":
    case "openai_api":
      return "cg-seat-gpt";
    case "gemini":
    case "gemini_agy":
    case "gemini_vertex":
    case "gemini_cli":
      return "cg-seat-gemini";
    case "grok":
    case "grok_cli":
    case "grok_api":
    case "grok_build":
    case "grok_hermes":
      return "cg-seat-grok";
    case "claude":
    case "claude_api":
    case "claude_code":
      return "cg-seat-claude";
    default:
      return "";
  }
}

export function convergenceTone(score: number): "success" | "warning" | "danger" {
  if (score >= 0.8) return "success";
  if (score >= 0.5) return "warning";
  return "danger";
}

/**
 * Hex for a provider's tone — for SVG fills / inline styles where a dynamic
 * Tailwind utility (`fill-${tone}`) would not be generated. Mirrors the theme
 * tokens in tailwind.config.mjs.
 */
export function providerHex(p: string): string {
  switch (providerColor(p)) {
    case "magenta":
      return "#ff2e88";
    case "amber":
      return "#f5a623";
    case "success":
      return "#00ff9d";
    case "cyan":
      return "#00d4ff";
    default:
      return "#8a8a96";
  }
}
