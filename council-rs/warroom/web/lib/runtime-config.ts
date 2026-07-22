/**
 * Runtime configuration for War Room (browser + Tauri static export).
 * Persistence: localStorage (primary) → NEXT_PUBLIC_* build defaults.
 */

export type RuntimeConfigKey =
  | "apiBase"
  | "wsBase"
  | "gatewayBase"
  | "authToken"
  | "councilPath"
  | "councilRoot"
  | "librarianBase";

export type RuntimeConfig = Record<RuntimeConfigKey, string>;

declare global {
  interface Window {
    __WARROOM_NATIVE_CONFIG__?: Partial<RuntimeConfig>;
  }
}

const STORAGE_KEY = "warroom.runtime-config.v1";

const DEFAULT_GATEWAY = "http://127.0.0.1:18080";

const BUILD_DEFAULTS: RuntimeConfig = {
  apiBase: process.env.NEXT_PUBLIC_API_BASE || "http://127.0.0.1:8765",
  wsBase: process.env.NEXT_PUBLIC_WS_BASE || "ws://127.0.0.1:8765",
  gatewayBase:
    process.env.NEXT_PUBLIC_GATEWAY_BASE ||
    process.env.GATEWAY_URL ||
    DEFAULT_GATEWAY,
  authToken: process.env.NEXT_PUBLIC_COUNCIL_AUTH_TOKEN || "",
  councilPath: "",
  councilRoot: "",
  librarianBase: process.env.LIBRARIAN_BASE_URL || "http://127.0.0.1:11435",
};

/**
 * A remotely served War Room uses one origin for web, Council API/WS, and the
 * Gateway proxy. Loopback remains the desktop and local-browser default.
 */
export function defaultsForPage(
  defaults: RuntimeConfig,
  pageUrl: string | undefined,
): RuntimeConfig {
  if (!pageUrl) return defaults;
  try {
    const page = new URL(pageUrl);
    if (
      (page.protocol !== "http:" && page.protocol !== "https:") ||
      isLoopbackUrl(page.origin)
    ) {
      return defaults;
    }
    const wsProtocol = page.protocol === "https:" ? "wss:" : "ws:";
    return {
      ...defaults,
      apiBase: page.origin,
      wsBase: `${wsProtocol}//${page.host}`,
      gatewayBase: page.origin,
    };
  } catch {
    return defaults;
  }
}

/** Stale device settings must not send a remote browser back to itself. */
export function dropRemoteLoopbackOverrides(
  local: Partial<RuntimeConfig>,
  defaults: RuntimeConfig,
): Partial<RuntimeConfig> {
  if (isLoopbackUrl(defaults.apiBase)) return local;
  const next = { ...local };
  for (const key of ["apiBase", "wsBase", "gatewayBase"] as const) {
    if (next[key] && isLoopbackUrl(next[key])) {
      delete next[key];
    }
  }
  return next;
}

let cache: RuntimeConfig | null = null;
let loadPromise: Promise<RuntimeConfig> | null = null;
let readyResolve: ((c: RuntimeConfig) => void) | null = null;

/** Resolves after the first successful loadRuntimeConfig() in the browser. */
export const configReady: Promise<RuntimeConfig> = new Promise((resolve) => {
  readyResolve = resolve;
});

function isBrowser(): boolean {
  return typeof window !== "undefined";
}

/** Treat trimmed empty strings as unset so stored "" does not block defaults. */
export function pickConfigValue(
  stored: string | undefined,
  fallback: string,
): string {
  if (stored !== undefined && stored.trim() !== "") {
    return stored.trim();
  }
  return fallback;
}

export function pickOptionalConfigValue(
  stored: string | undefined,
  fallback: string,
): string {
  return stored === undefined ? fallback : stored.trim();
}

export function mergeConfigSources(
  local: Partial<RuntimeConfig>,
  defaults: RuntimeConfig = BUILD_DEFAULTS,
): RuntimeConfig {
  return {
    apiBase: pickConfigValue(local.apiBase, defaults.apiBase),
    wsBase: pickConfigValue(local.wsBase, defaults.wsBase),
    gatewayBase: pickOptionalConfigValue(
      local.gatewayBase,
      defaults.gatewayBase,
    ),
    authToken: pickConfigValue(local.authToken, defaults.authToken),
    councilPath: pickConfigValue(local.councilPath, defaults.councilPath),
    councilRoot: pickConfigValue(local.councilRoot, defaults.councilRoot),
    librarianBase: pickConfigValue(local.librarianBase, defaults.librarianBase),
  };
}

