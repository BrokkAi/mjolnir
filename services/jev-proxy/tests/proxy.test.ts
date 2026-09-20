import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import proxy from "../src/index.ts";
import questions from "../../../mj-core/src/activity/verdict_questions_v1.json" with { type: "json" };

const base = {
  harness: "claude", phase: "running", silent_for_s: 60,
  tools_in_flight: [{ title: "Bash", running_s: 94 }], recent_tools: ["Read", "Bash"],
  background_commands: 0, queued_commands: 0,
  user_prompt_tail: "Do the task", assistant_text_tail: "Which option should I use?",
};
const answer = {
  answers: {
    waiting_on: { type: "choice", choice: "user", confidence: 0.95 },
    asked_question: { type: "noul", noul: 0.99 },
  },
};

function environment(success = true) {
  return {
    TYPESAFE_API_KEY: "private-test-key",
    TURN_RATE_LIMITER: { async limit({ key }: { key: string }) {
      assert.equal(key, "192.0.2.1");
      return { success };
    } },
  };
}

function request(state: unknown = base, headers = {}) {
  return new Request("https://proxy.example/v1/turn-verdict", {
    method: "POST",
    headers: { "Content-Type": "application/json", "CF-Connecting-IP": "192.0.2.1", ...headers },
    body: JSON.stringify(state),
  });
}

function upstream(t: TestContext, handler: typeof fetch = async () => Response.json(answer)) {
  const mocked = t.mock.method(globalThis, "fetch", handler);
  t.after(() => mocked.mock.restore());
  return mocked.mock;
}

async function expectError(response: Response, status: number, code?: string) {
  assert.equal(response.status, status);
  const body = await response.json();
  assert.deepEqual(Object.keys(body), ["error"]);
  if (code) assert.equal(body.error, code);
  assert.equal(response.headers.get("Cache-Control"), "no-store");
  assert.equal(response.headers.get("Access-Control-Allow-Origin"), null);
  assert.ok(!JSON.stringify(body).includes("private-test-key"));
}

test("forwards fixed questions and server key and returns only typed answers", async t => {
  const calls = upstream(t, async (url, options) => {
    assert.equal(url, "https://api.typesafe.ai/v1/systemone");
    assert.equal(options?.method, "POST");
    assert.equal(options?.redirect, "manual");
    const headers = new Headers(options?.headers);
    assert.equal(headers.get("Authorization"), "Bearer private-test-key");
    assert.equal(headers.get("X-Caller-Header"), null);
    assert.deepEqual(JSON.parse(options!.body as string), { model: "jev-latest", state: base, questions });
    return Response.json({ ...answer, debug: "private-test-key" });
  });
  const response = await proxy.fetch(request(base, { Authorization: "Bearer caller-key", "X-Caller-Header": "secret" }), environment());
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), answer);
  assert.equal(response.headers.get("Cache-Control"), "no-store");
  assert.equal(calls.callCount(), 1);
});

test("rejects malformed evidence and arbitrary proxy controls before fetching", async t => {
  const calls = upstream(t);
  const bad = [null, [], {}, { ...base, model: "other" }, { ...base, questions: {} },
    { ...base, endpoint: "https://elsewhere.example" }, { ...base, phase: "other" },
    { ...base, silent_for_s: -1 }, { ...base, queued_commands: 1.5 },
    { ...base, background_commands: 2 ** 54 }, { ...base, harness: "" },
    { ...base, user_prompt_tail: "x".repeat(1025) }, { ...base, assistant_text_tail: "😀".repeat(513) },
    { ...base, recent_tools: Array(9).fill("Bash") },
    { ...base, tools_in_flight: Array(17).fill({ title: "Bash", running_s: 1 }) },
    { ...base, tools_in_flight: [{ title: "😀".repeat(33), running_s: 1 }] },
    { ...base, tools_in_flight: [{ title: "Bash", running_s: 1, extra: true }] },
  ];
  for (const state of bad) await expectError(await proxy.fetch(request(state), environment()), 400, "invalid_evidence");
  assert.equal(calls.callCount(), 0);
});

test("allows UTF-8 fields exactly at the existing byte and list limits", async t => {
  const calls = upstream(t);
  const state = { ...base, user_prompt_tail: "😀".repeat(256), assistant_text_tail: "😀".repeat(512),
    recent_tools: Array(8).fill("😀".repeat(32)),
    tools_in_flight: Array(16).fill({ title: "😀".repeat(32), running_s: 1 }) };
  assert.equal((await proxy.fetch(request(state), environment())).status, 200);
  assert.equal(calls.callCount(), 1);
});

test("rejects unsupported routes, methods and content types", async t => {
  const calls = upstream(t);
  await expectError(await proxy.fetch(new Request("https://proxy.example/"), environment()), 404);
  await expectError(await proxy.fetch(new Request("https://proxy.example/v1/turn-verdict?url=x"), environment()), 404);
  const method = await proxy.fetch(new Request("https://proxy.example/v1/turn-verdict"), environment());
  await expectError(method, 405);
  assert.equal(method.headers.get("Allow"), "POST");
  await expectError(await proxy.fetch(request(base, { "Content-Type": "text/plain" }), environment()), 415);
  assert.equal(calls.callCount(), 0);
});

