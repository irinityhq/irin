import { gatewayStartFields } from "@/lib/gateway-mode";
import type { Cabinet } from "@/lib/types";
import type { GatewaySensitivity, StartPayload } from "@/lib/ws";
import type { ToastType } from "../Toast";
import type { ConveneMatter } from "./useConveneMatter";
import type { ConveneOptions } from "./useConveneOptions";

export function useConveneSubmit({
  canStart,
  matter,
  options,
  cabinetName,
  cabinet,
  viaGateway,
  sensitivity,
  toast,
  onStart,
}: {
  canStart: boolean;
  matter: ConveneMatter;
  options: ConveneOptions;
  cabinetName: string;
  cabinet?: Cabinet;
  viaGateway: boolean;
  sensitivity: GatewaySensitivity;
  toast: (type: ToastType, message: string) => void;
  onStart: (p: StartPayload) => void;
}) {
  const { topic, context, mapDir, mapBrief, submitting, setSubmitting } = matter;
  const {
    blind,
    pause,
    maxRounds,
    mode,
    thenTearDown,
    budgetUsd,
    tier,
    specopsThreshold,
    workerProvJson,
    validate,
    validateProvider,
    validateGate,
    frameCheck,
    scopeAuditor,
  } = options;

  return () => {
    if (!canStart || submitting) return;
    // If the Mapmaker produced a brief, inject it as structured context and
    // suppress the raw --map bundle so the council doesn't see both.
    const briefBlock = mapBrief
      ? `--- EXECUTION MAP (Mapmaker · ${mapBrief.result.model}) ---\n${mapBrief.text}`
      : null;
    const finalContext =
      [context.trim() || null, briefBlock].filter(Boolean).join("\n\n") || undefined;
    let worker_provenance: Record<string, unknown> | undefined;
    if (workerProvJson.trim()) {
      try {
        worker_provenance = JSON.parse(workerProvJson) as Record<string, unknown>;
      } catch {
        toast("error", "worker_provenance must be valid JSON");
        return;
      }
    }
    setSubmitting(true);
    onStart({
      topic: topic.trim(),
      cabinet_name: cabinetName,
      context: finalContext,
      map_dir: mapBrief ? undefined : (mapDir || undefined),
      blind,
      pause_after_each_round: pause,
      max_rounds: typeof maxRounds === "number" ? maxRounds : undefined,
      mode: thenTearDown ? "pathfind" : mode,
      then_tear_down: thenTearDown,
      budget_max_usd: typeof budgetUsd === "number" ? budgetUsd : undefined,
      tier,
      auto_specops_threshold: specopsThreshold,
      worker_provenance,
      validate,
      validate_provider: validate ? validateProvider : undefined,
      validate_gate: validate ? validateGate : undefined,
      frame_check: cabinet?.local_code_only ? false : frameCheck,
      ...(scopeAuditor ? { scope_auditor: true } : {}),
      // Interactive War Room starts always declare Direct or Governed.
      ...gatewayStartFields(viaGateway, sensitivity),
    });
  };
}
