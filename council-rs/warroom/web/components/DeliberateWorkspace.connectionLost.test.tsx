import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { CONNECTION_LOST_NOTICE, reduceDeliberationState } from "@/hooks/useDeliberation";
import { initialState } from "@/lib/types";
import DeliberateWorkspace from "./DeliberateWorkspace";

describe("DeliberateWorkspace connection-lost state", () => {
  const lost = reduceDeliberationState(
    {
      ...initialState,
      phase: "streaming",
      session_id: "active-session",
      topic: "Keep this proceeding honest",
    },
    { kind: "closed" },
  );

  it("renders the shared notice through one constant, not a copied string", () => {
    const html = renderToStaticMarkup(
      <DeliberateWorkspace
        state={lost}
        cabinets={[]}
        onStart={() => {}}
        onIntervene={() => {}}
        onReset={() => {}}
      />,
    );
    expect(html).toContain("Connection lost");
    expect(html).toContain(CONNECTION_LOST_NOTICE);
  });

  it("never offers a Reconnect that would replay the paid run", () => {
    const html = renderToStaticMarkup(
      <DeliberateWorkspace
        state={lost}
        cabinets={[]}
        onStart={() => {}}
        onIntervene={() => {}}
        onReset={() => {}}
      />,
    );
    expect(html).not.toContain("Reconnect");
    expect(html).toContain("Reset");
  });
});
