import { buildProviderChoices } from "@/lib/use-discover";
import type { DiscoverProvider } from "@/lib/types";

export function providerSelectOptions(
  providers: DiscoverProvider[],
  currentProvider: string,
) {
  return (
    <>
      {buildProviderChoices(providers, currentProvider).map((provider) => (
        <option
          key={provider.name}
          value={provider.name}
          disabled={!provider.available}
        >
          {provider.label}
        </option>
      ))}
    </>
  );
}
