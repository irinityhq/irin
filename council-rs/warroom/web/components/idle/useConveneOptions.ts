import { useState } from "react";

/**
 * Convene form field state (session controls + advanced tuning). Grouped so
 * IdlePanel reads as wiring and SessionControls receives one bag.
 */
export function useConveneOptions() {
  const [blind, setBlind] = useState(false);
  const [pause, setPause] = useState(true);
  const [maxRounds, setMaxRounds] = useState<number | "">("");
  const [mode, setMode] = useState<"teardown" | "pathfind" | "harden">("teardown");
  const [validate, setValidate] = useState(false);
  const [validateProvider, setValidateProvider] = useState<string>("grok_build");
  const [validateGate, setValidateGate] = useState(false);
  const [frameCheck, setFrameCheck] = useState(true);
  const [scopeAuditor, setScopeAuditor] = useState(false);
  const [budgetUsd, setBudgetUsd] = useState<number | "">("");
  const [tier, setTier] = useState("best");
  const [thenTearDown, setThenTearDown] = useState(false);
  const [specopsThreshold, setSpecopsThreshold] = useState(0.8);
  const [workerProvJson, setWorkerProvJson] = useState("");
  const [showAdvanced, setShowAdvanced] = useState(false);

  return {
    blind,
    setBlind,
    pause,
    setPause,
    maxRounds,
    setMaxRounds,
    mode,
    setMode,
    validate,
    setValidate,
    validateProvider,
    setValidateProvider,
    validateGate,
    setValidateGate,
    frameCheck,
    setFrameCheck,
    scopeAuditor,
    setScopeAuditor,
    budgetUsd,
    setBudgetUsd,
    tier,
    setTier,
    thenTearDown,
    setThenTearDown,
    specopsThreshold,
    setSpecopsThreshold,
    workerProvJson,
    setWorkerProvJson,
    showAdvanced,
    setShowAdvanced,
  };
}

export type ConveneOptions = ReturnType<typeof useConveneOptions>;
