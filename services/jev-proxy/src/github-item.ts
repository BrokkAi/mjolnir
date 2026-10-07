import questions from "../../../mj-core/src/github_item/questions.json" with { type: "json" };

export { questions as githubItemQuestions };

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
