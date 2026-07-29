/**
 * Header health label for the War Room shell.
 *
 * While the readiness-driven cold-start poll is active (`bootRetryActive`),
 * the label stays CONNECTING. OFFLINE is reserved for exhausted connecting
 * budget or a confirmed backend failure (apiStatus error/online with no
 * health payload after retries). Slow recovery after offline does not set
 * bootRetryActive — permanent failure stays visible.
 */

export type WarroomApiStatus = "loading" | "online" | "error";

export function warroomHealthLabel(
  healthVersion: string | null,
  streamVersion: string | null,
  apiStatus: WarroomApiStatus,
  bootRetryActive: boolean,
): string {
  if (healthVersion !== null && streamVersion !== null) {
    return `gen ${healthVersion} · stream ${streamVersion}`;
  }
  if (apiStatus === "loading" || bootRetryActive) {
    return "connecting";
  }
  return "offline";
}
