// Test harness: boots a real `mj server --allow-spawn` against a stub ACP
// agent in an isolated HOME, then launches the system Chrome via
// puppeteer-core (no bundled Chromium download). Everything is torn down and
// the temp HOME removed on stop().
const { spawn } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const puppeteer = require("puppeteer-core");

const REPO_ROOT = path.resolve(__dirname, "..", "..", "..");
const MJ_BIN = path.join(REPO_ROOT, "target", "debug", "mj");
const STUB_AGENT = path.join(__dirname, "..", "fixtures", "stub-agent.mjs");
const PORT = 11921;
const BASE_URL = `https://localhost:${PORT}`;

// Chrome candidates, most-specific first. Override with MJ_TEST_CHROME.
function findChrome() {
  const candidates = [
    process.env.MJ_TEST_CHROME,
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/usr/bin/google-chrome",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
  ].filter(Boolean);
  for (const candidate of candidates) {
    if (fs.existsSync(candidate)) return candidate;
  }
  throw new Error(
    "no Chrome/Chromium found; set MJ_TEST_CHROME to its executable path",
  );
}

// dirs::config_dir() for the isolated HOME. Computed deterministically from
// `home` (never the runner's own XDG_CONFIG_HOME) and matched by forcing the
// child's XDG_CONFIG_HOME below, so the isolation holds on CI too.
function configBase(home) {
  return process.platform === "darwin"
    ? path.join(home, "Library", "Application Support")
    : path.join(home, ".config");
}

function tokenPath(home) {
  return path.join(configBase(home), "mj", "remote-control", "token");
}

function configPath(home) {
  return path.join(configBase(home), "mj", "config.toml");
}

async function waitFor(predicate, { timeout = 15000, interval = 100, label } = {}) {
  const start = Date.now();
  for (;;) {
    const value = await predicate();
    if (value) return value;
    if (Date.now() - start > timeout) {
      throw new Error(`timed out waiting for ${label || "condition"}`);
    }
    await new Promise((r) => setTimeout(r, interval));
  }
}

async function start() {
  if (!fs.existsSync(MJ_BIN)) {
    throw new Error(`mj binary missing at ${MJ_BIN}; run \`cargo build\` first`);
  }
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "mj-web-test-"));
  const workspace = path.join(home, "workspace");
  fs.mkdirSync(workspace, { recursive: true });
  fs.mkdirSync(path.dirname(configPath(home)), { recursive: true });
  fs.writeFileSync(
    configPath(home),
    [
      "[agent]",
      'source_id = "custom:stub"',
      `program = "${process.execPath}"`,
      `args = ["${STUB_AGENT}"]`,
      "",
      "[[custom_agents]]",
      'name = "stub"',
      `program = "${process.execPath}"`,
      `args = ["${STUB_AGENT}"]`,
      'description = "stub agent for browser tests"',
      "",
    ].join("\n"),
  );

  const server = spawn(
    MJ_BIN,
    ["--cwd", workspace, "server", "--allow-spawn"],
    {
      // Force both HOME and XDG_CONFIG_HOME under the temp dir so config and
      // secrets land where we expect, regardless of the host/runner env.
      env: {
        ...process.env,
        HOME: home,
        XDG_CONFIG_HOME: path.join(home, ".config"),
      },
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  let serverLog = "";
  server.stdout.on("data", (d) => (serverLog += d.toString()));
  server.stderr.on("data", (d) => (serverLog += d.toString()));

  await waitFor(() => /Remote control listening/.test(serverLog), {
    label: "server startup",
    timeout: 20000,
  });
  const token = (
    await waitFor(
      () =>
        fs.existsSync(tokenPath(home)) &&
        fs.readFileSync(tokenPath(home), "utf8").trim(),
      { label: "server token" },
    )
  ).trim();

  const browser = await puppeteer.launch({
    executablePath: findChrome(),
    headless: true,
    acceptInsecureCerts: true,
    args: ["--no-sandbox", "--ignore-certificate-errors"],
  });

  return {
    home,
    token,
    baseUrl: BASE_URL,
    browser,
    serverLog: () => serverLog,
    async stop() {
      try {
        await browser.close();
      } catch {}
      server.kill("SIGINT");
      await new Promise((r) => setTimeout(r, 500));
      if (!server.killed) server.kill("SIGKILL");
      try {
        fs.rmSync(home, { recursive: true, force: true });
      } catch {}
    },
  };
}

module.exports = { start, waitFor, BASE_URL };
