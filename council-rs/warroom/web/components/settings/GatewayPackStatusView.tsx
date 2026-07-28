import { gatewayPackStateLabel } from "@/lib/gateway-pack";
import type { GatewayPackStatus } from "@/lib/tauri";

export function GatewayPackStatusView({
  status,
}: {
  status: GatewayPackStatus | null;
}) {
  return (
    <div
      className="text-xs font-mono space-y-1"
      data-testid="settings-gateway-pack-status"
    >
      <div>
        State:{" "}
        <span className="text-cyan">
          {status
            ? gatewayPackStateLabel(status.state)
            : "checking…"}
        </span>
      </div>
      {status?.message ? (
        <p className="text-fg-dim whitespace-pre-wrap">{status.message}</p>
      ) : null}
      {status?.key_id ? (
        <div className="text-fg-dim">Key id: {status.key_id}</div>
      ) : null}
      {status?.pack_version ? (
        <div className="text-fg-dim">Pack: {status.pack_version}</div>
      ) : null}
      <div className="text-fg-dim">
        Watch: producer=
        {String(status?.watch_producer_enabled ?? false)} dispatcher=
        {String(status?.watch_dispatcher_enabled ?? false)}
      </div>
    </div>
  );
}
