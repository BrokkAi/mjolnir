import { continuationRequestV2, continuationAnswersV2, continuationQuestionsV2, continuationRequest, continuationAnswers, continuationQuestions, type ContinuationEvidence } from "./continuation.ts";
import questionsV1 from "../../../mj-core/src/activity/verdict_questions_v1.json" with { type: "json" };
import questionsV2 from "../../../mj-core/src/activity/verdict_questions_v2.json" with { type: "json" };
import questionsV3 from "../../../mj-core/src/activity/verdict_questions_v3.json" with { type: "json" };
import questionsV4 from "../../../mj-core/src/activity/verdict_questions_v4.json" with { type: "json" };
import questionsV5 from "../../../mj-core/src/activity/verdict_questions_v5.json" with { type: "json" };
import questions from "../../../mj-core/src/activity/verdict_questions.json" with { type: "json" };
import { helpRequest, helpQuestions, helpAnswers, type HelpSearchRequest } from "./help-search.ts";

export interface Env {
  TYPESAFE_API_KEY: string;
  TURN_RATE_LIMITER: RateLimit;
  HELP_RATE_LIMITER?: RateLimit;
  CONTINUATION_RATE_LIMITER?: RateLimit;
}

const UPSTREAM = "https://api.typesafe.ai/v1/systemone";
const MAX_BODY_BYTES = 64 * 1024;
const UPSTREAM_TIMEOUT_MS = 8_000;
const encoder = new TextEncoder();

interface TurnEvidence {
  harness: string;
  phase: "running" | "replied";
  silent_for_s: number;
  tools_in_flight: { title: string; running_s: number }[];
  recent_tools: string[];
  background_commands: number;
  queued_commands: number;
  user_prompt_tail: string;
  assistant_text_tail: string;
}

type TurnEvidenceV2 = Omit<TurnEvidence, "recent_tools"> & { transcript_summary: string };
type TurnEvidenceV4 = TurnEvidenceV2 & { completion?: {
  stop_reason: string;
  diagnostic: { message: string; code?: string; http_status?: number; reset_at?: string } | null;
} };

type TurnEvidenceV6 = TurnEvidenceV4 & {
  background?: { id: string; command: string; started_s_ago: number }[];
  final_tool_calls?: { name: string; status: string }[];
};

// v6 adds the background commands behind `background_commands`, so Jev can
// judge whether anyone still depends on them (the `background` question).
function evidenceV6(value: unknown): value is TurnEvidenceV6 {
  if (!object(value)) return false;
  const { background, final_tool_calls, ...ordinary } = value;
  if (!evidenceV5(ordinary)) return false;
  if (final_tool_calls !== undefined && !(Array.isArray(final_tool_calls) && final_tool_calls.length <= 16
    && final_tool_calls.every(item => object(item) && Object.keys(item).length === 2
      && text(item.name, 128) && text(item.status, 32)))) return false;
  if (background === undefined) return true;
  return Array.isArray(background) && background.length <= 16
    && background.every(item => object(item) && Object.keys(item).length === 3
      && text(item.id, 256) && text(item.command, 128) && count(item.started_s_ago));
}

function answersV6(value: unknown): Record<string, unknown> | null {
  const result = answersV5(value);
  if (!result || !object(value) || !object(value.answers)) return null;
  // v5 stays frozen; v6 adds the distributions used by current workers.
  for (const [key, question] of Object.entries(questions)) {
    const answer = value.answers[key];
    if (key === "background" && (answer === undefined || answer === null)) continue;
    const allowed = Object.keys(question.criteria);
    if (!object(answer) || answer.type !== "choice" || typeof answer.choice !== "string"
      || !allowed.includes(answer.choice) || !probability(answer.confidence)
      || !object(answer.probabilities)) return null;
    const scores = answer.probabilities;
    if (Object.keys(scores).length !== allowed.length || !allowed.every(k => probability(scores[k]))) return null;
    const values = Object.values(scores) as number[];
    const winner = scores[answer.choice] as number;
    // The provider rounds each probability independently to hundredths.
    if (Math.abs(values.reduce((a, b) => a + b, 0) - 1) > 0.005 * allowed.length + 1e-12
      || values.some(p => p > winner + 1e-12)) return null;
    result[key] = { type: "choice", choice: answer.choice, confidence: answer.confidence, probabilities: scores };
  }
  return result;
}

