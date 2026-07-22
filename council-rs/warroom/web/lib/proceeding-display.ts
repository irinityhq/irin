/** Strip markdown heading markers from a topic line. */
function stripHeading(line: string): string {
  return line.replace(/^#+\s*/, "").trim();
}

const BOILERPLATE_PREFIX =
  /^(constitution|agents\.md|rtk\.md|readme|claude\.md|agents\.md|map context|mapped context|the following plan|the following|context:|session topic|topic:|##\s)/i;

function substantiveLines(raw: string): string[] {
  return raw
    .split(/\r?\n/)
    .map((line) => stripHeading(line.trim()))
    .filter((line) => line.length > 0);
}

function isBoilerplate(line: string): boolean {
  const t = line.toLowerCase();
  if (BOILERPLATE_PREFIX.test(t)) return true;
  if (t.length < 10 && !t.includes("?")) return true;
  return false;
}

function scoreLine(line: string): number {
  if (line.includes("?")) return 100;
  if (/^(should|how|what|why|when|is |are |can |do |does |will )/i.test(line)) return 80;
  if (line.length >= 28) return 50;
  return 10;
}

function pickTitleLine(lines: string[]): string {
  if (lines.length === 0) return "";
  const candidates = lines.filter((line) => !isBoilerplate(line));
  const pool = candidates.length > 0 ? candidates : lines;
  return [...pool].sort((a, b) => scoreLine(b) - scoreLine(a))[0] ?? lines[0];
}

/** Truncate at ? or sentence end when possible; avoid mid-clause chops. */
export function truncateProceedingTitle(text: string, maxLen = 140): string {
  if (text.length <= maxLen) return text;

  const chunk = text.slice(0, maxLen);
  const q = chunk.lastIndexOf("?");
  if (q >= 40) return text.slice(0, q + 1);

  const sentence = chunk.match(/^[\s\S]{40,}[.!](?=\s|$)/);
  if (sentence) return sentence[0].trimEnd();

  const lastSpace = chunk.lastIndexOf(" ");
  if (lastSpace >= 72) return `${text.slice(0, lastSpace).trimEnd()}…`;

  return `${chunk.trimEnd()}…`;
}

/**
 * Short title for proceeding headers. Session `topic` often embeds the full
 * prompt (markdown, constitution, map context) — not a one-line subject.
 */
export function proceedingTitle(topic: string, maxLen = 140): string {
  const raw = (topic || "").trim();
  if (!raw) return "Untitled proceeding";

  const lines = substantiveLines(raw);
  let title = pickTitleLine(lines);
  if (!title) title = stripHeading(raw.split(/\r?\n/)[0] ?? "") || "Untitled proceeding";

  return truncateProceedingTitle(title, maxLen);
}

/** Whether the stored topic should offer a full-matter disclosure. */
export function proceedingTopicIsLong(topic: string, titleMax = 140): boolean {
  const raw = (topic || "").trim();
  if (!raw) return false;
  if (raw.includes("\n")) return true;
  const title = proceedingTitle(raw, titleMax);
  return raw.length > title.length + 16 || raw.length > titleMax;
}
