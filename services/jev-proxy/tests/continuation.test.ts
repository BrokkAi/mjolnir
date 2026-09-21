import assert from "node:assert/strict";
import { test } from "node:test";
import proxy from "../src/index.ts";
import { continuationQuestions } from "../src/continuation.ts";
const evidence = { assistant_history_omitted: false, messages: [
  { id: "user:one", role: "user", text: "Implement the parser and run its tests." },
  { id: "reply:one", role: "assistant", text: "Implemented the parser. Shall I run the tests?" },
] };
const answers = { unfinished: { type: "noul", noul: 0.99 }, no_input_needed: { type: "noul", noul: 0.98 } };
function request(body: unknown = evidence) {
  return new Request("https://proxy.example/v1/continuation-verdict", { method: "POST",
    headers: { "Content-Type": "application/json", "CF-Connecting-IP": "192.0.2.1" }, body: JSON.stringify(body) });
}
function env(success = true) {
  return { TYPESAFE_API_KEY: "test-key", TURN_RATE_LIMITER: { async limit() { throw new Error("separate budget"); } },
    CONTINUATION_RATE_LIMITER: { async limit() { return { success }; } } };
}
test("continuation uses fixed shared questions and returns only bounded probabilities", async t => {
  t.mock.method(globalThis, "fetch", async (_url: unknown, options: RequestInit) => {
    assert.deepEqual(JSON.parse(options.body as string), { model: "jev-latest", state: evidence, questions: continuationQuestions });
    return Response.json({ answers, private: "discard" });
  });
  const response = await proxy.fetch(request(), env());
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), { answers });
});
test("continuation refuses partial evidence, arbitrary questions, and oversized Unicode", async t => {
  const fetch = t.mock.method(globalThis, "fetch", async () => Response.json({ answers }));
  for (const value of [{}, { ...evidence, questions: {} }, { assistant_history_omitted: false, messages: [] },
    { assistant_history_omitted: false, messages: [evidence.messages[0]] }, { assistant_history_omitted: false, messages: [evidence.messages[1]] },
    { assistant_history_omitted: false, messages: [{ ...evidence.messages[0], text: "😀".repeat(8193) }, evidence.messages[1]] },
    { assistant_history_omitted: false, messages: [evidence.messages[0], { ...evidence.messages[1], role: "system" }] },
  ]) assert.equal((await proxy.fetch(request(value), env())).status, 400);
  assert.equal(fetch.mock.callCount(), 0);
});
test("continuation fails closed on invalid probabilities, missing answers, and rate limits", async t => {
  let body: unknown = {};
  t.mock.method(globalThis, "fetch", async () => Response.json(body));
  for (body of [{}, { answers: { unfinished: answers.unfinished } },
    { answers: { ...answers, unfinished: { type: "noul", noul: 1.1 } } }]) {
    assert.equal((await proxy.fetch(request(), env())).status, 502);
  }
  assert.equal((await proxy.fetch(request(), env(false))).status, 429);
  assert.equal((await proxy.fetch(request(), { ...env(), CONTINUATION_RATE_LIMITER: undefined })).status, 503);
});

test("v2 accepts independent quota evidence and requires a bounded quota answer", async t => {
  const state = { assistant_history_omitted: true, messages: [], quota_message: "You've hit your session limit · resets 1:20pm (America/Chicago)" };
  let result: unknown = { answers: { ...answers, quota_limit: { type: "noul", noul: 0.99 } } };
  t.mock.method(globalThis, "fetch", async (_url: unknown, options: RequestInit) => {
    const body = JSON.parse(options.body as string);
    assert.deepEqual(body.state, state);
    assert.equal(body.questions.quota_limit.type, "noul");
    return Response.json(result);
  });
  const v2 = () => new Request("https://proxy.example/v2/continuation-verdict", {
    method: "POST", headers: { "Content-Type": "application/json", "CF-Connecting-IP": "192.0.2.1" }, body: JSON.stringify(state),
  });
  assert.equal((await proxy.fetch(v2(), env())).status, 200);
  for (const quota of [undefined, { type: "noul", noul: 1.01 }, { type: "choice", noul: 0.99 }]) {
    result = { answers: { ...answers, quota_limit: quota } };
    assert.equal((await proxy.fetch(v2(), env())).status, 502);
  }
  assert.equal((await proxy.fetch(request(state), env())).status, 400);
});