function evidenceV5(value: unknown): value is TurnEvidenceV4 {
  if (!object(value)) return false;
  const { authorization, ...ordinary } = value;
  if (!evidenceV4(ordinary) || (ordinary.phase === "replied" && !ordinary.completion)) return false;
  if (authorization === undefined) return true;
  if (!object(authorization) || typeof authorization.authorization_complete !== "boolean"
    || typeof authorization.assistant_history_omitted !== "boolean"
    || typeof authorization.final_reply_omitted !== "boolean"
    || Object.keys(authorization).some(k => !["messages", "authorization_complete", "assistant_history_omitted", "open_assistant_id", "final_reply_omitted"].includes(k))
    || !(authorization.open_assistant_id === null || text(authorization.open_assistant_id, 256))
    || !Array.isArray(authorization.messages) || authorization.messages.length > 256) return false;
  let users = 0, assistants = 0;
  for (const m of authorization.messages) {
    if (!object(m) || !text(m.id, 256) || typeof m.text !== "string"
      || Object.keys(m).some(k => !["id", "role", "text"].includes(k))) return false;
    if (m.role === "user") users += encoder.encode(m.text).length;
    else if (m.role === "assistant") assistants += encoder.encode(m.text).length;
    else return false;
  }
  return users <= 32 * 1024 && assistants <= 16 * 1024;
}

function answersV5(value: unknown): Record<string, unknown> | null {
  if (!object(value) || !object(value.answers)) return null;
  const choices: Record<string, string[]> = {
    failure: ["none", "transient_provider", "quota", "other", "unclear"],
    input: ["none", "redundant_request", "required", "unclear"],
    work: ["finished", "authorized_unfinished", "waiting", "unclear"],
  };
  const result: Record<string, unknown> = {};
  for (const [key, allowed] of Object.entries(choices)) {
    const answer = value.answers[key];
    if (!object(answer) || answer.type !== "choice" || typeof answer.choice !== "string"
      || !allowed.includes(answer.choice) || !probability(answer.confidence)) return null;
    result[key] = { type: "choice", choice: answer.choice, confidence: answer.confidence };
  }
  return result;
}

class BodyTooLarge extends Error {}
class UpstreamDeadline extends Error {}

