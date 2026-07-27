/**
 * Host-authoritative desktop status snapshot helpers.
 *
 * The Tauri host assigns `authority_epoch` + monotonic `seq`. The renderer's
 * entire ordering logic is `applyIfNewer`: accept on epoch change, otherwise
 * only when `next.seq > prev.seq`. No fences, poll epochs, or actionBusy refs.
 */

import type {
  GatewayPackStatus,
  PhoneAccessStatus,
  TouchIdStatus,
} from "./tauri";
import { assertNoSecretFields } from "./touch-id";

export interface DesktopStatusSnapshot {
  authority_epoch: string;
  seq: number;
  pack: GatewayPackStatus;
  touch_id: TouchIdStatus;
  phone: PhoneAccessStatus;
}

/**
 * Apply `next` only when it is strictly newer than `prev`.
 *
 * - epoch change → accept (process restart / host authority reset)
 * - same epoch → accept only when `next.seq > prev.seq`
 * - older or equal seq → no-op
 */
export function applyIfNewer(
  prev: DesktopStatusSnapshot | null | undefined,
  next: DesktopStatusSnapshot,
): DesktopStatusSnapshot | null {
  assertNoSecretFields(next);
  if (!prev) return next;
  if (next.authority_epoch !== prev.authority_epoch) return next;
  if (next.seq > prev.seq) return next;
  return null;
}

/** Merge helper for React state setters. */
export function mergeIfNewer(
  prev: DesktopStatusSnapshot | null,
  next: DesktopStatusSnapshot,
): DesktopStatusSnapshot {
  return applyIfNewer(prev, next) ?? prev ?? next;
}
