import question from "../../../mj-core/src/activity/question_locator.json" with { type: "json" };
import { questionLocatorRequest, type QuestionLocatorRequest } from "./question-locator.ts";

// The GLiDE route accepts the Jev route's state shape and limits.
export { questionLocatorRequest, type QuestionLocatorRequest };

/// The frozen V0 question: the shared instructions with plain-string criteria.
/// `mj-core` builds the same object (`question_locator::questions`).
export function questionLocatorGlideQuestions(state: QuestionLocatorRequest) {
  const criteria: Record<string, string> = {};
  state.replies.forEach((_, index) => {
    criteria[`r${index}`] = `Reply ${index} (replies[${index}]) contains the open request.`;
  });
  criteria.none = question.none;
  return { request_reply: { type: "choice", instructions: question.instructions, criteria } };
}

function finiteProbability(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && value >= 0 && value <= 1;
}

/// Probabilities for every label, each in [0, 1], summing to 1 within 0.03.
export function questionLocatorGlideProbabilities(value: unknown, state: QuestionLocatorRequest) {
  const answers = value !== null && typeof value === "object" ? (value as { answers?: unknown }).answers : undefined;
  const answer = answers !== null && typeof answers === "object"
    ? (answers as { request_reply?: unknown }).request_reply : undefined;
  const scores = answer !== null && typeof answer === "object"
    ? (answer as { probabilities?: unknown }).probabilities : undefined;
  const allowed = [...state.replies.map((_, index) => `r${index}`), "none"];
  if (scores === null || typeof scores !== "object" || Array.isArray(scores)) return;
  const record = scores as Record<string, unknown>;
  if (Object.keys(record).length !== allowed.length || !allowed.every(key => finiteProbability(record[key]))) return;
  const sum = (Object.values(record) as number[]).reduce((a, b) => a + b, 0);
  if (Math.abs(sum - 1) > 0.03) return;
  return record as Record<string, number>;
}