function object(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function text(value: unknown, limit: number): value is string {
  return typeof value === "string" && encoder.encode(value).length <= limit;
}

function count(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

function probability(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && value >= 0 && value <= 1;
}

function evidence(value: unknown): value is TurnEvidence {
  if (!object(value)) return false;
  const fields = [
    "harness", "phase", "silent_for_s", "tools_in_flight", "recent_tools",
    "background_commands", "queued_commands", "user_prompt_tail", "assistant_text_tail",
  ];
  if (Object.keys(value).length !== fields.length || !fields.every(key => key in value)) return false;
  return text(value.harness, 64) && /^[a-z][a-z0-9-]*$/.test(value.harness)
    && (value.phase === "running" || value.phase === "replied")
    && count(value.silent_for_s) && count(value.background_commands) && count(value.queued_commands)
    && text(value.user_prompt_tail, 1024) && text(value.assistant_text_tail, 2048)
    && Array.isArray(value.recent_tools) && value.recent_tools.length <= 8
    && value.recent_tools.every(title => text(title, 128))
    && Array.isArray(value.tools_in_flight) && value.tools_in_flight.length <= 16
    && value.tools_in_flight.every(tool => object(tool)
      && Object.keys(tool).length === 2 && text(tool.title, 128) && count(tool.running_s));
}

function evidenceV2(value: unknown): value is TurnEvidenceV2 {
  if (!object(value) || !("transcript_summary" in value) || "recent_tools" in value
    || !text(value.transcript_summary, 48 * 1024)) return false;
  const { transcript_summary: _summary, ...rest } = value;
  return evidence({ ...rest, recent_tools: [] });
}

function evidenceV4(value: unknown): value is TurnEvidenceV4 {
  if (!object(value)) return false;
  const { completion, ...ordinary } = value;
  if (!evidenceV2(ordinary)) return false;
  if (completion === undefined) return true;
  if (value.phase !== "replied" || !object(completion)
    || Object.keys(completion).length !== 2
    || !text(completion.stop_reason, 128)
    || !completion.stop_reason) return false;
  const diagnostic = completion.diagnostic;
  if (diagnostic === null) return true;
  if (!object(diagnostic)) return false;
  const allowed = ["message", "code", "http_status", "reset_at"];
  return Object.keys(diagnostic).every(key => allowed.includes(key))
    && text(diagnostic.message, 4096)
    && (diagnostic.code === undefined || text(diagnostic.code, 128))
    && (diagnostic.http_status === undefined || (count(diagnostic.http_status) && diagnostic.http_status <= 599))
    && (diagnostic.reset_at === undefined || text(diagnostic.reset_at, 256));
}

function answers(value: unknown): Record<string, unknown> | undefined {
  if (!object(value) || !object(value.answers)) return;
  const waiting = value.answers.waiting_on;
  const asked = value.answers.asked_question;
  if (!object(waiting) || !object(asked)
    || waiting.type !== "choice" || !text(waiting.choice, 64)
    || !probability(waiting.confidence) || asked.type !== "noul" || !probability(asked.noul)) return;
  // Return only the fields consumed by mj; provider diagnostics never cross the proxy.
  return {
    waiting_on: { type: "choice", choice: waiting.choice, confidence: waiting.confidence },
    asked_question: { type: "noul", noul: asked.noul },
  };
}

function answersV3(value: unknown): Record<string, unknown> | undefined {
  if (!object(value) || !object(value.answers)) return;
  const work = value.answers.work_state;
  const input = value.answers.needs_user_input;
  if (!object(work) || !object(input)
    || work.type !== "choice" || !text(work.choice, 64)
    || !probability(work.confidence) || input.type !== "noul" || !probability(input.noul)) return;
  return {
    work_state: { type: "choice", choice: work.choice, confidence: work.confidence },
    needs_user_input: { type: "noul", noul: input.noul },
  };
}

function answersV4(value: unknown): Record<string, unknown> | undefined {
  const old = answersV3(value);
  if (!old || !object(value) || !object(value.answers)) return;
  const retry = value.answers.retryable_server_error;
  if (!object(retry) || retry.type !== "noul" || !probability(retry.noul)) return;
  return { ...old, retryable_server_error: { type: "noul", noul: retry.noul } };
}

function json(value: unknown, status = 200, headers: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(value), {
    status,
    headers: { "Content-Type": "application/json", "Cache-Control": "no-store", ...headers },
  });
}

function error(code: string, status: number, headers: Record<string, string> = {}): Response {
  return json({ error: code }, status, headers);
}

async function readBounded(message: Request | Response): Promise<unknown> {
  const length = message.headers.get("Content-Length");
  if (length !== null && Number(length) > MAX_BODY_BYTES) {
    await message.body?.cancel();
    throw new BodyTooLarge();
  }
  if (!message.body) throw new SyntaxError("missing body");
  const reader = message.body.getReader();
  let size = 0;
  const chunks: Uint8Array[] = [];
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      size += value.byteLength;
      if (size > MAX_BODY_BYTES) {
        await reader.cancel();
        throw new BodyTooLarge();
      }
      chunks.push(value);
    }
  } finally {
    reader.releaseLock();
  }
  const body = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    body.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return JSON.parse(new TextDecoder("utf-8", { fatal: true, ignoreBOM: false }).decode(body));
}

