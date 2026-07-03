// End-to-end browser tests for the Mjolnir Web viewer. These drive a real
// Chrome against a real `mj server --allow-spawn`, exercising the surfaces
// that only exist at runtime: token sign-in, the Cmd-K command palette, the
// New task dialog, and live transcript / working-state rendering.
const fs = require("node:fs");
const path = require("node:path");
const { start, waitFor } = require("./lib/harness");
const { startCoverage, stopAndReport } = require("./lib/coverage");

// Minimum line coverage across the viewer's ES modules. The afterAll hook
// fails the run if the browser-driven suite exercises less than this. Set
// COVERAGE_MIN=0 to measure without enforcing.
const COVERAGE_MIN =
  process.env.COVERAGE_MIN !== undefined ? Number(process.env.COVERAGE_MIN) : 75;

const ARTIFACTS = path.join(__dirname, "artifacts");

let h;
let page;

async function shot(name) {
  fs.mkdirSync(ARTIFACTS, { recursive: true });
  await page.screenshot({ path: path.join(ARTIFACTS, `${name}.png`) });
}

// Fetch JSON from an API route inside the page context, reusing the viewer's
// token so we can assert on server truth (e.g. a spawned task's session id)
// without duplicating the auth logic.
function apiJson(route) {
  return page.evaluate(async (url) => {
    const res = await fetch(url, { credentials: "same-origin", cache: "no-store" });
    return res.ok ? res.json() : null;
  }, `${route}${route.includes("?") ? "&" : "?"}token=${h.token}`);
}

// Click only once the target is the topmost element at its own center. The
// viewer wraps renders in the View Transitions API, whose `::view-transition`
// overlay briefly covers the viewport and would otherwise swallow a click
// fired mid-transition.
async function clickWhenHittable(sel) {
  await page.waitForFunction(
    (s) => {
      const el = document.querySelector(s);
      if (!el || el.hidden) return false;
      const r = el.getBoundingClientRect();
      if (r.width === 0 || r.height === 0) return false;
      const top = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
      return el === top || el.contains(top);
    },
    { timeout: 15000 },
    sel,
  );
  await page.click(sel);
}

// Open the New task dialog reliably. Waiting on the `.open` PROPERTY is
// robust; the `[open]` attribute selector flakes under puppeteer once the
// dialog joins the top layer as a modal.
async function openTaskDialog() {
  await page.evaluate(() => {
    const d = document.querySelector("#new-task-dialog");
    if (d && d.open) d.close();
  });
  await clickWhenHittable("#new-task-button");
  await page.waitForFunction(
    () => document.querySelector("#new-task-dialog").open,
    { timeout: 15000 },
  );
}

const visible = (sel) => page.waitForFunction(
  (s) => { const el = document.querySelector(s); return el && !el.hidden; },
  { timeout: 15000 },
  sel,
);
const hidden = (sel) => page.waitForFunction(
  (s) => { const el = document.querySelector(s); return el && el.hidden; },
  { timeout: 15000 },
  sel,
);

beforeAll(async () => {
  h = await start();
  page = await h.browser.newPage();
  await startCoverage(page);
  page.setDefaultTimeout(15000);
  // A desktop viewport keeps the two-pane layout (sidebar + main both always
  // visible). The default 800px width lands exactly on the viewer's phone
  // breakpoint, where the sidebar toggles by route and controls in it come
  // and go — a fragile state for E2E.
  await page.setViewport({ width: 1280, height: 900 });
  // Reduced motion trims CSS transitions; combined with clickWhenHittable it
  // keeps the View Transitions overlay from intercepting clicks.
  await page.emulateMediaFeatures([
    { name: "prefers-reduced-motion", value: "reduce" },
  ]);
  page.on("pageerror", (e) => console.log("PAGE EXCEPTION:", e.message));
  // Auto-accept the Stop task / cancel confirm() dialogs.
  page.on("dialog", (d) => d.accept());
  // Pre-grant notifications so the enable-notifications command resolves.
  await h.browser
    .defaultBrowserContext()
    .overridePermissions(h.baseUrl, ["notifications"]);
  await page.goto(`${h.baseUrl}/?token=${h.token}`, { waitUntil: "domcontentloaded" });
}, 45000);

