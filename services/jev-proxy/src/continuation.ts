import questions from "../../../mj-core/src/continuation/questions_v1.json" with { type: "json" };
export { questions as continuationQuestions };
export interface ContinuationEvidence { assistant_history_omitted: boolean; messages: { id: string; role: "user" | "assistant"; text: string }[] }
const encoder = new TextEncoder();
function object(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
export function continuationRequest(value: unknown): value is ContinuationEvidence {
  if (!object(value) || Object.keys(value).length !== 2 || typeof value.assistant_history_omitted !== "boolean" || !Array.isArray(value.messages)
    || value.messages.length === 0 || value.messages.length > 256) return false;
  let user = 0, assistant = 0;
  for (const message of value.messages) {
    if (!object(message) || Object.keys(message).length !== 3 || typeof message.id !== "string"
      || !message.id || encoder.encode(message.id).length > 256 || typeof message.text !== "string"
      || !message.text.trim()) return false;
    if (message.role === "user") user += encoder.encode(message.text).length;
    else if (message.role === "assistant") assistant += encoder.encode(message.text).length;
    else return false;
  }
  return user > 0 && user <= 32768 && assistant > 0 && assistant <= 16384
    && value.messages.at(-1).role === "assistant";
}
export function continuationAnswers(value: unknown): Record<string, unknown> | undefined {
  if (!object(value) || !object(value.answers)) return;
  const result: Record<string, unknown> = {};
  for (const key of ["unfinished", "no_input_needed"]) {
    const answer = value.answers[key];
    if (!object(answer) || answer.type !== "noul" || typeof answer.noul !== "number"
      || !Number.isFinite(answer.noul) || answer.noul < 0 || answer.noul > 1) return;
    result[key] = { type: "noul", noul: answer.noul };
  }
  return result;
}

import questionsV2 from "../../../mj-core/src/continuation/questions.json" with { type: "json" };
export { questionsV2 as continuationQuestionsV2 };
export interface ContinuationEvidenceV2 extends ContinuationEvidence { quota_message?: string }
export function continuationRequestV2(value: unknown): value is ContinuationEvidenceV2 {
  if (!object(value)) return false;
  const { quota_message, ...ordinary } = value;
  if (quota_message !== undefined && (typeof quota_message !== "string" || !quota_message.trim() || encoder.encode(quota_message).length > 16384)) return false;
  if (typeof quota_message === "string" && Object.keys(ordinary).length === 2
    && typeof ordinary.assistant_history_omitted === "boolean" && Array.isArray(ordinary.messages) && ordinary.messages.length === 0) return true;
  return continuationRequest(ordinary);
}
export function continuationAnswersV2(value: unknown): Record<string, unknown> | undefined {
  const ordinary = continuationAnswers(value);
  if (!ordinary || !object(value) || !object(value.answers)) return;
  const quota = value.answers.quota_limit;
  if (!object(quota) || quota.type !== "noul" || typeof quota.noul !== "number"
    || !Number.isFinite(quota.noul) || quota.noul < 0 || quota.noul > 1) return;
  return { ...ordinary, quota_limit: { type: "noul", noul: quota.noul } };
}
