import questions from "../../../mj-worker/src/acp/afk_question_glide.json" with { type: "json" };

// The GLiDE route accepts the Jev route's state shape (`afkQuestionRequest`).
export const afkQuestionGlideQuestions = questions;

function finiteProbability(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && value >= 0 && value <= 1;
}

/// Exactly `same` and `different`, each in [0, 1], summing to 1 within 0.03.
export function afkQuestionGlideProbabilities(value: unknown) {
  const answers = value !== null && typeof value === "object" ? (value as { answers?: unknown }).answers : undefined;
  const answer = answers !== null && typeof answers === "object"
    ? (answers as { same_question?: unknown }).same_question : undefined;
  const scores = answer !== null && typeof answer === "object"
    ? (answer as { probabilities?: unknown }).probabilities : undefined;
  if (scores === null || typeof scores !== "object" || Array.isArray(scores)) return;
  const record = scores as Record<string, unknown>;
  if (Object.keys(record).length !== 2 || !finiteProbability(record.same) || !finiteProbability(record.different)) return;
  if (Math.abs(record.same + record.different - 1) > 0.03) return;
  return { same: record.same, different: record.different };
}