function readLocalStorage(): Partial<RuntimeConfig> {
  if (!isBrowser()) return {};
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return {};
    const parsed = JSON.parse(raw) as Partial<RuntimeConfig>;
    return parsed && typeof parsed === "object" ? parsed : {};
  } catch {
    return {};
  }
}

function readNativeConfig(): Partial<RuntimeConfig> {
  if (!isBrowser()) return {};
  const native = window.__WARROOM_NATIVE_CONFIG__;
  return native && typeof native === "object" ? native : {};
}

function mergedRuntimeConfig(): RuntimeConfig {
  const defaults = defaultsForPage(
    BUILD_DEFAULTS,
    isBrowser() ? window.location.href : undefined,
  );
  const local = dropRemoteLoopbackOverrides(readLocalStorage(), defaults);
  return mergeConfigSources({ ...local, ...readNativeConfig() }, defaults);
}

function writeLocalStorage(partial: Partial<RuntimeConfig>): void {
  if (!isBrowser()) return;
  const merged = { ...readLocalStorage(), ...partial };
  localStorage.setItem(STORAGE_KEY, JSON.stringify(merged));
}

function markReady(cfg: RuntimeConfig): RuntimeConfig {
  readyResolve?.(cfg);
  readyResolve = null;
  return cfg;
}

/** Load and cache runtime config (safe to call repeatedly). */
export async function loadRuntimeConfig(): Promise<RuntimeConfig> {
  if (cache) return cache;
  if (!loadPromise) {
    loadPromise = Promise.resolve().then(() => {
      cache = markReady(mergedRuntimeConfig());
      return cache;
    });
  }
  return loadPromise;
}

/** Synchronous getters after first load; uses build defaults until hydrated. */
export function getRuntimeConfig(): RuntimeConfig {
  if (cache) return cache;
  return mergedRuntimeConfig();
}

export function getApiBase(): string {
  return getRuntimeConfig().apiBase;
}

export function getWsBase(): string {
  return getRuntimeConfig().wsBase;
}

export function getGatewayBase(): string {
  return getRuntimeConfig().gatewayBase;
}

export function getAuthToken(): string {
  return getRuntimeConfig().authToken;
}

export function getCouncilPath(): string {
  return getRuntimeConfig().councilPath;
}

/** Sidecar --base-dir override (feature contract) — empty means repo-root default. */
export function getCouncilRoot(): string {
  return getRuntimeConfig().councilRoot;
}

export function getLibrarianBase(): string {
  return getRuntimeConfig().librarianBase;
}

/** True when URL host is loopback (127.0.0.1 / localhost / ::1). */
export function isLoopbackUrl(url: string): boolean {
  try {
    const host = new URL(url.trim()).hostname.toLowerCase();
    return host === "127.0.0.1" || host === "localhost" || host === "::1";
  } catch {
    return false;
  }
}

/** Persist overrides and refresh in-memory cache. */
export async function saveRuntimeConfig(
  partial: Partial<RuntimeConfig>,
): Promise<RuntimeConfig> {
  writeLocalStorage(partial);
  cache = mergedRuntimeConfig();
  loadPromise = Promise.resolve(cache);
  markReady(cache);
  if (isBrowser()) {
    window.dispatchEvent(new CustomEvent("warroom-config-changed"));
  }
  return cache;
}

function invalidateCacheFromStorage(): void {
  if (!isBrowser()) return;
  cache = mergedRuntimeConfig();
  loadPromise = Promise.resolve(cache);
  window.dispatchEvent(new CustomEvent("warroom-config-changed"));
}

let storageListenerInstalled = false;

/** Call once on app mount to hydrate from localStorage. */
export function initRuntimeConfig(): void {
  if (!isBrowser()) return;
  void loadRuntimeConfig();
  if (!storageListenerInstalled) {
    storageListenerInstalled = true;
    window.addEventListener("storage", (ev) => {
      if (ev.key === STORAGE_KEY || ev.key === null) {
        invalidateCacheFromStorage();
      }
    });
  }
}
