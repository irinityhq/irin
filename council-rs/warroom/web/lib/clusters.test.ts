import { describe, expect, it } from "vitest";
import {
  buildThemeRows,
  cleanThemeTerms,
  clusterCountView,
  clusterSampleTopic,
  clusterSessionIds,
  formatClusterCount,
  formatThemeLabel,
  normalizeClustersResponse,
  selectedThemeLabels,
} from "./clusters";
import type { SessionCluster } from "./types";

describe("normalizeClustersResponse", () => {
  it("passes a well-formed payload through, sorted largest-first", () => {
    const r = normalizeClustersResponse({
      clusters: [
        { id: 0, size: 3, top_terms: ["risk", "ship"], session_ids: ["a", "b", "c"] },
        { id: 1, size: 5, top_terms: ["cost"], session_ids: ["d"] },
      ],
      method: "kmeans",
      k: 2,
      n_sessions: 8,
      generated_at: "2026-06-06T00:00:00Z",
    });
    expect(r.clusters.map((c) => c.id)).toEqual([1, 0]);
    expect(r.method).toBe("kmeans");
    expect(r.k).toBe(2);
    expect(r.n_sessions).toBe(8);
  });

  it("returns empty clusters for an empty index (200 empty case)", () => {
    const r = normalizeClustersResponse({
      clusters: [],
      method: "kmeans",
      k: 0,
      n_sessions: 0,
      generated_at: "2026-06-06T00:00:00Z",
    });
    expect(r.clusters).toEqual([]);
    expect(r.n_sessions).toBe(0);
  });

  it("drops malformed clusters and non-string terms/ids", () => {
    const r = normalizeClustersResponse({
      clusters: [
        { id: 0, size: 2, top_terms: ["ok", "", 7, null], session_ids: ["x", "", 3] },
        { id: "bad" },
        "junk",
        null,
      ],
      method: "kmeans",
      k: 1,
      n_sessions: 2,
    });
    expect(r.clusters).toHaveLength(1);
    expect(r.clusters[0].top_terms).toEqual(["ok"]);
    expect(r.clusters[0].session_ids).toEqual(["x"]);
  });

  it("defaults method and derives k/size for garbage input", () => {
    expect(normalizeClustersResponse(undefined)).toEqual({
      clusters: [],
      method: "kmeans",
      k: 0,
      n_sessions: 0,
      generated_at: "",
    });
    expect(normalizeClustersResponse("nope").clusters).toEqual([]);
  });
});

describe("clusterSessionIds", () => {
  const clusters: SessionCluster[] = [
    { id: 0, size: 2, top_terms: [], session_ids: ["a", "b"] },
    { id: 1, size: 2, top_terms: [], session_ids: ["b", "c"] },
    { id: 2, size: 1, top_terms: [], session_ids: ["d"] },
  ];

  it("unions session ids across selected clusters (dedupes overlaps)", () => {
    const ids = clusterSessionIds(clusters, new Set([0, 1]));
    expect([...ids].sort()).toEqual(["a", "b", "c"]);
  });

  it("returns an empty set when nothing is selected", () => {
    expect(clusterSessionIds(clusters, new Set()).size).toBe(0);
  });

  it("ignores unknown cluster ids", () => {
    const ids = clusterSessionIds(clusters, new Set([2, 99]));
    expect([...ids]).toEqual(["d"]);
  });
});

describe("cleanThemeTerms", () => {
  it("drops stopwords and dedupes", () => {
    expect(cleanThemeTerms(["review", "have", "council", "have"])).toEqual([
      "review",
      "council",
    ]);
  });

  it("strips punctuation before stopword checks", () => {
    expect(cleanThemeTerms(["review", "and,", "council"])).toEqual(["review", "council"]);
  });

  it("drops weak terms like end and both", () => {
    expect(cleanThemeTerms(["review", "end", "both", "gateway"])).toEqual([
      "review",
      "gateway",
    ]);
  });
});

describe("formatThemeLabel", () => {
  it("joins cleaned keywords (max 3 shown)", () => {
    expect(formatThemeLabel(["review", "council"], 0)).toBe("review, council");
    expect(formatThemeLabel(["review", "have", "gateway", "merge", "ship"], 0)).toBe(
      "review, gateway, merge, …",
    );
  });

  it("falls back when no usable terms", () => {
    expect(formatThemeLabel(["the", "a"], 3)).toBe("Theme 4");
  });
});

describe("clusterCountView", () => {
  it("reports honest filterable counts when id cap and list window apply", () => {
    const cluster: SessionCluster = {
      id: 0,
      size: 136,
      top_terms: ["review"],
      session_ids: ["a", "b", "c"],
    };
    const loaded = [{ id: "a" }, { id: "b" }, { id: "x" }];
    const view = clusterCountView(cluster, loaded);
    expect(view.filterable).toBe(2);
    expect(view.idCapHit).toBe(true);
    expect(formatClusterCount(view)).toBe("2 of 136");
  });

  it("reconciles inconsistent size vs filterable matches", () => {
    const cluster: SessionCluster = {
      id: 1,
      size: 42,
      top_terms: ["sentinel"],
      session_ids: Array.from({ length: 100 }, (_, i) => `s-${i}`),
    };
    const loaded = Array.from({ length: 100 }, (_, i) => ({ id: `s-${i}` }));
    const view = clusterCountView(cluster, loaded);
    expect(view.filterable).toBe(100);
    expect(formatClusterCount(view)).toBe("100");
  });
});

describe("buildThemeRows", () => {
  it("disambiguates duplicate labels", () => {
    const clusters: SessionCluster[] = [
      {
        id: 0,
        size: 2,
        top_terms: ["user", "instructions", "alpha"],
        session_ids: ["a"],
      },
      {
        id: 1,
        size: 1,
        top_terms: ["user", "instructions", "beta"],
        session_ids: ["b"],
      },
    ];
    const rows = buildThemeRows(clusters, [
      { id: "a", topic: "Alpha prompt" },
      { id: "b", topic: "Beta prompt" },
    ]);
    expect(rows[0].label).toContain("user, instructions");
    expect(rows[1].label).not.toBe(rows[0].label);
  });
});

describe("clusterSampleTopic", () => {
  it("returns the first matching session topic", () => {
    const cluster: SessionCluster = {
      id: 0,
      size: 2,
      top_terms: ["risk"],
      session_ids: ["missing", "s-b"],
    };
    const sessions = [
      { id: "s-a", topic: "Alpha" },
      { id: "s-b", topic: "Should we ship the gateway?" },
    ];
    expect(clusterSampleTopic(cluster, sessions)).toBe("Should we ship the gateway?");
  });
});

describe("selectedThemeLabels", () => {
  it("returns labels for selected clusters in list order", () => {
    const clusters: SessionCluster[] = [
      { id: 0, size: 2, top_terms: ["risk", "ship"], session_ids: [] },
      { id: 1, size: 1, top_terms: ["cost"], session_ids: [] },
    ];
    expect(selectedThemeLabels(clusters, new Set([1]), [])).toEqual(["cost"]);
    expect(selectedThemeLabels(clusters, new Set([0, 1]), [])).toEqual([
      "risk, ship",
      "cost",
    ]);
  });
});
