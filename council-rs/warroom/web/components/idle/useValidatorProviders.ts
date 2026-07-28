import { useMemo } from "react";
import type { DiscoverProvider } from "@/lib/types";
import { getProviderOption } from "@/lib/use-discover";

export function useValidatorProviders(providerOptions: DiscoverProvider[]) {
  return useMemo(() => {
    const allowed = [
      "grok_build",
      "grok_hermes",
      "grok_api",
      "claude_code",
      "claude_api",
      "codex_cli",
      "openai_api",
      "gemini_agy",
      "gemini_vertex",
    ];
    return allowed
      .map((id) => getProviderOption(providerOptions, id))
      .filter((provider): provider is NonNullable<typeof provider> => !!provider);
  }, [providerOptions]);
}
