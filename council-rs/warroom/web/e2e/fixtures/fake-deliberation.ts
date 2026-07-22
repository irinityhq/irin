import { type Page, type WebSocketRoute } from "@playwright/test";

export interface StreamEvent {
  type: string;
  session_id: string;
  ts: string;
  data: Record<string, unknown>;
}

export interface DeliberationScriptStep {
  delayMs?: number;
  event: StreamEvent;
}

export interface InterventionWire {
  type: "intervention";
  payload: {
    action: string;
    text?: string;
    seat_name?: string;
    provider?: string;
    model?: string;
    system?: string;
  };
}

export interface FakeDeliberationTimings {
  /** Hold after client `start` before `session_started` (connecting spinner). */
  connectingHoldMs?: number;
  /** Delay after KISS click before `intervention_received` (Applying KISS…). */
  kissPendingMs?: number;
}

export interface FakeDeliberationOptions {
  sessionId?: string;
  topic?: string;
  cabinet?: string;
  timings?: FakeDeliberationTimings;
  seats?: Array<{ name: string; provider: string; model: string }>;
}

const DEFAULT_SEATS = [
  { name: "CFO", provider: "grok_cli", model: "grok-build" },
  { name: "Mirror", provider: "gemini", model: "gemini-3.1-pro" },
  { name: "Red Team", provider: "grok_cli", model: "grok-build" },
  { name: "Constraint", provider: "gpt", model: "gpt-5.6-sol" },
  { name: "Operator", provider: "claude", model: "claude-opus-4-8" },
];

const ALL_INTERVENTION_OPTIONS = [
  "continue",
  "end_early",
  "escalate_specops",
  "escalate_munger",
  "escalate_contrarian",
  "escalate_kiss",
  "inject_context",
  "swap_seat",
] as const;

/** Captured client → server intervention frames. */
export type CapturedIntervention = InterventionWire["payload"];

export class FakeDeliberationHarness {
  readonly interventions: CapturedIntervention[] = [];
  private sessionId: string;
  private topic: string;
  private cabinet: string;
  private seats: FakeDeliberationOptions["seats"];
  private timings: Required<FakeDeliberationTimings>;
  private ws: WebSocketRoute | null = null;

  constructor(opts: FakeDeliberationOptions = {}) {
    this.sessionId = opts.sessionId ?? "e2e_fake_session_001";
    this.topic = opts.topic ?? "E2E harness topic — package update cadence";
    this.cabinet = opts.cabinet ?? "warroom";
    this.seats = opts.seats ?? DEFAULT_SEATS;
    this.timings = {
      connectingHoldMs: opts.timings?.connectingHoldMs ?? 600,
      kissPendingMs: opts.timings?.kissPendingMs ?? 1600,
    };
  }

