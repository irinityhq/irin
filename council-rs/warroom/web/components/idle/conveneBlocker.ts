import { noRunnableCabinetExplanation } from "@/lib/cabinet-selection";
import type { Cabinet, DiscoverProvider, DiscoverResponse } from "@/lib/types";
import {
  unavailableProviderReason,
  unsupportedGatewayTransportReason,
} from "@/lib/use-discover";

/** Cold-start / first-scan tone — not a permanent provider failure. */
export const COUNCIL_LOADING_BLOCKER = "Council loading…";

/**
 * Single blocking reason for the convene button, in precedence order:
 * still loading / empty inventory → terminal discovery failure → no runnable
 * cabinet → cabinet/validator provider gaps → gateway transport gaps.
 * Null means convene may proceed (topic-length gating is separate).
 *
 * Cold-start races used to surface sticky "Provider discovery failed" while
 * Council was still binding (adapter preflight + spawn). Prefer a loading
 * tone until inventory exists or retries are clearly exhausted.
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

  // Loading / not-yet-scanned: never look like a permanent failure.
  if (discoverLoading || !discoverData) {
    if (discoverLoading || discoverError === null) {
      return COUNCIL_LOADING_BLOCKER;
    }
    // Idle with no inventory after retries exhausted → honest failure.
    return `Provider discovery failed: ${discoverError}`;
  }

  return (
    noRunnableExplanation
    ?? cabinetProviderProblem
    ?? validatorProviderProblem
    ?? gatewayProviderProblem
  );
}

/** True when the convene blocker is cold-start loading tone (not danger). */
export function isCouncilLoadingBlocker(message: string | null): boolean {
  return message === COUNCIL_LOADING_BLOCKER;
}
