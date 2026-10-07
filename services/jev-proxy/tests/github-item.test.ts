import assert from "node:assert/strict";
import { test } from "node:test";
import proxy from "../src/index.ts";
import { githubItemQuestions } from "../src/github-item.ts";

const evidence = {
  item: {
    repo: "brokkai/mjolnir",
    kind: "pull_request",
    number: 42,
    title: "Add mailbox support",
    body: "Adds delivery through tool hooks.",
    author: "agent",
    url: "https://github.com/brokkai/mjolnir/pull/42",
  },
  session: { recent_turns: "<turn number=\"1\">Implement the mailbox.</turn>" },
};
const answers = {
  interested: { type: "noul", noul: 0.98 },
  created: { type: "noul", noul: 0.96 },
};

function request(body: unknown = evidence) {
  return new Request("https://proxy.example/v1/github-item-verdict", {
    method: "POST",
    headers: { "Content-Type": "application/json", "CF-Connecting-IP": "192.0.2.1" },
    body: JSON.stringify(body),
  });
}

function env() {
  return {
    TYPESAFE_API_KEY: "test-key",
    TURN_RATE_LIMITER: { async limit() { throw new Error("separate budget"); } },
    CONTINUATION_RATE_LIMITER: { async limit() { return { success: true }; } },
  };
}

test("GitHub item verdict forwards ordered item and session evidence and returns typed answers", async t => {
  t.mock.method(globalThis, "fetch", async (_url: unknown, options: RequestInit) => {
    const body = JSON.parse(options.body as string);
    assert.deepEqual(body, { model: "jev-latest", state: evidence, questions: githubItemQuestions });
    assert.deepEqual(Object.keys(body.state), ["item", "session"]);
    assert.deepEqual(Object.keys(body.state.item), ["repo", "kind", "number", "title", "body", "author", "url"]);
    assert.deepEqual(Object.keys(body.state.session), ["recent_turns"]);
    return Response.json({ answers, private: "discard" });
  });
  const response = await proxy.fetch(request(), env());
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), { answers });
});

test("GitHub item verdict rejects malformed evidence and provider answers", async t => {
  let upstream: unknown = { answers };
  const fetch = t.mock.method(globalThis, "fetch", async () => Response.json(upstream));
  for (const value of [
    {},
    { ...evidence, extra: true },
    { ...evidence, item: { ...evidence.item, kind: "discussion" } },
    { ...evidence, item: { ...evidence.item, number: 0 } },
  ]) {
    assert.equal((await proxy.fetch(request(value), env())).status, 400);
  }
  assert.equal(fetch.mock.callCount(), 0);
  for (upstream of [
    {},
    { answers: { interested: answers.interested } },
    { answers: { ...answers, created: { type: "choice", choice: "yes" } } },
    { answers: { ...answers, interested: { type: "noul", noul: 1.1 } } },
  ]) {
    assert.equal((await proxy.fetch(request(), env())).status, 502);
  }
});

test("GitHub item verdict enforces the complete upstream request cap", async t => {
  const fetch = t.mock.method(globalThis, "fetch", async () => Response.json({ answers }));
  const oversizedUpstream = {
    ...evidence,
    item: { ...evidence.item, body: "x".repeat(63 * 1024) },
  };
  const response = await proxy.fetch(request(oversizedUpstream), env());
  assert.equal(response.status, 413);
  assert.equal(fetch.mock.callCount(), 0);
});
