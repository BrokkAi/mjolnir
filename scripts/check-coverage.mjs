#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";

const [reportPath, baselinePath, summaryPath] = process.argv.slice(2);
if (!reportPath || !baselinePath || !summaryPath) {
  console.error(
    "usage: node scripts/check-coverage.mjs <coverage.json> <baseline.json> <summary.md>",
  );
  process.exit(2);
}

const report = JSON.parse(fs.readFileSync(reportPath, "utf8"));
const baseline = JSON.parse(fs.readFileSync(baselinePath, "utf8"));
const data = report.data?.[0];
if (!data?.totals?.lines || !Array.isArray(data.files)) {
  throw new Error("coverage report does not contain LLVM line totals and files");
}

function projectPath(filename) {
  const normalized = filename.replaceAll("\\", "/");
  const workspace = process.cwd().replaceAll("\\", "/");
  const relative = path.posix.relative(workspace, normalized);
  if (
    relative !== "" &&
    relative !== ".." &&
    !relative.startsWith("../") &&
    !path.posix.isAbsolute(relative)
  ) {
    return relative;
  }

  // Longer roots come first: every crate directory ends in `src/`, so the
  // bare `src/` marker of the foundation crate has to be the last resort.
  for (const marker of [
    "mj-cli/src/",
    "mj-tui/src/",
    "mj-worker/src/",
    "mj-controller/src/",
    "mj-chat/src/",
    "voice-worker/src/",
    "src/",
  ]) {
    const index = normalized.lastIndexOf(`/${marker}`);
    if (index >= 0) {
      return `${marker}${normalized.slice(index + marker.length + 1)}`;
    }
  }
  return normalized;
}

const moduleLines = new Map();
for (const file of data.files) {
  const modulePath = projectPath(file.filename);
  const percent = file.summary?.lines?.percent;
  if (
    !/^(src|mj-(cli|tui|worker|controller|chat)\/src|voice-worker\/src)\/.*\.rs$/.test(
      modulePath,
    ) ||
    !Number.isFinite(percent)
  ) {
    continue;
  }
  if (moduleLines.has(modulePath)) {
    throw new Error(`coverage report contains duplicate module ${modulePath}`);
  }
  moduleLines.set(modulePath, percent);
}
const aggregate = data.totals.lines.percent;
const aggregateFloor =
  baseline.aggregate.lines - baseline.aggregate.material_regression_tolerance;
const failures = [];
if (aggregate < aggregateFloor) {
  failures.push(
    `aggregate line coverage ${aggregate.toFixed(2)}% is below ${aggregateFloor.toFixed(2)}%`,
  );
}

const rows = [];
for (const [path, actual] of [...moduleLines].sort(([a], [b]) => a.localeCompare(b))) {
  const exception = baseline.module_exceptions[path];
  const required = exception?.minimum ?? baseline.production_module_line_target;
  const passed = actual + Number.EPSILON >= required;
  if (!passed) {
    failures.push(
      `${path} line coverage ${actual.toFixed(2)}% is below ${required.toFixed(2)}%`,
    );
  }
  rows.push(
    `| \`${path}\` | ${actual.toFixed(2)}% | ${required.toFixed(2)}% | ${passed ? "pass" : "FAIL"} |`,
  );
}

for (const path of Object.keys(baseline.module_exceptions)) {
  if (!moduleLines.has(path)) {
    failures.push(`coverage exception ${path} does not match a reported module`);
  }
}

const aggregatePassed = aggregate >= aggregateFloor;
const summary = [
  "# LLVM coverage",
  "",
  `Aggregate line coverage: **${aggregate.toFixed(2)}%** (baseline ${baseline.aggregate.lines.toFixed(2)}%, regression floor ${aggregateFloor.toFixed(2)}%) — ${aggregatePassed ? "pass" : "FAIL"}.`,
  "",
  `Production module target: **${baseline.production_module_line_target.toFixed(2)}%**. Reviewed exceptions use the minimum recorded in \`${baselinePath}\`.`,
  "",
  "| Module | Lines | Required | Result |",
  "| --- | ---: | ---: | --- |",
  ...rows,
  "",
].join("\n");

fs.writeFileSync(summaryPath, summary);
if (process.env.GITHUB_STEP_SUMMARY) {
  fs.appendFileSync(process.env.GITHUB_STEP_SUMMARY, summary);
}
process.stdout.write(summary);

if (failures.length > 0) {
  console.error("\nCoverage regression check failed:");
  for (const failure of failures) {
    console.error(`- ${failure}`);
  }
  process.exit(1);
}