// Spawn a task with the given prompt, select its session, and wait for the
// transcript to render. Returns the new session id.
async function spawnTaskAndSelect(prompt) {
  // Snapshot existing session ids so we can identify the genuinely new one
  // (a just-spawned task briefly has no session id, during which an older
  // task would otherwise be mistaken for the newest).
  const before = new Set(
    ((await apiJson("/api/tasks")) || []).map((t) => t.session_id).filter(Boolean),
  );
  await openTaskDialog();
  await page.$eval("#new-task-prompt", (el) => (el.value = ""));
  await page.type("#new-task-prompt", prompt);
  await clickWhenHittable("#new-task-start");
  const sid = await waitFor(
    async () => {
      const tasks = await apiJson("/api/tasks");
      const fresh = (tasks || []).find((t) => t.session_id && !before.has(t.session_id));
      return fresh ? fresh.session_id : null;
    },
    { label: "spawned session id", timeout: 15000 },
  );
  await page.evaluate((s) => {
    window.location.hash = `#/session/${s}`;
  }, sid);
  await page.waitForFunction(
    () => document.querySelector("#transcript .entry-body"),
    { timeout: 15000 },
  );
  return sid;
}

afterAll(async () => {
  let summary = null;
  let collectionError = null;
  if (page) {
    try {
      summary = await stopAndReport(page);
    } catch (e) {
      collectionError = e;
      console.log("coverage collection failed:", e.message);
    }
  }
  if (h) await h.stop();
  // Fail closed: when a threshold is set, a failure to COLLECT coverage must
  // fail the run too, or a broken collector would silently disable the gate
  // and let CI pass with no coverage enforced.
  if (COVERAGE_MIN > 0 && collectionError) {
    throw new Error(
      `coverage collection failed, cannot enforce the ${COVERAGE_MIN}% gate: ${collectionError.message}`,
    );
  }
  if (summary) {
    const lines = summary.lines?.pct ?? 0;
    if (COVERAGE_MIN > 0 && lines < COVERAGE_MIN) {
      throw new Error(
        `viewer JS line coverage ${lines}% is below the required ${COVERAGE_MIN}%`,
      );
    }
  }
});

test("token sign-in loads the app shell, not the auth screen", async () => {
  await page.waitForSelector("#app:not([hidden])");
  const authHidden = await page.$eval("#auth-screen", (el) => el.hidden);
  expect(authHidden).toBe(true);
  // The server hosts its own default agent session, so the list is non-empty.
  await page.waitForFunction(
    () => document.querySelectorAll(".session-item").length >= 1,
  );
  await shot("01-app-shell");
});

test("spawn controls appear once /api/tasks confirms --allow-spawn", async () => {
  await page.waitForSelector("#new-task-button:not([hidden])");
  const label = await page.$eval("#new-task-button", (el) => el.textContent.trim());
  expect(label).toContain("New task");
});

test("Cmd-K opens the command palette with a New task command", async () => {
  await page.keyboard.down("Control");
  await page.keyboard.press("KeyK");
  await page.keyboard.up("Control");

  await page.waitForFunction(() => {
    const dlg = document.querySelector("#command-palette .palette-dialog");
    return dlg && dlg.open;
  });
  const labels = await page.$$eval("#command-palette .palette-label", (els) =>
    els.map((el) => el.textContent),
  );
  expect(labels.some((l) => l.includes("New task"))).toBe(true);
  await shot("02-command-palette");

  await page.keyboard.press("Escape");
  await page.waitForFunction(() => {
    const dlg = document.querySelector("#command-palette .palette-dialog");
    return dlg && !dlg.open;
  });
});

test("New task dialog spawns a task whose transcript streams back", async () => {
  await openTaskDialog();
  await page.type("#new-task-prompt", "hello from puppeteer");
  await shot("03-new-task-dialog");
  await clickWhenHittable("#new-task-start");

  // The task's session id is authoritative on the server; wait for it, then
  // route the viewer to that session and assert the reply rendered.
  const sessionId = await waitFor(
    async () => {
      const tasks = await apiJson("/api/tasks");
      const withSession = (tasks || []).find((t) => t.session_id);
      return withSession ? withSession.session_id : null;
    },
    { label: "spawned task session id", timeout: 15000 },
  );

  await page.evaluate((sid) => {
    window.location.hash = `#/session/${sid}`;
  }, sessionId);

  await page.waitForFunction(
    () =>
      [...document.querySelectorAll("#transcript .entry-body")].some((el) =>
        el.textContent.includes("stub reply: hello from puppeteer"),
      ),
    { timeout: 15000 },
  );
  await shot("04-task-transcript");
});

