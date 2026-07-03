// V8 JavaScript coverage for the browser-driven viewer, computed directly
// from Chrome's covered byte ranges — no third-party coverage library, so
// there is nothing to misclassify our ES modules. Chrome records which byte
// ranges of each served module actually executed during the E2E run; we map
// those to source lines and report per-file and overall line coverage. This
// is real coverage of code as the browser ran it.
const fs = require("node:fs");
const path = require("node:path");

const OUTPUT_DIR = path.join(__dirname, "..", "artifacts", "coverage");

async function startCoverage(page) {
  await page.coverage.startJSCoverage({ resetOnNavigation: false });
}

// A line counts toward the denominator only if it holds real code: it has a
// word character and is not a pure line/block comment. This drops blank
// lines, brace-only lines, and comments so the percentage reflects logic, not
// formatting.
function isCoverable(line) {
  const t = line.trim();
  if (!t) return false;
  if (t.startsWith("//") || t.startsWith("*") || t.startsWith("/*")) return false;
  return /[A-Za-z0-9]/.test(t);
}

function fileLineCoverage(text, ranges) {
  const hits = new Uint8Array(text.length);
  for (const { start, end } of ranges) {
    for (let i = start; i < end && i < hits.length; i += 1) hits[i] = 1;
  }
  const lines = text.split("\n");
  let offset = 0;
  let total = 0;
  let covered = 0;
  const lineHits = [];
  for (const line of lines) {
    const lineStart = offset;
    offset += line.length + 1; // + newline
    if (!isCoverable(line)) {
      lineHits.push(null);
      continue;
    }
    total += 1;
    let isCovered = false;
    for (let i = 0; i < line.length; i += 1) {
      if (/\S/.test(line[i]) && hits[lineStart + i]) {
        isCovered = true;
        break;
      }
    }
    if (isCovered) covered += 1;
    lineHits.push(isCovered ? 1 : 0);
  }
  return { total, covered, lineHits };
}

async function startAndReturnStop(page) {
  await startCoverage(page);
}

// Stop collection, write an lcov artifact, print a per-file table, and return
// an overall summary `{ lines: { pct, covered, total }, files: [...] }`.
async function stopAndReport(page) {
  const coverage = await page.coverage.stopJSCoverage();
  const modules = coverage.filter((entry) =>
    /\/assets\/[^/]+\.js(\?|$)/.test(entry.url),
  );

  fs.mkdirSync(OUTPUT_DIR, { recursive: true });
  const lcov = [];
  const files = [];
  let totalAll = 0;
  let coveredAll = 0;

  for (const entry of modules) {
    const name = entry.url.replace(/^.*\/assets\//, "").replace(/\?.*$/, "");
    const { total, covered, lineHits } = fileLineCoverage(entry.text, entry.ranges);
    totalAll += total;
    coveredAll += covered;
    const pct = total ? Math.round((covered / total) * 1000) / 10 : 100;
    files.push({ name, total, covered, pct });

    lcov.push(`SF:src/remote_assets/${name}`);
    lineHits.forEach((hit, idx) => {
      if (hit !== null) lcov.push(`DA:${idx + 1},${hit}`);
    });
    lcov.push(`LF:${total}`, `LH:${covered}`, "end_of_record");
  }

  fs.writeFileSync(path.join(OUTPUT_DIR, "lcov.info"), lcov.join("\n") + "\n");

  const overallPct = totalAll ? Math.round((coveredAll / totalAll) * 1000) / 10 : 0;
  files.sort((a, b) => a.name.localeCompare(b.name));
  console.log("\n  JS line coverage (browser V8):");
  for (const f of files) {
    console.log(
      `    ${f.name.padEnd(14)} ${String(f.pct).padStart(5)}%  (${f.covered}/${f.total})`,
    );
  }
  console.log(
    `    ${"OVERALL".padEnd(14)} ${String(overallPct).padStart(5)}%  (${coveredAll}/${totalAll})\n`,
  );

  return {
    lines: { pct: overallPct, covered: coveredAll, total: totalAll },
    files,
  };
}

module.exports = { startCoverage, startAndReturnStop, stopAndReport, OUTPUT_DIR };
