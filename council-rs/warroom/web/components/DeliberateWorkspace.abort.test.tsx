import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { ABORT_NOTICE, reduceDeliberationState } from "@/hooks/useDeliberation";
import { initialState } from "@/lib/types";
import DeliberateWorkspace from "./DeliberateWorkspace";

describe("DeliberateWorkspace Abort state", () => {
  it("renders the local-stop and accepted-provider-request limitation", () => {
    const state = reduceDeliberationState(
      {
        ...initialState,
        phase: "streaming",
        session_id: "active-session",
        topic: "Stop this proceeding",
      },
      { kind: "aborted" },
    );

    const html = renderToStaticMarkup(
      <DeliberateWorkspace
        state={state}
        cabinets={[]}
        health={null}
        onStart={() => {}}
        onIntervene={() => {}}
        onReset={() => {}}
      />,
    );

    expect(html).toContain("Deliberation aborted");
    expect(html).toContain(ABORT_NOTICE);
  });
});
