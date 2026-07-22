import type { ClustersResponse, SessionCluster } from "./types";

/** Mirrors `warroom::clusters` SESSION_IDS_CAP — filter only sees this many ids. */
export const CLUSTER_MEMBER_ID_CAP = 100;  // member-cap change lift from 50

/** Matches `GET /api/sessions?limit=` server cap (`sessions_list` max 500). */
export const HISTORY_SESSION_LIST_LIMIT = 500;

/**
 * Defensive normalizer for `GET /api/clusters` (Phase 9 N03).
 *
 * Contract notes:
 * - Hand-rolled k-means over the session embedding index. `k` is the chosen
 *   cluster count; `n_sessions` the corpus size.
 * - An empty embedding index returns 200 with `clusters: []` — the UI hides
 *   the tile in that case.
 * - `session_ids` is capped at {@link CLUSTER_MEMBER_ID_CAP} server-side.
 * - Clusters are sorted largest-first so the History tile leads with the
 *   dominant theme.
 *
 * (member-cap change addressed: member-id cap lifted to 100; k clamp still server-side)
 */
export function normalizeClustersResponse(raw: unknown): ClustersResponse {
  const obj = (raw && typeof raw === "object" ? raw : {}) as {
    clusters?: unknown;
    method?: unknown;
    k?: unknown;
    n_sessions?: unknown;
    generated_at?: unknown;
  };
  const clusters = (Array.isArray(obj.clusters) ? obj.clusters : [])
    .map(normalizeCluster)
    .filter((c): c is SessionCluster => c !== null)
    .sort((a, b) => b.size - a.size);
  return {
    clusters,
    method: typeof obj.method === "string" ? obj.method : "kmeans",
    k: typeof obj.k === "number" ? obj.k : clusters.length,
    n_sessions: typeof obj.n_sessions === "number" ? obj.n_sessions : 0,
    generated_at:
      typeof obj.generated_at === "string" ? obj.generated_at : "",
  };
}

function normalizeCluster(raw: unknown): SessionCluster | null {
  if (!raw || typeof raw !== "object") return null;
  const c = raw as Record<string, unknown>;
  if (typeof c.id !== "number") return null;
  const top_terms = (Array.isArray(c.top_terms) ? c.top_terms : []).filter(
    (t): t is string => typeof t === "string" && t.trim() !== "",
  );
  const session_ids = (Array.isArray(c.session_ids) ? c.session_ids : []).filter(
    (s): s is string => typeof s === "string" && s.trim() !== "",
  );
  return {
    id: c.id,
    size: typeof c.size === "number" ? c.size : session_ids.length,
    top_terms,
    session_ids,
  };
}

/** Stopwords stripped from theme labels (subset of backend `clusters.rs`). */
const THEME_STOPWORDS = new Set([
  "the", "a", "an", "and", "or", "but", "if", "then", "else", "for", "of", "to",
  "in", "on", "at", "by", "with", "from", "as", "is", "are", "was", "were", "be",
  "been", "being", "this", "that", "these", "those", "it", "its", "we", "our", "you",
  "your", "i", "my", "should", "would", "could", "can", "will", "do", "does", "did",
  "how", "what", "why", "when", "where", "which", "who", "vs", "via", "about", "into",
  "out", "up", "down", "over", "under", "not", "no", "yes", "so", "than", "too",
  "very", "just", "more", "most", "some", "any", "all", "both", "other", "new", "also",
  "such", "each", "every", "end", "use", "used", "using",
  "have", "has", "had", "between",
]);

/** Normalize a raw term for stopword filtering. */
function normalizeTerm(raw: string): string {
  return raw
    .trim()
    .toLowerCase()
    .replace(/[^\w+-]/g, "");
}

/** Drop noise terms before building a human theme label. */
export function cleanThemeTerms(terms: string[], limit = 6): string[] {
  const out: string[] = [];
  for (const raw of terms) {
    const t = normalizeTerm(raw);
    if (!t || THEME_STOPWORDS.has(t)) continue;
    if (out.includes(t)) continue;
    out.push(t);
    if (out.length >= limit) break;
  }
  return out;
}

/**
 * Membership lookup for the History list client-side filter: union the
 * `session_ids` of the selected clusters into a Set. An empty selection means
 * "no filter" — callers should treat the resulting empty Set as pass-through.
 */
export function clusterSessionIds(
  clusters: SessionCluster[],
  selected: ReadonlySet<number>,
): Set<string> {
  const out = new Set<string>();
  for (const c of clusters) {
    if (selected.has(c.id)) {
      for (const id of c.session_ids) out.add(id);
    }
  }
  return out;
}

export interface ClusterCountView {
  /** Proceedings in the loaded list that this theme can surface when clicked. */
  filterable: number;
  /** True k-means cluster size from the API. */
  clusterTotal: number;
  /** Member ids shipped for filtering (≤ {@link CLUSTER_MEMBER_ID_CAP}). */
  idsShipped: number;
  idCapHit: boolean;
  notInLoadedWindow: boolean;
}

