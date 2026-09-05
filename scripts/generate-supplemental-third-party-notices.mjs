#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { readFile, readdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repositoryRoot = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "..",
);
const outputPath = path.resolve(
  repositoryRoot,
  process.argv[2] ?? "licenses/SUPPLEMENTAL_THIRD_PARTY_NOTICES.txt",
);

// cargo-about handles license files declared by Cargo packages. This inventory
// catches standalone notices, native payloads, and embedded assets that can
// otherwise change without anyone deciding whether an artifact notice is due.
const auditedStandaloneNotices = new Set(["cfg_aliases/NOTICES.md"]);
const auditedLinksPackages = new Set([
  // aws-lc-sys compiles the vendored AWS-LC source. Its nested LICENSE is
  // byte-identical to the package LICENSE apart from trailing whitespace, so
  // cargo-about already ships the complete Apache-2.0/ISC attribution. The
  // safe Rust wrapper links that native library without another payload.
  "aws-lc-rs",
  "aws-lc-sys",
  // Desktop system-library bindings and objc2's compiled helper are covered
  // by their Cargo package licenses; they do not embed separate payloads.
  "atk-sys",
  "gdk-sys",
  "gtk-sys",
  "objc2-exception-helper",
  "webkit2gtk-sys",
  "alsa-sys",
  "libmimalloc-sys",
  "libsqlite3-sys",
  "prettyplease",
  // rayon-core uses `links` only to prevent multiple versions; its build
  // script does not link or ship a native library.
  "rayon-core",
  "ring",
  "wasm-bindgen-shared",
  // Vendored zstd has its own BSD-3-Clause license, included below.
  "zstd-sys",
]);
const auditedFonts = new Set([
  "jetbrains-mono.woff2",
  "rajdhani-500.woff2",
  "rajdhani-600.woff2",
  "rajdhani-700.woff2",
  "staatliches-400.woff2",
]);

function cargoMetadata() {
  return JSON.parse(
    execFileSync(
      "cargo",
      ["metadata", "--locked", "--offline", "--format-version", "1"],
      {
        cwd: repositoryRoot,
        encoding: "utf8",
        maxBuffer: 32 * 1024 * 1024,
      },
    ),
  );
}

function resolvedPackageIds(metadata) {
  return new Set(metadata.resolve.nodes.map(({ id }) => id));
}

function resolvedPackage(metadata, name, version) {
  const resolvedIds = resolvedPackageIds(metadata);
  const matches = metadata.packages.filter(
    (packageInfo) =>
      packageInfo.name === name &&
      (!version || packageInfo.version === version) &&
      resolvedIds.has(packageInfo.id),
  );
  if (matches.length !== 1) {
    throw new Error(
      `expected exactly one resolved ${name}${version ? `@${version}` : ""} package, found ${matches.length}`,
    );
  }
  return matches[0];
}

function checkNativePackageInventory(metadata) {
  const resolvedIds = resolvedPackageIds(metadata);
  const unknown = metadata.packages
    .filter(
      (packageInfo) =>
        resolvedIds.has(packageInfo.id) &&
        packageInfo.links &&
        !auditedLinksPackages.has(packageInfo.name),
    )
    .map(({ name, version, links }) => `${name}@${version} (links=${links})`)
    .sort();
  if (unknown.length > 0) {
    throw new Error(
      `unaudited native-linking packages in the locked graph:\n${unknown.join("\n")}`,
    );
  }
}

async function checkStandaloneNoticeInventory(metadata) {
  const resolvedIds = resolvedPackageIds(metadata);
  const discovered = [];
  for (const packageInfo of metadata.packages) {
    if (!resolvedIds.has(packageInfo.id)) {
      continue;
    }
    const filenames = await readdir(packageRoot(packageInfo));
    for (const filename of filenames) {
      if (/^NOTICES?(?:\..*)?$/i.test(filename)) {
        discovered.push(`${packageInfo.name}/${filename}`);
      }
    }
  }
  const unknown = discovered
    .filter((notice) => !auditedStandaloneNotices.has(notice))
    .sort();
  if (unknown.length > 0) {
    throw new Error(
      `unaudited standalone notice files in the locked graph:\n${unknown.join("\n")}`,
    );
  }
}

async function checkFontInventory() {
  const fontDirectory = path.join(repositoryRoot, "mj-controller", "src", "fonts");
  const discovered = (await readdir(fontDirectory))
    .filter((filename) => filename.endsWith(".woff2"))
    .sort();
  const expected = [...auditedFonts].sort();
  if (JSON.stringify(discovered) !== JSON.stringify(expected)) {
    throw new Error(
      `embedded font inventory changed:\nexpected ${expected.join(", ")}\nfound ${discovered.join(", ")}`,
    );
  }
}

function packageRoot(packageInfo) {
  return path.dirname(packageInfo.manifest_path);
}

