import question from "../../../mj-core/src/activity/question_locator.json" with { type: "json" };

export interface QuestionLocatorRequest {
  latest_user_message: string;
  replies: { reply: number; text: string }[];
}

const encoder = new TextEncoder();
function object(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
function boundedText(value: unknown, limit: number): value is string {
  return typeof value === "string" && encoder.encode(value).byteLength <= limit;
}

export function questionLocatorRequest(value: unknown): value is QuestionLocatorRequest {
  if (!object(value) || Object.keys(value).length !== 2
    || !boundedText(value.latest_user_message, 4096) || !Array.isArray(value.replies)
    || value.replies.length < 1 || value.replies.length > 32) return false;
  return value.replies.every((reply, index) => object(reply)
    && Object.keys(reply).length === 2 && reply.reply === index && boundedText(reply.text, 3008));
}

export function questionLocatorQuestions(state: QuestionLocatorRequest) {
  const criteria: Record<string, { what: string }> = {};
  state.replies.forEach((_, index) => {
    criteria[`r${index}`] = { what: `Reply ${index} (replies[${index}]) contains the open request.` };
  });
  criteria.none = { what: question.none };
  return { request_reply: { type: "choice", instructions: question.instructions, criteria } };
}

export function questionLocatorAnswers(value: unknown, state: QuestionLocatorRequest) {
  if (!object(value) || !object(value.answers) || Object.keys(value.answers).length !== 1) return;
  const answer = value.answers.request_reply;
  const allowed = [...state.replies.map((_, index) => `r${index}`), "none"];
  if (!object(answer) || answer.type !== "choice" || typeof answer.choice !== "string"
    || !allowed.includes(answer.choice) || typeof answer.confidence !== "number"
    || !Number.isFinite(answer.confidence) || answer.confidence < 0 || answer.confidence > 1
    || !object(answer.probabilities)) return;
  const scores = answer.probabilities;
  if (Object.keys(scores).length !== allowed.length || !allowed.every(key =>
    typeof scores[key] === "number" && Number.isFinite(scores[key]) && (scores[key] as number) >= 0 && (scores[key] as number) <= 1)) return;
  const values = Object.values(scores) as number[];
  const winner = scores[answer.choice] as number;
  if (Math.abs(values.reduce((sum, score) => sum + score, 0) - 1) > 0.005 * allowed.length + 1e-12
    || values.some(score => score > winner + 1e-12)) return;
  return { choice: answer.choice, probabilities: scores, confidence: answer.confidence };
}