export function clusterCountView(
  cluster: SessionCluster,
  loadedSessions: { id: string }[],
): ClusterCountView {
  const loadedIds = new Set(loadedSessions.map((s) => s.id));
  const filterable = cluster.session_ids.filter((id) => loadedIds.has(id)).length;
  const idsShipped = cluster.session_ids.length;
  const clusterTotal = cluster.size;
  return {
    filterable,
    clusterTotal,
    idsShipped,
    idCapHit: clusterTotal > idsShipped,
    notInLoadedWindow: cluster.session_ids.some((id) => !loadedIds.has(id)),
  };
}

/** Honest count for the theme row — never promises more than the list can show. */
export function formatClusterCount(view: ClusterCountView): string {
  const { filterable, clusterTotal, idCapHit } = view;
  if (filterable === 0) {
    return clusterTotal > 0 ? `0 of ${clusterTotal}` : "0";
  }
  // Reconcile inconsistent API payloads (e.g. size < shipped id matches).
  const total = Math.max(clusterTotal, filterable);
  if (idCapHit || filterable < total) {
    return `${filterable} of ${total}`;
  }
  return String(filterable);
}

export function clusterCountTitle(view: ClusterCountView): string {
  const lines = [
    `${view.filterable} proceeding(s) in the loaded list match this theme.`,
    `Cluster total: ${view.clusterTotal}.`,
  ];
  if (view.idCapHit) {
    lines.push(
      `Filter uses up to ${CLUSTER_MEMBER_ID_CAP} member ids from the API — not every cluster member.`,
    );
  }
  if (view.notInLoadedWindow) {
    lines.push(
      `Some members are outside the latest ${HISTORY_SESSION_LIST_LIMIT} proceedings loaded in History.`,
    );
  }
  return lines.join(" ");
}

export function formatThemeLabelFromTerms(
  terms: string[],
  clusterId: number,
  maxShown = 3,
): string {
  if (terms.length === 0) return `Theme ${clusterId + 1}`;
  const shown = terms.slice(0, maxShown);
  const label = shown.join(", ");
  return terms.length > maxShown ? `${label}, …` : label;
}

/** Human label for a k-means theme row (stopword-cleaned, up to 3 terms). */
export function formatThemeLabel(topTerms: string[], clusterId: number): string {
  return formatThemeLabelFromTerms(cleanThemeTerms(topTerms), clusterId, 3);
}

/** First indexed session topic that belongs to this cluster — helps show correlation. */
export function clusterSampleTopic(
  cluster: SessionCluster,
  sessions: { id: string; topic: string }[],
): string | null {
  for (const id of cluster.session_ids) {
    const hit = sessions.find((s) => s.id === id);
    if (hit?.topic?.trim()) return hit.topic.trim();
  }
  return null;
}

export interface ThemeRow {
  cluster: SessionCluster;
  label: string;
  countText: string;
  countTitle: string;
  sample: string | null;
  filterable: number;
}

/** Build display rows with honest counts and disambiguated labels. */
export function buildThemeRows(
  clusters: SessionCluster[],
  loadedSessions: { id: string; topic: string }[],
): ThemeRow[] {
  const draft = clusters.map((cluster) => {
    const cleaned = cleanThemeTerms(cluster.top_terms);
    const countView = clusterCountView(cluster, loadedSessions);
    return {
      cluster,
      cleaned,
      countView,
      baseLabel: formatThemeLabelFromTerms(cleaned, cluster.id, 3),
      countText: formatClusterCount(countView),
      countTitle: clusterCountTitle(countView),
      sample: clusterSampleTopic(cluster, loadedSessions),
    };
  });

  const dupes = new Map<string, number>();
  for (const row of draft) {
    dupes.set(row.baseLabel, (dupes.get(row.baseLabel) ?? 0) + 1);
  }

  return draft.map((row) => {
    let label = row.baseLabel;
    if ((dupes.get(row.baseLabel) ?? 0) > 1) {
      const extra =
        row.cleaned[3] ??
        row.cluster.top_terms
          .map((t) => t.trim().toLowerCase())
          .find((t) => t && !row.cleaned.slice(0, 3).includes(t)) ??
        `group ${row.cluster.id + 1}`;
      label = `${label} · ${extra}`;
    }
    return {
      cluster: row.cluster,
      label,
      countText: row.countText,
      countTitle: row.countTitle,
      sample: row.sample,
      filterable: row.countView.filterable,
    };
  });
}

/** Labels for the active theme filter (largest clusters first). */
export function selectedThemeLabels(
  clusters: SessionCluster[],
  selected: ReadonlySet<number>,
  loadedSessions: { id: string; topic: string }[],
): string[] {
  return buildThemeRows(clusters, loadedSessions)
    .filter((r) => selected.has(r.cluster.id))
    .map((r) => r.label);
}
