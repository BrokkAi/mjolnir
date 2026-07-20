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
  "alsa-sys",
  "bzip2-sys",
  "libsqlite3-sys",
  "prettyplease",
  "ring",
  "sherpa-onnx-sys",
  "wasm-bindgen-shared",
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
  const fontDirectory = path.join(repositoryRoot, "src", "fonts");
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

async function sqliteNotice(metadata) {
  const packageInfo = resolvedPackage(metadata, "libsqlite3-sys", "0.30.1");
  const relativePath = "sqlite3/sqlite3.c";
  const source = await readFile(
    path.join(packageRoot(packageInfo), relativePath),
    "utf8",
  );
  const version = source.match(/^#define SQLITE_VERSION\s+"([^"]+)"/m)?.[1];
  const notice = source.match(
    /^\*\* The author disclaims copyright to this source code\. +In place of\n\*\* a legal notice, here is a blessing:\n\*\*\n\*\*    May you do good and not evil\.\n\*\*    May you find forgiveness for yourself and forgive others\.\n\*\*    May you share freely, never taking more than you give\./m,
  )?.[0];
  if (!version || !notice) {
    throw new Error("could not find SQLite version and public-domain notice");
  }
  const text = notice
    .split("\n")
    .map((line) => line.replace(/^\*\* ?/, ""))
    .join("\n");
  return {
    component: `SQLite ${version}`,
    source: `${packageUrl(packageInfo)} (${relativePath})`,
    scope: "compiled from the bundled SQLite amalgamation",
    text,
  };
}

async function sherpaNativePayload(metadata) {
  const packageInfo = resolvedPackage(metadata, "sherpa-onnx-sys", "1.13.3");
  const legalFiles = [
    "LICENSE",
    "licenses/native/ONNXRUNTIME_LICENSE.txt",
    "licenses/native/ONNXRUNTIME_THIRD_PARTY_NOTICES.txt",
    "licenses/native/PIPER_PHONEMIZE_LICENSE.txt",
    "licenses/native/ESPEAK_NG_BSD2.txt",
    "licenses/native/ESPEAK_NG_UCD.txt",
    "licenses/native/UNI_ALGO_LICENSE.txt",
    "licenses/native/KISSFFT_COPYING.txt",
  ];
  await Promise.all(legalFiles.map(checkedInLegalFile));
  return {
    component: `sherpa-onnx native static payload ${packageInfo.version}`,
    source: packageUrl(packageInfo),
    scope: "statically linked into mj-voice-worker on Linux, macOS, and Windows",
    text: [
      "The sherpa-onnx-sys build downloads a native archive that contains only static libraries, without legal files. The linked payload was audited against these exact upstream revisions:",
      "",
      "- sherpa-onnx v1.13.3: Apache-2.0",
      "  https://github.com/k2-fsa/sherpa-onnx/tree/v1.13.3",
      "- kaldi-decoder v0.3.0: Apache-2.0",
      "  https://github.com/k2-fsa/kaldi-decoder/tree/v0.3.0",
      "- OpenFst v1.8.5-2026-04-11: Apache-2.0; Copyright 2005-2026 Google LLC",
      "  https://github.com/csukuangfj/openfst/tree/v1.8.5-2026-04-11",
      "- kaldi-native-fbank v1.22.3: Apache-2.0",
      "  https://github.com/csukuangfj/kaldi-native-fbank/tree/v1.22.3",
      "- KissFFT febd4caeed32e33ad8b2e0bb5ea77542c40f18ec: BSD-3-Clause",
      "  https://github.com/mborgerding/kissfft/tree/febd4caeed32e33ad8b2e0bb5ea77542c40f18ec",
      "- Piper phonemize 78a788e0b719013401572d70fef372e77bff8e43: MIT; Copyright (c) 2023 Michael Hansen",
      "  https://github.com/csukuangfj/piper-phonemize/tree/78a788e0b719013401572d70fef372e77bff8e43",
      "- eSpeak NG f6fed6c58b5e0998b8e68c6610125e2d07d595a7: GPL-3.0-or-later, with BSD-2-Clause and Unicode data terms for identified portions",
      "  https://github.com/csukuangfj/espeak-ng/tree/f6fed6c58b5e0998b8e68c6610125e2d07d595a7",
      "- uni-algo incorporated by Piper phonemize: public-domain dedication",
      "- ONNX Runtime v1.24.4: MIT, plus its bundled third-party notices",
      "  https://github.com/microsoft/onnxruntime/tree/v1.24.4",
      "- simple-sentencepiece v0.7: Apache-2.0",
      "  https://github.com/pkufool/simple-sentencepiece/tree/v0.7",
      "",
      "The Apache-2.0, MIT, BSD-2-Clause, BSD-3-Clause, and GPLv3 texts are included in THIRD_PARTY_LICENSES.html or LICENSE. Exact additional terms are shipped in licenses/native/.",
    ].join("\n"),
  };
}

async function embeddedFontsNotice() {
  await checkedInLegalFile("licenses/OFL-1.1.md");
  return {
    component: "Embedded web fonts",
    source: "src/fonts/*.woff2",
    scope: "embedded in the mj binary and served by the remote viewer",
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

async function bundledAnvilNotice() {
  const version = "0.22.0";
  const workflow = await readFile(
    path.join(repositoryRoot, ".github", "workflows", "release.yml"),
    "utf8",
  );
  const pinnedVersion = workflow.match(/^  ANVIL_VERSION: "([^"]+)"$/m)?.[1];
  if (pinnedVersion !== version) {
    throw new Error(
      `Anvil release pin changed from ${version} to ${pinnedVersion ?? "<missing>"}; regenerate and review its legal bundle`,
    );
  }
  const directory = `licenses/anvil-v${version}`;
  const legalFiles = [
    "LICENSE",
    "GPL-3.0.md",
    "LGPL-3.0.md",
    "SOURCE.md",
    "THIRD_PARTY_LICENSES.html",
    "SUPPLEMENTAL_THIRD_PARTY_NOTICES.txt",
  ];
  await Promise.all(
    legalFiles.map((filename) =>
      checkedInLegalFile(path.join(directory, filename)),
    ),
  );
  return {
    component: `Bundled Anvil ${version}`,
    source: `https://github.com/BrokkAi/anvil/releases/tag/v${version}`,
    scope: "separately built binary aggregated into every official Mjolnir archive",
    text: [
      `Anvil's complete legal bundle is shipped as anvil-licenses/ and maintained in ${directory}/.`,
      "Its SOURCE.md identifies the exact source commit. The bundle includes Anvil's LGPLv3 and incorporated GPLv3 texts, the generated locked Rust dependency report, and supplemental native notices.",
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
      "bzip2-sys",
      "0.1.13+1.0.8",
      "bzip2-1.0.8/LICENSE",
      "bzip2/libbzip2 1.0.8",
      "used while unpacking sherpa-onnx native libraries during the voice-worker build",
    ),
    await sqliteNotice(metadata),
    await sherpaNativePayload(metadata),
    await embeddedFontsNotice(),
    await bundledAnvilNotice(),
  ];
  await writeFile(outputPath, render(sections), "utf8");
  process.stdout.write(`Wrote supplemental notices to ${outputPath}\n`);
}

await main();
