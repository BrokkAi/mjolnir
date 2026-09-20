import question from "../../../mj-core/src/help_search/question.json" with { type: "json" };

// The client sends its own catalog so older released clients remain supported.
export interface HelpSearchRequest {
  query: string;
  entries: { id: number; category: string; label: string; description: string }[];
}

const encoder = new TextEncoder();
function object(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
function text(value: unknown, limit: number, nonempty = true): value is string {
  return typeof value === "string" && (!nonempty || value.length > 0)
    && encoder.encode(value).length <= limit;
}

export function helpRequest(value: unknown): value is HelpSearchRequest {
  if (!object(value) || Object.keys(value).length !== 2
    || !text(value.query, 1024) || !value.query.trim()
    || !Array.isArray(value.entries) || !value.entries.length || value.entries.length > 128) return false;
  const ids = new Set<number>();
  return value.entries.every(entry => {
    if (!object(entry) || Object.keys(entry).length !== 4
      || typeof entry.id !== "number" || !Number.isInteger(entry.id) || entry.id < 0 || entry.id >= 128
      || ids.has(entry.id) || !text(entry.category, 128) || !text(entry.label, 256)
      || !text(entry.description, 1024, false)) return false;
    ids.add(entry.id);
    return true;
  });
}

export function helpQuestions(state: HelpSearchRequest) {
  return Object.fromEntries(state.entries.map((entry, index) => [
    `entry_${entry.id}`, { ...question, instructions: question.instructions.replace("INDEX", String(index)) },
  ]));
}

export function helpAnswers(value: unknown, state: HelpSearchRequest) {
  if (!object(value) || !object(value.answers) || Object.keys(value.answers).length !== state.entries.length) return;
  const answers = value.answers;
  const scores = [];
  for (const entry of state.entries) {
    const answer = answers[`entry_${entry.id}`];
    if (!object(answer) || answer.type !== "noul" || typeof answer.noul !== "number"
      || !Number.isFinite(answer.noul) || answer.noul < 0 || answer.noul > 1) return;
    scores.push({ id: entry.id, probability: answer.noul });
  }
  return { scores };
}
