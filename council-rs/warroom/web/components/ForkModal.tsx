"use client";

import { useState } from "react";
import { motion, AnimatePresence } from "framer-motion";
import { GitFork, Play, X } from "lucide-react";
import { api } from "@/lib/api";
import { cn } from "@/lib/cn";
import {
  buildProviderChoices,
  getModelsForProvider,
  getProviderOption,
  unavailableProviderReason,
  useDiscover,
} from "@/lib/use-discover";
import { buildForkStartPayload } from "@/lib/fork-launch";
import type {
  DiscoverProvider,
  ForkResult,
  SeatSwap,
  SessionIndexEntry,
} from "@/lib/types";
import type { StartPayload } from "@/lib/ws";

/**
 * Fork-and-vary modal. Loads the parent session, lets the user edit any
 * seat's provider/model/system prompt, then opens a new deliberation via
 * the supplied `onLaunch` callback (which should drive the WebSocket).
 */
export default function ForkModal({
  parent,
  onClose,
  onLaunch,
}: {
  parent: SessionIndexEntry;
  onClose: () => void;
  onLaunch: (start: StartPayload) => void;
}) {
  const [resolved, setResolved] = useState<ForkResult | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [edits, setEdits] = useState<Record<string, SeatSwap>>({});
  const [pauseEachRound, setPauseEachRound] = useState(true);

  // B4: dynamic provider/model selects from discover (with custom fallback)
  const {
    data: discoverData,
    loading: discoverLoading,
    error: discoverError,
    providerModelMap,
    providerOptions,
  } = useDiscover();

  const providerAvailabilityProblem = resolved
    ? discoverError
      ? `Provider discovery failed: ${discoverError}. Rescan from Discover before launching.`
      : !discoverData || discoverLoading
        ? "Provider availability is still being checked. Wait for discovery before launching."
        : unavailableProviderReason(providerOptions, [
            ...resolved.cabinet.seats.map(
              (seat) => edits[seat.name]?.provider ?? seat.provider,
            ),
            resolved.cabinet.chair.provider,
          ])
    : null;

  const ensureResolved = async () => {
    if (resolved) return resolved;
    setLoading(true); setError(null);
    try {
      const r = await api.fork(parent.id, []);
      setResolved(r);
      return r;
    } catch (e) {
      setError((e as Error).message);
      return null;
    } finally {
      setLoading(false);
    }
  };

  // Lazy-load on first interaction
  if (!resolved && !loading && !error) ensureResolved();

  const updateSeat = (name: string, field: "provider" | "model" | "system", value: string) => {
    setEdits((prev) => ({ ...prev, [name]: { ...prev[name], seat_name: name, [field]: value } }));
  };

  const launch = async () => {
    const r = resolved ?? (await ensureResolved());
    if (!r) return;
    if (providerAvailabilityProblem) {
      setError(providerAvailabilityProblem);
      return;
    }
    onLaunch(buildForkStartPayload(r, edits, pauseEachRound));
    onClose();
  };

  return (
    <AnimatePresence>
      <motion.div
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        exit={{ opacity: 0 }}
        className="fixed inset-0 bg-black/70 backdrop-blur-sm z-50 flex items-center justify-center p-4"
        onClick={onClose}
      >
        <motion.div
          initial={{ scale: 0.96, opacity: 0 }}
          animate={{ scale: 1, opacity: 1 }}
          exit={{ scale: 0.96, opacity: 0 }}
          onClick={(e) => e.stopPropagation()}
          className="panel-glass border-amber/40 max-w-3xl w-full max-h-[90vh] overflow-y-auto"
        >
          <div className="flex items-center justify-between px-5 py-4 border-b border-border">
            <div className="flex items-center gap-2">
              <GitFork className="w-4 h-4 text-amber" />
              <span className="font-display font-bold text-fg-bright">
                Fork &amp; vary
              </span>
              <span className="chip chip-amber">{parent.id}</span>
            </div>
            <button onClick={onClose} className="text-fg-dim hover:text-fg">
              <X className="w-4 h-4" />
            </button>
          </div>

          <div className="p-5 space-y-4">
            <div className="text-sm text-fg leading-snug">
              {parent.topic}
            </div>

            {loading && (
              <div className="font-mono text-cyan text-sm animate-pulse-cyan p-4">
                Loading parent cabinet…
              </div>
            )}

            {error && (
              <div className="text-sm text-danger font-mono">
                {error}
              </div>
            )}

            {resolved && (
              <>
                <div className="text-xs font-mono text-fg-dim border-b border-border pb-2">
                  Cabinet: <span className="text-amber">{resolved.parent_cabinet_label}</span>
                  {" · "}
                  {resolved.cabinet.seats.length} seats
                </div>
                <div className="space-y-3">
                  {resolved.cabinet.seats.map((s) => (
                    <div key={s.name} className="border border-border rounded-md p-3 bg-bg-overlay/40">
                      <div className="flex items-center gap-2 mb-2">
                        <span className="font-display font-semibold text-fg-bright text-sm">
                          {s.name}
                        </span>
                        <span className="chip text-[10px]">parent: {s.provider}/{s.model}</span>
                      </div>
                      <div className="grid grid-cols-2 gap-2 mb-2">
                        {(() => {
                          const currentProv = edits[s.name]?.provider ?? s.provider;
                          return (
                            <select
                              className="input text-xs"
                              value={currentProv}
                              onChange={(e) => {
                                const newProv = e.target.value;
                                updateSeat(s.name, "provider", newProv);
                                const newModels = getModelsForProvider(providerModelMap, newProv);
                                if (newModels.length > 0) {
                                  // auto-pick first discovered model for the new provider
                                  updateSeat(s.name, "model", newModels[0]);
                                }
                              }}
                            >
                              {providerSelectOptions(providerOptions, currentProv)}
                            </select>
                          );
                        })()}
                        {(() => {
                          const currentProv = (edits[s.name]?.provider as string) || s.provider;
                          const provModels = getModelsForProvider(providerModelMap, currentProv);
                          const currentModel = edits[s.name]?.model ?? s.model;
                          if (provModels.length > 0) {
                            return (
                              <select
                                className="input text-xs"
                                value={currentModel}
                                onChange={(e) => updateSeat(s.name, "model", e.target.value)}
                              >
                                {provModels.map((m) => (
                                  <option key={m} value={m}>{m}</option>
                                ))}
                                {!provModels.includes(currentModel) && currentModel && (
                                  <option value={currentModel}>{currentModel} (custom)</option>
                                )}
                              </select>
                            );
                          }
                          return (
                            <input
                              className="input text-xs"
                              value={currentModel}
                              onChange={(e) => updateSeat(s.name, "model", e.target.value)}
                            />
                          );
                        })()}
                      </div>
                      {discoverData && !getProviderOption(
                        providerOptions,
                        edits[s.name]?.provider ?? s.provider,
                      )?.available && (
                        <div className="mb-2 text-[10px] font-mono text-danger">
                          {edits[s.name]?.provider ?? s.provider} is unavailable or is a legacy provider ID. Choose an available transport.
                        </div>
                      )}
                      <textarea
                        rows={3}
                        className="input text-xs font-mono"
                        placeholder="(override system prompt — leave empty to keep parent's)"
                        defaultValue=""
                        onChange={(e) => updateSeat(s.name, "system", e.target.value)}
                      />
                    </div>
                  ))}
                </div>
                {providerAvailabilityProblem && (
                  <div
                    data-testid="fork-provider-warning"
                    className="text-[11px] font-mono text-danger"
                  >
                    {providerAvailabilityProblem}
                  </div>
                )}
                <div className="flex items-center justify-end gap-3 pt-2">
                  <label className="flex items-center gap-1.5 text-xs font-mono text-fg-muted cursor-pointer mr-auto">
                    <input
                      type="checkbox"
                      checked={pauseEachRound}
                      onChange={(e) => setPauseEachRound(e.target.checked)}
                      className="w-3.5 h-3.5"
                    />
                    Pause after each round
                  </label>
                  <button onClick={onClose} className="btn text-sm">Cancel</button>
                  <button
                    onClick={launch}
                    disabled={!!providerAvailabilityProblem}
                    title={providerAvailabilityProblem ?? undefined}
                    className={cn("btn btn-primary text-sm")}
                  >
                    <Play className="w-3.5 h-3.5" />
                    Run forked deliberation
                  </button>
                </div>
              </>
            )}
          </div>
        </motion.div>
      </motion.div>
    </AnimatePresence>
  );
}

function providerSelectOptions(
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
