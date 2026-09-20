import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import proxy from "../src/index.ts";
import { helpQuestions } from "../src/help-search.ts";
import question from "../../../mj-core/src/help_search/question.json" with { type: "json" };

const catalog = {
  query: "leave agents running when I exit",
  entries: [
    { id: 3, category: "Essentials", label: "Detach", description: "Leave the terminal; sessions keep running." },
    { id: 7, category: "Sessions", label: "Stop session", description: "Stop the selected session." },
  ],
};
const answer = { answers: { entry_3: { type: "noul", noul: 0.97 }, entry_7: { type: "noul", noul: 0.01 } } };
function request(body: unknown = catalog) {
  return new Request("https://proxy.example/v1/help-search", {
    method: "POST", headers: { "Content-Type": "application/json", "CF-Connecting-IP": "192.0.2.1" },
    body: JSON.stringify(body),
  });
}
function environment(success = true) {
  return { TYPESAFE_API_KEY: "private-test-key",
    TURN_RATE_LIMITER: { async limit() { throw new Error("help must not consume turn capacity"); } },
    HELP_RATE_LIMITER: { async limit({ key }: { key: string }) { assert.equal(key, "192.0.2.1"); return { success }; } },
  };
}
function upstream(t: TestContext, handler: typeof fetch) {
  const mocked = t.mock.method(globalThis, "fetch", handler);
  t.after(() => mocked.mock.restore());
  return mocked.mock;
}

test("help sends a bounded client catalog with fixed per-entry questions and returns only scores", async t => {
  upstream(t, async (url, options) => {
    assert.equal(url, "https://api.typesafe.ai/v1/systemone");
    assert.equal(new Headers(options!.headers).get("Authorization"), "Bearer private-test-key");
    const body = JSON.parse(options!.body as string);
    assert.deepEqual(body, { model: "jev-latest", state: catalog, questions: helpQuestions(catalog) });
    assert.deepEqual(body.questions.entry_3, { ...question, instructions: question.instructions.replace("INDEX", "0") });
    assert.ok(body.questions.entry_7.instructions.includes("entries[1]"));
    return Response.json({ ...answer, diagnostic: "private-test-key" });
  });
  const response = await proxy.fetch(request(), environment());
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), { scores: [{ id: 3, probability: 0.97 }, { id: 7, probability: 0.01 }] });
  assert.equal(response.headers.get("Cache-Control"), "no-store");
});

test("help rejects arbitrary proxy controls, duplicate IDs and out-of-bound fields before calling upstream", async t => {
  const calls = upstream(t, async () => Response.json(answer));
  for (const value of [null, {}, { ...catalog, query: " " }, { ...catalog, query: "😀".repeat(257) },
    { ...catalog, questions: {} }, { ...catalog, model: "other" }, { ...catalog, entries: [] },
    { ...catalog, entries: Array(129).fill(catalog.entries[0]) },
    { ...catalog, entries: [catalog.entries[0], catalog.entries[0]] },
    ...[-1, 128, 1.2].map(id => ({ ...catalog, entries: [{ ...catalog.entries[0], id }] })),
    { ...catalog, entries: [{ ...catalog.entries[0], description: "x".repeat(1025) }] },
    { ...catalog, entries: [{ ...catalog.entries[0], extra: true }] },
  ]) {
    assert.equal((await proxy.fetch(request(value), environment())).status, 400);
  }
  assert.equal(calls.callCount(), 0);
  assert.equal((await proxy.fetch(request({ ...catalog, query: "😀".repeat(256) }), environment())).status, 200);
});

test("help has an independent rate limit and fails gracefully when unavailable", async t => {
  const calls = upstream(t, async () => Response.json(answer));
  assert.equal((await proxy.fetch(request(), environment(false))).status, 429);
  assert.equal((await proxy.fetch(request(), { ...environment(), HELP_RATE_LIMITER: undefined })).status, 503);
  assert.equal(calls.callCount(), 0);
});

test("help rejects incomplete, unknown and invalid provider scores", async t => {
  for (const body of [{}, { answers: {} }, { answers: { entry_3: answer.answers.entry_3 } },
    { answers: { ...answer.answers, entry_8: answer.answers.entry_3 } },
    { answers: { ...answer.answers, entry_3: { type: "noul", noul: -0.1 } } },
    { answers: { ...answer.answers, entry_3: { type: "choice", noul: 0.9 } } },
  ]) {
    const calls = upstream(t, async () => Response.json(body));
    const response = await proxy.fetch(request(), environment());
    assert.equal(response.status, 502);
    assert.deepEqual(await response.json(), { error: "invalid_upstream_response" });
    calls.restore();
  }
});