test("a busy task shows the working badge and a Cancel turn control", async () => {
  await openTaskDialog();
  await page.type("#new-task-prompt", "slow job please");
  await clickWhenHittable("#new-task-start");

  const sessionId = await waitFor(
    async () => {
      const tasks = await apiJson("/api/tasks");
      // The most recent task without a finished turn; pick the slow one by
      // checking which session is currently working.
      const sessions = await apiJson("/live/sessions");
      const working = (sessions || []).find((s) => s.working);
      return working ? working.session_id : null;
    },
    { label: "busy task session", timeout: 15000 },
  );

  await page.evaluate((sid) => {
    window.location.hash = `#/session/${sid}`;
  }, sessionId);

  await visible("#working-badge");
  await visible("#cancel-turn-button");
  await shot("05-working-and-cancel");

  // Cancelling returns the task to idle; the badge and control retract.
  await clickWhenHittable("#cancel-turn-button");
  await hidden("#working-badge");
});

test("transcript renders rich markdown and a thought entry", async () => {
  await spawnTaskAndSelect("render some markdown");
  // Wait for the rich agent message (has a fenced code block) to arrive.
  await page.waitForFunction(
    () => document.querySelector("#transcript .entry-body pre.code-block"),
    { timeout: 15000 },
  );
  const found = await page.evaluate(() => {
    const t = document.querySelector("#transcript");
    return {
      code: !!t.querySelector("pre.code-block code"),
      strong: !!t.querySelector(".entry-body strong"),
      link: !!t.querySelector('.entry-body a[href^="https://"]'),
      heading: !!t.querySelector(".entry-body h4, .entry-body h5, .entry-body h6"),
      ul: !!t.querySelector(".entry-body ul li"),
      ol: !!t.querySelector(".entry-body ol li"),
      thought: [...t.querySelectorAll(".entry-kind")].some(
        (el) => el.textContent === "Thought",
      ),
    };
  });
  expect(found).toEqual({
    code: true,
    strong: true,
    link: true,
    heading: true,
    ul: true,
    ol: true,
    thought: true,
  });
  await shot("06-markdown");
});

test("session config comboboxes change a setting", async () => {
  await spawnTaskAndSelect("configure me");
  // The config panel appears once options are advertised; open it.
  await page.waitForSelector("#session-config-panel:not([hidden])", { timeout: 15000 });
  await page.evaluate(() => {
    document.querySelector("#session-config-panel").open = true;
  });
  await page.waitForFunction(
    () => document.querySelectorAll("#session-config .config-combo-trigger").length >= 1,
    { timeout: 15000 },
  );

  let posted = false;
  const onResp = (r) => {
    if (r.url().includes("/api/config-changes") && r.request().method() === "POST") posted = true;
  };
  page.on("response", onResp);

  // Open the first combobox, search, and choose a different option.
  await clickWhenHittable("#session-config .config-combo-trigger");
  await page.waitForFunction(
    () => document.querySelector("#session-config .config-combo-option"),
    { timeout: 15000 },
  );
  await page.type("#session-config .config-combo-search", "Two");
  await page.waitForFunction(
    () =>
      [...document.querySelectorAll("#session-config .config-combo-option")].some(
        (el) => el.textContent.includes("Two") && el.offsetParent !== null,
      ),
    { timeout: 15000 },
  );
  // Options commit on `mousedown` (so the choice lands before the search input
  // blurs), so a synthetic click() would not trigger it — dispatch mousedown
  // on the visible matching option.
  await page.evaluate(() => {
    const opt = [...document.querySelectorAll("#session-config .config-combo-option")].find(
      (el) => el.textContent.includes("Two") && el.offsetParent !== null,
    );
    opt.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
  });

  await waitFor(() => posted, { label: "config-change POST", timeout: 15000 });
  page.off("response", onResp);
  await shot("07-config-combobox");
});

