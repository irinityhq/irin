import { api } from "./api";
import { isTauri, savePdf } from "./tauri";

/** Trigger a browser download from a Blob (works in the Tauri webview too). */
export function triggerBlobDownload(blob: Blob, name: string) {
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = name;
  a.click();
  URL.revokeObjectURL(url);
}

/**
 * N06 — fetch the server-rendered PDF for a session and download it. Throws on
 * a non-2xx response so callers can surface the failure.
 * In Tauri, uses native OS save dialog via Rust command for proper native save.
 */
export async function downloadSessionPdf(id: string): Promise<void> {
  const blob = await api.exportSessionPdf(id);
  if (isTauri()) {
    const buf = await blob.arrayBuffer();
    const bytes = new Uint8Array(buf);
    await savePdf(bytes, `council_${id}.pdf`);
    return;
  }
  triggerBlobDownload(blob, `council_${id}.pdf`);
}
