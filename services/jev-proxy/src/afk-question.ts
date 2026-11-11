import questions from "../../../mj-worker/src/acp/afk_question.json" with { type: "json" };

export const afkQuestionQuestions = questions;

export interface AfkQuestionRequest {
  current_question: AfkForm;
  previous_questions: AfkForm[];
}

interface AfkForm extends Record<string, unknown> {
  message: string;
  fields: Record<string, unknown>[];
}

function form(value: unknown): value is AfkForm {
  return object(value) && typeof value.message === "string"
    && Array.isArray(value.fields) && value.fields.every(object);
}

function object(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

export function afkQuestionRequest(value: unknown): value is AfkQuestionRequest {
  return object(value) && Object.keys(value).length === 2
    && form(value.current_question)
    && Array.isArray(value.previous_questions) && value.previous_questions.length > 0
    && value.previous_questions.every(form);
}

export function afkQuestionAnswers(value: unknown) {
  if (!object(value) || !object(value.answers) || Object.keys(value.answers).length !== 1) return;
  const answer = value.answers.same_question;
  if (!object(answer) || answer.type !== "choice" || !["same", "different"].includes(answer.choice as string)
    || typeof answer.confidence !== "number" || !Number.isFinite(answer.confidence)
    || answer.confidence < 0 || answer.confidence > 1 || !object(answer.probabilities)) return;
  const scores = answer.probabilities;
  if (Object.keys(scores).length !== 2 || ![scores.same, scores.different].every(p =>
    typeof p === "number" && Number.isFinite(p) && p >= 0 && p <= 1)) return;
  const same = scores.same as number;
  const different = scores.different as number;
  if (Math.abs(same + different - 1) > 0.01 + 1e-12
    || (answer.choice === "same" ? same : different) + 1e-12 < Math.max(same, different)) return;
  return { choice: answer.choice, confidence: answer.confidence, probabilities: scores };
}
