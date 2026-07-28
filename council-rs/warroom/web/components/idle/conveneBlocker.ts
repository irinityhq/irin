import { noRunnableCabinetExplanation } from "@/lib/cabinet-selection";
import type { Cabinet, DiscoverProvider, DiscoverResponse } from "@/lib/types";
import {
  unavailableProviderReason,
  unsupportedGatewayTransportReason,
} from "@/lib/use-discover";

/**
 * Single blocking reason for the convene button, in precedence order:
 * discovery failure → still loading → no runnable cabinet → cabinet/validator
 * provider gaps → gateway transport gaps. Null means convene may proceed
 * (topic-length gating is separate).
 */
export function conveneBlocker({
  discoverData,
  discoverLoading,
  discoverError,
  cabinets,
  availableIds,
  cabinet,
  providerOptions,
  validate,
  validateProvider,
  viaGateway,
}: {
  discoverData: DiscoverResponse | null;
  discoverLoading: boolean;
  discoverError: string | null;
  cabinets: Cabinet[];
  availableIds: string[] | null;
  cabinet: Cabinet | undefined;
  providerOptions: DiscoverProvider[];
  validate: boolean;
  validateProvider: string;
  viaGateway: boolean;
}): string | null {
  const cabinetProviderProblem = cabinet
    ? unavailableProviderReason(providerOptions, [
        ...cabinet.seats.map((seat) => seat.provider),
        cabinet.chair.provider,
      ])
    : "Selected cabinet was not found.";
  const validatorProviderProblem = validate
    ? unavailableProviderReason(providerOptions, [validateProvider])
    : null;
  const selectedProviderIds = cabinet
    ? [
        ...cabinet.seats.map((seat) => seat.provider),
        cabinet.chair.provider,
        ...(validate ? [validateProvider] : []),
      ]
    : [];
  const gatewayProviderProblem = viaGateway
    ? unsupportedGatewayTransportReason(providerOptions, selectedProviderIds)
    : null;
  const noRunnableExplanation = noRunnableCabinetExplanation(
    cabinets,
    availableIds,
  );
  return discoverError
    ? `Provider discovery failed: ${discoverError}`
    : !discoverData || discoverLoading
      ? "Provider availability is still being checked."
      : noRunnableExplanation
        ?? cabinetProviderProblem
        ?? validatorProviderProblem
        ?? gatewayProviderProblem;
}