function packageUrl(packageInfo) {
  return `https://crates.io/crates/${packageInfo.name}/${encodeURIComponent(packageInfo.version)}`;
}

async function legalFile(metadata, name, version, relativePath, component, scope) {
  const packageInfo = resolvedPackage(metadata, name, version);
  const text = (
    await readFile(path.join(packageRoot(packageInfo), relativePath), "utf8")
  ).trimEnd();
  if (!text) {
    throw new Error(`${name}/${relativePath} is empty`);
  }
  return {
    component,
    source: `${packageUrl(packageInfo)} (${relativePath})`,
    scope,
    text,
  };
}

async function checkedInLegalFile(relativePath) {
  const text = (await readFile(path.join(repositoryRoot, relativePath), "utf8"))
    .trimEnd();
  if (!text) {
    throw new Error(`${relativePath} is empty`);
  }
  return relativePath;
}


async function sqliteNativePayload(metadata) {
  const packageInfo = resolvedPackage(metadata, "libsqlite3-sys", "0.35.0");
  const amalgamation = await readFile(
    path.join(packageRoot(packageInfo), "sqlite3", "sqlite3.c"),
    "utf8",
  );
  const sqliteVersion = "3.50.2";
  if (!amalgamation.includes(`SQLite\n** version ${sqliteVersion}`)) {
    throw new Error(
      `libsqlite3-sys does not contain the audited SQLite ${sqliteVersion} amalgamation`,
    );
  }
  return {
    component: `SQLite ${sqliteVersion} amalgamation`,
    source: `${packageUrl(packageInfo)} (sqlite3/sqlite3.c)`,
    scope: "statically linked into Mjolnir by libsqlite3-sys with its bundled feature",
    text: [
      "SQLite is dedicated to the public domain.",
      "https://www.sqlite.org/copyright.html",
    ].join("\n"),
  };
}

async function embeddedFontsNotice() {
  await checkedInLegalFile("licenses/OFL-1.1.md");
  return {
    component: "Embedded web fonts",
    source: "mj-controller/src/fonts/*.woff2",
    scope: "embedded in the Mjolnir binary and served by the remote viewer",
    text: [
      "The following font software is licensed under the SIL Open Font License 1.1. The complete terms are in OFL-1.1.md.",
      "",
      "- Rajdhani 500, 600, and 700: Copyright (c) 2014, Indian Type Foundry (info@indiantypefoundry.com).",
      "  https://github.com/google/fonts/tree/main/ofl/rajdhani",
      "- Staatliches 400: Copyright 2018 The Staatliches Authors (https://github.com/googlefonts/staatliches)",
      "  https://github.com/google/fonts/tree/main/ofl/staatliches",
      "- JetBrains Mono: Copyright 2020 The JetBrains Mono Project Authors (https://github.com/JetBrains/JetBrainsMono)",
      "  https://github.com/JetBrains/JetBrainsMono",
    ].join("\n"),
  };
}

function render(sections) {
  const lines = [
    "MJOLNIR SUPPLEMENTAL THIRD-PARTY NOTICES",
    "",
    "This file supplements THIRD_PARTY_LICENSES.html. Cargo package metadata",
    "does not enumerate standalone NOTICE files, embedded web fonts, or every",
    "license in native source and prebuilt static-library payloads.",
    "",
    "The sections below are generated from Cargo.lock, exact installed crate",
    "sources, and a reviewed inventory of the non-Cargo assets shipped by",
    "official Mjolnir archives.",
  ];

  for (const section of sections) {
    lines.push(
      "",
      "=".repeat(80),
      section.component,
      "=".repeat(80),
      "",
      `Source: ${section.source}`,
      `Inclusion: ${section.scope}`,
      "",
      section.text,
    );
  }
  return `${lines.join("\n")}\n`;
}

async function main() {
  const metadata = cargoMetadata();
  checkNativePackageInventory(metadata);
  await checkStandaloneNoticeInventory(metadata);
  await checkFontInventory();
  const sections = [
    await legalFile(
      metadata,
      "cfg_aliases",
      "0.2.1",
      "NOTICES.md",
      "cfg_aliases third-party attribution",
      "used while compiling target-specific dependency configuration",
    ),
    await legalFile(
      metadata,
      "libmimalloc-sys",
      "0.1.49",
      "c_src/mimalloc/v3/LICENSE",
      "mimalloc native allocator",
      "statically linked into the musl Linux Mjolnir controller and worker",
    ),
    await legalFile(
      metadata,
      "zstd-sys",
      "2.0.16+zstd.1.5.7",
      "zstd/LICENSE",
      "Zstandard native compression library",
      "statically linked for checkpoint archive compression",
    ),
    await sqliteNativePayload(metadata),
    await embeddedFontsNotice(),
  ];
  await writeFile(outputPath, render(sections), "utf8");
  process.stdout.write(`Wrote supplemental notices to ${outputPath}\n`);
}

await main();
