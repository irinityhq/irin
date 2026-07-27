/**
 * Header health label for the War Room shell.
 *
 * While the mount-time health retry window is in progress, the label stays
 * CONNECTING. OFFLINE is reserved for exhausted retries or a confirmed
 * backend failure (apiStatus error/online with no health payload after retries).
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