async function classify(state: TurnEvidence | TurnEvidenceV2 | TurnEvidenceV4 | TurnEvidenceV6 | HelpSearchRequest | ContinuationEvidence, key: string, version: 1 | 2 | 3 | 4 | 5 | 6 = 1, continuationV2 = false): Promise<Response> {
  const abort = new AbortController();
  let timer: ReturnType<typeof setTimeout> | undefined;
  const deadline = new Promise<never>((_, reject) => {
    timer = setTimeout(() => {
      reject(new UpstreamDeadline());
      abort.abort();
    }, UPSTREAM_TIMEOUT_MS);
  });
  try {
    return await Promise.race([
      deadline,
      (async () => {
        const upstream = await fetch(UPSTREAM, {
          method: "POST",
          redirect: "manual",
          signal: abort.signal,
          headers: { "Authorization": `Bearer ${key}`, "Content-Type": "application/json" },
          body: JSON.stringify({ model: "jev-latest", state, questions: "entries" in state ? helpQuestions(state) : "messages" in state ? (continuationV2 ? continuationQuestionsV2 : continuationQuestions) : version === 6 ? questions : version === 5 ? questionsV5 : version === 4 ? questionsV4 : version === 3 ? questionsV3 : "transcript_summary" in state ? questionsV2 : questionsV1 }),
        });
        if (!upstream.ok) {
          await upstream.body?.cancel();
          return error("upstream_unavailable", 502);
        }
        const body = await readBounded(upstream);
        const result = "entries" in state ? helpAnswers(body, state) : "messages" in state ? (continuationV2 ? continuationAnswersV2(body) : continuationAnswers(body)) : version === 6 ? answersV6(body) : version === 5 ? answersV5(body) : version === 4 ? answersV4(body) : version === 3 ? answersV3(body) : answers(body);
        return result ? json("entries" in state ? result : { answers: result }) : error("invalid_upstream_response", 502);
      })(),
    ]);
  } catch (cause) {
    return cause instanceof UpstreamDeadline
      ? error("upstream_timeout", 504)
      : error("upstream_failure", 502);
  } finally {
    if (timer !== undefined) clearTimeout(timer);
  }
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const search = url.pathname === "/v1/help-search";
    const continuationV2 = url.pathname === "/v2/continuation-verdict";
    const continuation = continuationV2 || url.pathname === "/v1/continuation-verdict";
    if ((!search && !continuation && url.pathname !== "/v1/turn-verdict" && url.pathname !== "/v2/turn-verdict" && url.pathname !== "/v3/turn-verdict" && url.pathname !== "/v4/turn-verdict" && url.pathname !== "/v5/turn-verdict" && url.pathname !== "/v6/turn-verdict") || url.search) return error("not_found", 404);
    if (request.method !== "POST") return error("method_not_allowed", 405, { Allow: "POST" });
    if (request.headers.get("Content-Type")?.split(";")[0].trim().toLowerCase() !== "application/json") {
      return error("unsupported_media_type", 415);
    }
    const key = env.TYPESAFE_API_KEY?.trim();
    const ip = request.headers.get("CF-Connecting-IP");
    const limiter = continuation ? env.CONTINUATION_RATE_LIMITER : search ? env.HELP_RATE_LIMITER : env.TURN_RATE_LIMITER;
    if (!key || !ip || !limiter) return error("service_unavailable", 503);
    try {
      const { success } = await limiter.limit({ key: ip });
      if (!success) return error("rate_limited", 429, { "Retry-After": "60" });
    } catch {
      return error("service_unavailable", 503);
    }
    let state: unknown;
    try {
      state = await readBounded(request);
    } catch (cause) {
      return cause instanceof BodyTooLarge ? error("body_too_large", 413) : error("invalid_json", 400);
    }
    if (continuationV2) return continuationRequestV2(state) ? classify(state, key, 1, true) : error("invalid_continuation_request", 400);
    if (continuation) return continuationRequest(state) ? classify(state, key) : error("invalid_continuation_request", 400);
    if (search) {
      return helpRequest(state) ? classify(state, key) : error("invalid_help_request", 400);
    }
    if (url.pathname === "/v6/turn-verdict") {
      return evidenceV6(state) ? classify(state, key, 6) : error("invalid_evidence", 400);
    }
    if (url.pathname === "/v5/turn-verdict") {
      return evidenceV5(state) ? classify(state, key, 5) : error("invalid_evidence", 400);
    }
    if (url.pathname === "/v4/turn-verdict") {
      return evidenceV4(state) ? classify(state, key, 4) : error("invalid_evidence", 400);
    }
    if (url.pathname === "/v2/turn-verdict" || url.pathname === "/v3/turn-verdict") {
      return evidenceV2(state) ? classify(state, key, url.pathname === "/v3/turn-verdict" ? 3 : 2) : error("invalid_evidence", 400);
    }
    return evidence(state) ? classify(state, key) : error("invalid_evidence", 400);
  },
} satisfies ExportedHandler<Env>;