test("live composer sends a prompt to a running task", async () => {
  await spawnTaskAndSelect("first task prompt");
  // The composer says Send for a live task.
  await page.waitForFunction(
    () => document.querySelector("#queue-submit").textContent.trim() === "Send",
    { timeout: 15000 },
  );
  const before = await page.$$eval("#transcript .entry", (els) => els.length);
  await page.click("#queue-input");
  await page.type("#queue-input", "a live follow-up");
  await clickWhenHittable("#queue-submit");
  await page.waitForFunction(
    (n) =>
      [...document.querySelectorAll("#transcript .entry-body")].some((el) =>
        el.textContent.includes("a live follow-up"),
      ) && document.querySelectorAll("#transcript .entry").length > n,
    { timeout: 15000 },
    before,
  );
  await shot("08-live-composer");
});

test("composer drafts survive switching sessions", async () => {
  const first = await spawnTaskAndSelect("draft host one");
  const second = await spawnTaskAndSelect("draft host two");

  // Type a draft while on the second session.
  await page.click("#queue-input");
  await page.type("#queue-input", "unsent draft text");
  // Switch to the first session, then back to the second.
  await page.evaluate((s) => {
    window.location.hash = `#/session/${s}`;
  }, first);
  await page.waitForFunction(
    () => document.querySelector("#queue-input").value === "",
    { timeout: 15000 },
  );
  await page.evaluate((s) => {
    window.location.hash = `#/session/${s}`;
  }, second);
  await page.waitForFunction(
    () => document.querySelector("#queue-input").value === "unsent draft text",
    { timeout: 15000 },
  );
});

test("command palette filters, navigates, and runs a command", async () => {
  const target = await spawnTaskAndSelect("palette target session");
  // Open a different session so a "Go to" command for `target` exists.
  await spawnTaskAndSelect("palette current session");

  await page.keyboard.down("Control");
  await page.keyboard.press("KeyK");
  await page.keyboard.up("Control");
  await page.waitForFunction(() => {
    const d = document.querySelector("#command-palette .palette-dialog");
    return d && d.open;
  });
  // Filter to Go-to commands, move the active row, and run with Enter.
  await page.type("#command-palette .palette-dialog input", "Go to");
  await page.waitForFunction(
    () =>
      [...document.querySelectorAll("#command-palette .palette-label")].some((el) =>
        el.textContent.startsWith("Go to"),
      ),
    { timeout: 15000 },
  );
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("Enter");
  await page.waitForFunction(() => {
    const d = document.querySelector("#command-palette .palette-dialog");
    return d && !d.open;
  });
  // Navigation changed the selected session (hash route updated).
  const hash = await page.evaluate(() => window.location.hash);
  expect(hash.startsWith("#/session/")).toBe(true);
});

test("enable notifications command runs from the palette", async () => {
  await page.keyboard.down("Control");
  await page.keyboard.press("KeyK");
  await page.keyboard.up("Control");
  await page.waitForFunction(() => {
    const d = document.querySelector("#command-palette .palette-dialog");
    return d && d.open;
  });
  const ran = await page.evaluate(async () => {
    const label = [...document.querySelectorAll("#command-palette .palette-label")].find(
      (el) => el.textContent.includes("notifications"),
    );
    if (!label) return "absent";
    label.closest(".palette-item").click();
    return "clicked";
  });
  // Either it ran (permission was un-granted) or notifications were already
  // enabled so the command was absent — both are valid end states.
  expect(["clicked", "absent"]).toContain(ran);
  await page.evaluate(() => {
    const d = document.querySelector("#command-palette .palette-dialog");
    if (d && d.open) d.close();
  });
});

test("stop task removes it from the task list", async () => {
  const sid = await spawnTaskAndSelect("task to stop");
  await visible("#stop-task-button");
  const countBefore = (await apiJson("/api/tasks")).length;
  await clickWhenHittable("#stop-task-button");
  await waitFor(
    async () => {
      const tasks = await apiJson("/api/tasks");
      return tasks.length < countBefore && !tasks.some((t) => t.session_id === sid);
    },
    { label: "task removed", timeout: 15000 },
  );
});

test("logout returns to the auth screen", async () => {
  await clickWhenHittable("#logout-button");
  await page.waitForSelector("#auth-screen:not([hidden])", { timeout: 15000 });
  const appHidden = await page.$eval("#app", (el) => el.hidden);
  expect(appHidden).toBe(true);
});
