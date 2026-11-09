import questions from "../../../mj-core/src/github_item/questions.json" with { type: "json" };
import questionsV2 from "../../../mj-core/src/github_item/questions_v2.json" with { type: "json" };

export { questions as githubItemQuestions };
export { questionsV2 as githubItemQuestionsV2 };

export interface GithubItemEvidence {
  item: {
    repo: string;
    kind: "issue" | "pull_request";
    number: number;
    title: string;
    body: string;
    author: string;
    url: string;
  };
  session: { recent_turns: string };
}

export interface GithubItemEvidenceV2 {
  item: GithubItemEvidence["item"];
  session: { conversation: string; files: string[]; references: string[]; mentions?: string[] };
}

const encoder = new TextEncoder();
function object(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
function text(value: unknown, limit: number, nonempty = true): value is string {
  return typeof value === "string" && (!nonempty || value.trim().length > 0)
    && encoder.encode(value).byteLength <= limit;
}
function fields(value: Record<string, unknown>, expected: string[]): boolean {
  return Object.keys(value).length === expected.length && expected.every(key => key in value);
}

export function githubItemRequest(value: unknown): value is GithubItemEvidence {
  if (!object(value) || !fields(value, ["item", "session"])
    || !object(value.item) || !fields(value.item, ["repo", "kind", "number", "title", "body", "author", "url"])
    || !object(value.session) || !fields(value.session, ["recent_turns"])) return false;
  const item = value.item;
  return text(item.repo, 256) && (item.kind === "issue" || item.kind === "pull_request")
    && typeof item.number === "number" && Number.isSafeInteger(item.number) && item.number > 0
    && text(item.title, 1024) && text(item.body, 64 * 1024, false)
    && text(item.author, 256, false) && text(item.url, 2048)
    && text(value.session.recent_turns, 64 * 1024, false);
}

export function githubItemAnswers(value: unknown): Record<string, unknown> | undefined {
  if (!object(value) || !object(value.answers) || !fields(value.answers, ["interested", "created"])) return;
  const result: Record<string, unknown> = {};
  for (const key of ["interested", "created"]) {
    const answer = value.answers[key];
    if (!object(answer) || answer.type !== "noul" || typeof answer.noul !== "number"
      || !Number.isFinite(answer.noul) || answer.noul < 0 || answer.noul > 1) return;
    result[key] = { type: "noul", noul: answer.noul };
  }
  return result;
}

export function githubItemRequestV2(value: unknown): value is GithubItemEvidenceV2 {
  if (!object(value) || !fields(value, ["item", "session"])
    || !object(value.item) || !fields(value.item, ["repo", "kind", "number", "title", "body", "author", "url"])
    || !object(value.session) || !fields(value.session, "mentions" in value.session ? ["conversation", "files", "references", "mentions"] : ["conversation", "files", "references"])) return false;
  const item = value.item;
  const session = value.session;
  const strings = (values: unknown, count: number, limit: number, nonempty = true) => Array.isArray(values)
    && values.length <= count && values.every(value => text(value, limit, nonempty));
  return text(item.repo, 256) && (item.kind === "issue" || item.kind === "pull_request")
    && typeof item.number === "number" && Number.isSafeInteger(item.number) && item.number > 0
    && text(item.title, 1024) && text(item.body, 4096 + 128, false)
    && text(item.author, 256, false) && text(item.url, 2048)
    && text(session.conversation, 8 * 1024, false)
    && strings(session.files, 32, 256) && strings(session.references, 32, 128)
    && (!("mentions" in session) || strings(session.mentions, 16, 256));
}

export function githubItemAnswersV2(value: unknown): Record<string, unknown> | undefined {
  if (!object(value) || !object(value.answers) || !fields(value.answers, ["interested"])) return;
  const answer = value.answers.interested;
  if (!object(answer) || answer.type !== "noul" || typeof answer.noul !== "number"
    || !Number.isFinite(answer.noul) || answer.noul < 0 || answer.noul > 1) return;
  return { interested: { type: "noul", noul: answer.noul } };
}