test("rejects invalid JSON and bodies over 64 KiB even without Content-Length", async t => {
  const calls = upstream(t);
  for (const [body, status] of [["{", 400], [" ".repeat(128 * 1024), 413]] as const) {
    const incoming = new Request("https://proxy.example/v1/turn-verdict", {
      method: "POST", headers: { "Content-Type": "application/json", "CF-Connecting-IP": "192.0.2.1" }, body,
    });
    await expectError(await proxy.fetch(incoming, environment()), status);
  }
  await expectError(await proxy.fetch(request(base, { "Content-Length": "131072" }), environment()), 413);
  assert.equal(calls.callCount(), 0);
});

test("rate limiting and missing configuration never send upstream requests", async t => {
  const calls = upstream(t);
  const limited = await proxy.fetch(request(), environment(false));
  await expectError(limited, 429, "rate_limited");
  assert.equal(limited.headers.get("Retry-After"), "60");
  await expectError(await proxy.fetch(request(), { ...environment(), TYPESAFE_API_KEY: " " }), 503);
  const noIp = request(); noIp.headers.delete("CF-Connecting-IP");
  await expectError(await proxy.fetch(noIp, environment()), 503);
  const broken = { ...environment(), TURN_RATE_LIMITER: { async limit() { throw new Error("internal details"); } } };
  await expectError(await proxy.fetch(request(), broken), 503);
  assert.equal(calls.callCount(), 0);
});

test("upstream failures and redirects are sanitized without retry", async t => {
  for (const status of [302, 401, 429, 500, 529]) {
    const calls = upstream(t, async () => new Response("private-test-key", { status, headers: { Location: "https://elsewhere.example" } }));
    await expectError(await proxy.fetch(request(), environment()), 502, "upstream_unavailable");
    assert.equal(calls.callCount(), 1);
    calls.restore();
  }
  upstream(t, async () => { throw new Error("private-test-key"); });
  await expectError(await proxy.fetch(request(), environment()), 502, "upstream_failure");
});

test("rejects oversized and malformed upstream answers", async t => {
  for (const body of [" ".repeat(128 * 1024), "{", "{}", JSON.stringify({ answers: { ...answer.answers, asked_question: 0.99 } }),
    JSON.stringify({ answers: { ...answer.answers, waiting_on: { type: "choice", choice: "user", confidence: 1.1 } } })]) {
    const calls = upstream(t, async () => new Response(body));
    await expectError(await proxy.fetch(request(), environment()), 502);
    assert.equal(calls.callCount(), 1);
    calls.restore();
  }
});

test("the eight-second deadline aborts a pending upstream fetch", async t => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  let entered!: () => void;
  const started = new Promise<void>(resolve => { entered = resolve; });
  let signal: AbortSignal | undefined;
  upstream(t, async (_, options) => {
    signal = options?.signal as AbortSignal;
    entered();
    return new Promise<Response>((_, reject) => signal!.addEventListener("abort", () => reject(new Error("aborted"))));
  });
  const response = proxy.fetch(request(), environment());
  await started;
  t.mock.timers.tick(8000);
  await expectError(await response, 504, "upstream_timeout");
  assert.equal(signal?.aborted, true);
});

test("the deadline also covers a stalled response body", async t => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  let entered!: () => void;
  const started = new Promise<void>(resolve => { entered = resolve; });
  upstream(t, async (_, options) => new Response(new ReadableStream({
    start(controller) {
      options?.signal?.addEventListener("abort", () => controller.error(new Error("aborted")));
      entered();
    },
  })));
  const response = proxy.fetch(request(), environment());
  await started;
  t.mock.timers.tick(8000);
  await expectError(await response, 504, "upstream_timeout");
});


test("v2 forwards shared transcript evidence and rejects legacy/mixed shapes", async t => {
  const { recent_tools: _old, ...rest } = base;
  const state = { ...rest, transcript_summary: '<tool cargo test [completed]>full results</tool>' };
  const calls = upstream(t, async (_url, options) => {
    const forwarded = JSON.parse(options!.body as string);
    assert.deepEqual(forwarded.state, state);
    assert.match(forwarded.questions.waiting_on.instructions, /transcript_summary/);
    return Response.json(answer);
  });
  function v2(body: unknown) {
    return new Request("https://proxy.example/v2/turn-verdict", { method: "POST", headers: { "Content-Type": "application/json", "CF-Connecting-IP": "192.0.2.1" }, body: JSON.stringify(body) });
  }
  assert.equal((await proxy.fetch(v2(state), environment())).status, 200);
  for (const bad of [base, {...state,recent_tools:[]}, {...state,transcript_summary:"x".repeat(48 * 1024 + 1)}, {...state,extra:true}]) {
    await expectError(await proxy.fetch(v2(bad), environment()),400,"invalid_evidence");
  }
  assert.equal(calls.callCount(),1);
});
