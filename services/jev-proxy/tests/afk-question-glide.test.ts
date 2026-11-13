import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { test, type TestContext } from "node:test";
import proxy from "../src/index.ts";
import questions from "../../../mj-worker/src/acp/afk_question_glide.json" with { type: "json" };

const state = {
  current_question: { message: "Who owns #11482 and who owns #10941?",
    fields: [{ kind: "text", title: "Assignments", required: true }] },
  previous_questions: [{ message: "Who should own #10941?",
    fields: [{ kind: "text", title: "Owner", required: true }] }],
};
const scores = { same: 0.2, different: 0.8 };
let allowed = true;
const limited: string[] = [];
const environment = { FASTINO_API_KEY: "fastino-test-key", TYPESAFE_API_KEY: "",
  TURN_RATE_LIMITER: { async limit({ key }: { key: string }) { limited.push(key); return { success: allowed }; } } };
function request(body: unknown = state) {
  return new Request("https://proxy.example/v1/afk-question-glide", { method: "POST",
    headers: { "Content-Type": "application/json", "CF-Connecting-IP": "192.0.2.7" }, body: JSON.stringify(body) });
}
function upstream(t: TestContext, handler: typeof fetch) {
  const mocked = t.mock.method(globalThis, "fetch", handler);
  t.after(() => mocked.mock.restore());
}

test("AFK GLiDE question is the frozen research file", () => {
  // SHA256SUMS in /mnt/optane/tmp/glide-afk-locator-20261113/afk/glide/FROZEN-v2/
  const bytes = readFileSync(new URL("../../../mj-worker/src/acp/afk_question_glide.json", import.meta.url));
  assert.equal(createHash("sha256").update(bytes).digest("hex"),
    "82a99d261302bb1013e79896a9faf57dfce3f607382d65994f52f785a29e9a4b");
});

test("AFK GLiDE sends the frozen question and returns probabilities", async t => {
  upstream(t, async (url, options) => {
    assert.equal(url, "https://api.fastino.ai/v1/systemone");
    assert.deepEqual(options!.headers, { "Content-Type": "application/json", "X-API-Key": "fastino-test-key" });
    assert.deepEqual(JSON.parse(options!.body as string), { model: "fastino/GLiDE", state, questions, store: false });
    return Response.json({ answers: { same_question: { type: "choice", choice: "different", confidence: 0.8, probabilities: scores } } });
  });
  const response = await proxy.fetch(request(), environment);
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), { probabilities: scores });
});

test("AFK GLiDE rejects each invalid evidence shape before any provider call", async t => {
  const calls = t.mock.method(globalThis, "fetch", async () => { throw Error("provider must not be called"); });
  const invalid = [null, {}, { ...state, extra: true }, { current_question: state.current_question },
    { ...state, previous_questions: [] }, { ...state, previous_questions: [{ message: 1, fields: [] }] },
    { ...state, current_question: { message: "x", fields: [1] } }];
  for (const value of invalid) assert.equal((await proxy.fetch(request(value), environment)).status, 400);
  assert.equal(calls.mock.callCount(), 0);
});

test("AFK GLiDE errors and backpressure pass through once with Retry-After", async t => {
  for (const status of [413, 425, 429, 503, 401]) {
    const mock = t.mock.method(globalThis, "fetch", async () => new Response("provider error", { status, headers: { "Retry-After": "12" } }));
    const response = await proxy.fetch(request(), environment);
    assert.equal(response.status, status);
    assert.equal(response.headers.get("Retry-After"), "12");
    assert.equal(await response.text(), "provider error");
    assert.equal(mock.mock.callCount(), 1);
    mock.mock.restore();
  }
});

test("AFK GLiDE rejects malformed and incomplete distributions", async t => {
  for (const probabilities of [{ same: 1 }, { same: 0.5, different: 0.5, extra: 0 }, { same: -0.1, different: 1.1 },
    { same: 0.2, different: 0.9 }, { same: "0.2", different: 0.8 }, [0.2, 0.8]]) {
    upstream(t, async () => Response.json({ answers: { same_question: { probabilities } } }));
    assert.equal((await proxy.fetch(request(), environment)).status, 502);
  }
  upstream(t, async () => Response.json({ answers: {} }));
  assert.equal((await proxy.fetch(request(), environment)).status, 502);
});

test("AFK GLiDE accepts a sum within 0.03 and no more", async t => {
  upstream(t, async () => Response.json({ answers: { same_question: { probabilities: { same: 0.2, different: 0.82 } } } }));
  assert.equal((await proxy.fetch(request(), environment)).status, 200);
  upstream(t, async () => Response.json({ answers: { same_question: { probabilities: { same: 0.2, different: 0.84 } } } }));
  assert.equal((await proxy.fetch(request(), environment)).status, 502);
});

test("AFK GLiDE is rate limited per IP, keyed, and bounded before any provider call", async t => {
  const calls = t.mock.method(globalThis, "fetch", async () => { throw Error("provider must not be called"); });
  allowed = false;
  limited.length = 0;
  assert.equal((await proxy.fetch(request(), environment)).status, 429);
  assert.deepEqual(limited, ["192.0.2.7"]);
  allowed = true;
  assert.equal((await proxy.fetch(request(), { ...environment, FASTINO_API_KEY: "" })).status, 503);
  const oversized = await proxy.fetch(request({ ...state, extra: "x".repeat(257 * 1024) }), environment);
  assert.equal(oversized.status, 413);
  // A body above the Jev route's 64 KiB bound but within the GLiDE bound reaches the provider.
  calls.mock.restore();
  upstream(t, async () => Response.json({ answers: { same_question: { probabilities: scores } } }));
  const large = { ...state, previous_questions: [{ message: "x".repeat(100 * 1024), fields: [] }] };
  assert.equal((await proxy.fetch(request(large), environment)).status, 200);
});