  /** Intercept `/ws/deliberate` — REST still hits real backend on :8765. */
  async install(page: Page): Promise<void> {
    await page.route("**/api/interventions/predict**", async (route) => {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          probability: 0.2,
          method: "frequency",
          n_samples: 12,
        }),
      });
    });

    await page.routeWebSocket("**/ws/deliberate", async (ws: WebSocketRoute) => {
      this.ws = ws;
      let started = false;

      ws.onMessage(async (message: string | Buffer) => {
        let data: { type?: string; payload?: CapturedIntervention };
        try {
          data = JSON.parse(
            typeof message === "string" ? message : message.toString(),
          );
        } catch {
          return;
        }

        if (data?.type === "start" && !started) {
          started = true;
          await this.runOpeningScript();
          return;
        }

        if (data?.type === "intervention" && data.payload) {
          this.interventions.push(data.payload);
          await this.handleIntervention(data.payload);
        }
      });
    });
  }

  private send(ev: StreamEvent): void {
    if (!this.ws) return;
    this.ws.send(JSON.stringify(ev));
  }

  private ev(type: string, data: Record<string, unknown>): StreamEvent {
    return {
      type,
      session_id: this.sessionId,
      ts: new Date().toISOString(),
      data,
    };
  }

  private async delay(ms: number): Promise<void> {
    if (ms > 0) await new Promise((r) => setTimeout(r, ms));
  }

  private async runOpeningScript(): Promise<void> {
    await this.delay(this.timings.connectingHoldMs);

    this.send(
      this.ev("session_started", {
        topic: this.topic,
        cabinet_name: this.cabinet,
        rounds_planned: 2,
        mode: "normal",
        active_seats: this.seats,
        dropped_seats: [],
        chair: { provider: "claude", model: "claude-opus-4-8" },
        available_providers: ["grok_cli", "claude", "gpt", "gemini"],
        council_version: "e2e",
        stream_version: "e2e",
        deliberation_mode: "teardown",
        tier: "best",
      }),
    );

    await this.delay(200);
    this.send(this.ev("round_started", { round_num: 1 }));

    const seat = this.seats![0];
    await this.delay(150);
    this.send(
      this.ev("seat_started", {
        round_num: 1,
        seat_name: seat.name,
        provider: seat.provider,
        model: seat.model,
      }),
    );
    await this.delay(120);
    this.send(
      this.ev("seat_chunk", {
        round_num: 1,
        seat_name: seat.name,
        text_delta: "Restated problem: ",
        seq: 1,
      }),
    );
    await this.delay(100);
    this.send(
      this.ev("seat_complete", {
        round_num: 1,
        seat_name: seat.name,
        provider: seat.provider,
        model: seat.model,
        text: "Restated problem: weekly security patches matter.",
        latency_ms: 800,
        tokens_in: 120,
        tokens_out: 80,
        cached_in: 0,
        cost_usd: 0.002,
        error: null,
      }),
    );

    await this.delay(80);
    this.send(
      this.ev("convergence_scored", {
        round_num: 1,
        score: 0.92,
        converged: true,
        early_convergence: true,
      }),
    );
    this.send(this.ev("round_complete", { round_num: 1, early_convergence: true }));

    await this.delay(100);
    this.send(this.awaitingPause(1));
  }

  private awaitingPause(roundNum: number, specopsSignal?: string): StreamEvent {
    const data: Record<string, unknown> = {
      round_num: roundNum,
      convergence: 0.92,
      converged: true,
      options: [...ALL_INTERVENTION_OPTIONS],
    };
    if (specopsSignal) data.specops_signal = specopsSignal;
    return this.ev("awaiting_input", data);
  }

  private async handleIntervention(payload: CapturedIntervention): Promise<void> {
    const action = payload.action;

    if (action === "escalate_kiss") {
      await this.delay(this.timings.kissPendingMs);
      this.send(this.ev("intervention_received", { action }));
      await this.delay(250);
      this.send(
        this.awaitingPause(
          1,
          "KISS escalation result — strip to essentials: patch cadence follows severity × reachability.",
        ),
      );
      return;
    }

    if (action === "inject_context") {
      await this.delay(400);
      this.send(this.ev("intervention_received", { action, text: payload.text }));
      await this.delay(200);
      this.send(this.awaitingPause(1));
      return;
    }

    if (action === "continue") {
      await this.delay(350);
      this.send(this.ev("intervention_received", { action }));
      await this.delay(200);
      this.send(this.ev("synthesis_started", { model: "claude-opus-4-8" }));
      await this.delay(400);
      const ruling =
        "## Chair Ruling\n\nConsensus: patch on severity, not calendar.\n\n1. **Consensus** — weekly for critical deps.";
      this.send(
        this.ev("synthesis_complete", {
          text: ruling,
          model: "claude-opus-4-8",
          latency_ms: 1200,
          cost_usd: 0.015,
          tokens_in: 400,
          tokens_out: 200,
        }),
      );
      await this.delay(150);
      this.send(
        this.ev("done", {
          total_tokens: 1800,
          total_cost_usd: 0.042,
          total_latency_ms: 95000,
          synthesis: ruling,
          session_id: this.sessionId,
          convergence_final: 0.92,
          rounds_run: 1,
        }),
      );
      return;
    }

    // Default ack for other actions in extended tests.
    await this.delay(300);
    this.send(this.ev("intervention_received", { action }));
    await this.delay(150);
    this.send(this.awaitingPause(1));
  }
}

/** Legacy helper — static script replay (no intervention handling). */
export async function setupFakeDeliberation(
  page: Page,
  script: DeliberationScriptStep[],
  opts: FakeDeliberationOptions = {},
) {
  const sessionId = opts.sessionId ?? "test_session_001";

  await page.routeWebSocket("**/ws/deliberate", async (ws: WebSocketRoute) => {
    let started = false;

    ws.onMessage(async (message: string | Buffer) => {
      let data: { type?: string };
      try {
        data = JSON.parse(
          typeof message === "string" ? message : message.toString(),
        );
      } catch {
        return;
      }

      if (data?.type === "start" && !started) {
        started = true;
        for (const step of script) {
          if (step.delayMs && step.delayMs > 0) {
            await new Promise((r) => setTimeout(r, step.delayMs));
          }
          const ev = { ...step.event, session_id: sessionId };
          if (!ev.ts) ev.ts = new Date().toISOString();
          ws.send(JSON.stringify(ev));
        }
      }
    });
  });
}

export function makeEvent(
  type: string,
  data: Record<string, unknown>,
  sessionId = "test_session_001",
): DeliberationScriptStep {
  return {
    event: {
      type,
      session_id: sessionId,
      ts: new Date().toISOString(),
      data,
    },
  };
}
