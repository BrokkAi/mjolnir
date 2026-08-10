import { createHash } from "node:crypto";
import { execFile as execFileCallback } from "node:child_process";
import { cp, mkdir, mkdtemp, readFile, readdir, rm, stat, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";

const execFile = promisify(execFileCallback);
const npmRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const repositoryRoot = path.resolve(npmRoot, "..");

export const ROOT_PACKAGE = "@brokkai/mjolnir";

export const PLATFORMS = [
  {
    packageName: "@brokkai/mjolnir-darwin-universal",
    target: "universal-apple-darwin",
    extension: ".tar.gz",
    binary: "mj",
    desktop: true,
    description: "Native universal macOS bundle for @brokkai/mjolnir",
    os: ["darwin"],
    cpu: ["x64", "arm64"],
  },
  {
    packageName: "@brokkai/mjolnir-linux-x64-gnu",
    target: "x86_64-unknown-linux-gnu",
    extension: ".tar.gz",
    binary: "mj",
    desktop: true,
    description: "Native Linux x64 glibc bundle for @brokkai/mjolnir",
    os: ["linux"],
    cpu: ["x64"],
    libc: ["glibc"],
  },
  {
    packageName: "@brokkai/mjolnir-linux-arm64-gnu",
    target: "aarch64-unknown-linux-gnu",
    extension: ".tar.gz",
    binary: "mj",
    desktop: true,
    description: "Native Linux ARM64 glibc bundle for @brokkai/mjolnir",
    os: ["linux"],
    cpu: ["arm64"],
    libc: ["glibc"],
  },
  {
    packageName: "@brokkai/mjolnir-android-arm64",
    target: "aarch64-linux-android",
    extension: ".tar.gz",
    binary: "mj",
    desktop: false,
    description: "Native Android ARM64 bundle for @brokkai/mjolnir",
    os: ["android"],
    cpu: ["arm64"],
  },
  {
    packageName: "@brokkai/mjolnir-win32-x64",
    target: "x86_64-pc-windows-msvc",
    extension: ".zip",
    binary: "mj.exe",
    desktop: true,
    description: "Native Windows x64 bundle for @brokkai/mjolnir",
    os: ["win32"],
    cpu: ["x64"],
  },
];

export function versionFromTag(tag) {
  const match = /^v(\d+\.\d+\.\d+)$/.exec(tag);
  if (!match) throw new Error(`release tag must look like vX.Y.Z, got: ${tag}`);
  return match[1];
}

export async function cargoVersion() {
  const manifest = await readFile(path.join(repositoryRoot, "Cargo.toml"), "utf8");
  // The release version lives in [workspace.package]; both crates inherit it.
  const version = manifest.match(/^\[workspace\.package\]$[^[]*^version = "([^"]+)"$/m)?.[1];
  if (!version) throw new Error("could not read the [workspace.package] version from Cargo.toml");
  return version;
}

function baseManifest(version) {
  return {
    version,
    license: "GPL-3.0-only",
    repository: "https://github.com/BrokkAi/mjolnir",
    homepage: "https://mjolnir.brokk.ai/",
    bugs: "https://github.com/BrokkAi/mjolnir/issues",
    publishConfig: { access: "public" },
  };
}

export function platformManifest(platform, version) {
  return {
    name: platform.packageName,
    ...baseManifest(version),
    description: platform.description,
    os: platform.os,
    cpu: platform.cpu,
    ...(platform.libc ? { libc: platform.libc } : {}),
    files: ["bin/", "README.md", "LICENSE", "licenses/"],
  };
}

export function rootManifest(version) {
  return {
    name: ROOT_PACKAGE,
    ...baseManifest(version),
    description: "Mjolnir terminal client for Agent Client Protocol servers",
    type: "module",
    bin: { mj: "bin/mj.js" },
    files: ["bin/", "README.md", "LICENSE"],
    optionalDependencies: Object.fromEntries(
      PLATFORMS.map((platform) => [platform.packageName, version]),
    ),
    engines: { node: ">=18" },
  };
}

function packageDirectory(packageName) {
  return packageName.replace("@", "").replace("/", "-");
}

function archiveName(version, platform) {
  return `brokk-mjolnir-v${version}-${platform.target}${platform.extension}`;
}

async function writeManifest(directory, manifest) {
  await writeFile(path.join(directory, "package.json"), `${JSON.stringify(manifest, null, 2)}\n`);
}

async function sha256(filename) {
  return createHash("sha256").update(await readFile(filename)).digest("hex");
}

async function verifyChecksum(filename) {
  const checksum = (await readFile(`${filename}.sha256`, "utf8")).trim().split(/\s+/)[0];
  if (!/^[a-f0-9]{64}$/i.test(checksum)) {
    throw new Error(`invalid SHA-256 sidecar for ${path.basename(filename)}`);
  }
  const actual = await sha256(filename);
  if (actual !== checksum.toLowerCase()) {
    throw new Error(`checksum mismatch for ${path.basename(filename)}: expected ${checksum}, got ${actual}`);
  }
}

async function extractArchive(filename, destination) {
  if (filename.endsWith(".zip")) {
    await execFile("unzip", ["-q", filename, "-d", destination]);
  } else {
    await execFile("tar", ["-xzf", filename, "-C", destination]);
  }
  const entries = await readdir(destination, { withFileTypes: true });
  const roots = entries.filter((entry) => entry.isDirectory());
  if (roots.length !== 1) {
    throw new Error(`${path.basename(filename)} must contain exactly one top-level directory`);
  }
  return path.join(destination, roots[0].name);
}

async function ensureBinary(filename, requireExecutableBit) {
  const metadata = await stat(filename);
  if (metadata.size === 0) throw new Error(`${filename} is empty`);
  if (requireExecutableBit && (metadata.mode & 0o111) === 0) {
    throw new Error(`${filename} is not executable`);
  }
}

async function stagePlatform(platform, version, source, stagingRoot) {
  const destination = path.join(stagingRoot, packageDirectory(platform.packageName));
  await mkdir(path.join(destination, "bin"), { recursive: true });
  for (const entry of ["README.md", "LICENSE", "licenses"]) {
    await cp(path.join(source, entry), path.join(destination, entry), { recursive: true });
  }
  await cp(path.join(source, platform.binary), path.join(destination, "bin", platform.binary));
  if (platform.desktop) {
    const worker = platform.binary === "mj.exe" ? "mj-voice-worker.exe" : "mj-voice-worker";
    await cp(path.join(source, worker), path.join(destination, "bin", worker));
    await ensureBinary(path.join(destination, "bin", worker), platform.binary !== "mj.exe");
  }
  await ensureBinary(path.join(destination, "bin", platform.binary), platform.binary !== "mj.exe");
  await writeManifest(destination, platformManifest(platform, version));
  return destination;
}

async function stageRoot(version, stagingRoot) {
  const destination = path.join(stagingRoot, packageDirectory(ROOT_PACKAGE));
  await mkdir(path.join(destination, "bin"), { recursive: true });
  await cp(path.join(npmRoot, "launcher", "mj.js"), path.join(destination, "bin", "mj.js"));
  await cp(path.join(npmRoot, "launcher", "README.md"), path.join(destination, "README.md"));
  await cp(path.join(repositoryRoot, "LICENSE"), path.join(destination, "LICENSE"));
  await writeManifest(destination, rootManifest(version));
  return destination;
}

async function pack(directory, outputDirectory) {
  const { stdout } = await execFile("npm", ["pack", directory, "--pack-destination", outputDirectory, "--silent"]);
  return path.join(outputDirectory, stdout.trim().split("\n").at(-1));
}

export async function packageRelease({ releaseTag, assetsDirectory, outputDirectory }) {
  const version = versionFromTag(releaseTag);
  const manifestVersion = await cargoVersion();
  if (version !== manifestVersion) {
    throw new Error(`release tag ${releaseTag} does not match Cargo.toml version v${manifestVersion}`);
  }
  const output = path.resolve(outputDirectory ?? path.join(npmRoot, "dist"));
  await rm(output, { recursive: true, force: true });
  await mkdir(output, { recursive: true });
  const temporaryRoot = await mkdtemp(path.join(os.tmpdir(), "mjolnir-npm-"));
  try {
    for (const platform of PLATFORMS) {
      const archive = path.join(assetsDirectory, archiveName(version, platform));
      await verifyChecksum(archive);
      const extractDirectory = path.join(temporaryRoot, `extract-${packageDirectory(platform.packageName)}`);
      await mkdir(extractDirectory, { recursive: true });
      const bundle = await extractArchive(archive, extractDirectory);
      await pack(await stagePlatform(platform, version, bundle, temporaryRoot), output);
    }
    await pack(await stageRoot(version, temporaryRoot), output);
  } finally {
    await rm(temporaryRoot, { recursive: true, force: true });
  }
  return output;
}

function usage() {
  return "Usage: node scripts/package-release.mjs --release-tag vX.Y.Z --assets DIRECTORY [--out DIRECTORY]";
}

async function main() {
  const args = process.argv.slice(2);
  const releaseTag = args[args.indexOf("--release-tag") + 1];
  const assetsDirectory = args[args.indexOf("--assets") + 1];
  const outputArgument = args.includes("--out") ? args[args.indexOf("--out") + 1] : undefined;
  if (!releaseTag || !assetsDirectory) throw new Error(usage());
  await packageRelease({
    releaseTag,
    assetsDirectory: path.resolve(assetsDirectory),
    outputDirectory: outputArgument ? path.resolve(outputArgument) : undefined,
  });
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  main().catch((error) => {
    console.error(`npm packaging: ${error.message}`);
    process.exitCode = 1;
  });
}
