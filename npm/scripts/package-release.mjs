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

export const PLATFORMS = [
  {
    packageName: "@brokkai/mjolnir-darwin-universal",
    target: "universal-apple-darwin",
    extension: ".tar.gz",
    binary: "mj",
    desktop: true,
  },
  {
    packageName: "@brokkai/mjolnir-linux-x64-gnu",
    target: "x86_64-unknown-linux-gnu",
    extension: ".tar.gz",
    binary: "mj",
    desktop: true,
  },
  {
    packageName: "@brokkai/mjolnir-linux-arm64-gnu",
    target: "aarch64-unknown-linux-gnu",
    extension: ".tar.gz",
    binary: "mj",
    desktop: true,
  },
  {
    packageName: "@brokkai/mjolnir-android-arm64",
    target: "aarch64-linux-android",
    extension: ".tar.gz",
    binary: "mj",
    desktop: false,
  },
  {
    packageName: "@brokkai/mjolnir-win32-x64",
    target: "x86_64-pc-windows-msvc",
    extension: ".zip",
    binary: "mj.exe",
    desktop: true,
  },
];

export async function cargoVersion() {
  const manifest = await readFile(path.join(repositoryRoot, "Cargo.toml"), "utf8");
  const version = manifest.match(/^version = "([^"]+)"$/m)?.[1];
  if (!version) throw new Error("could not read the root Cargo.toml version");
  return version;
}

async function readJson(filename) {
  return JSON.parse(await readFile(filename, "utf8"));
}

async function writeJson(filename, value) {
  await writeFile(filename, `${JSON.stringify(value, null, 2)}\n`);
}

export async function syncVersions(version) {
  const releaseVersion = version ?? (await cargoVersion());
  const rootManifestPath = path.join(npmRoot, "package.json");
  const rootManifest = await readJson(rootManifestPath);
  rootManifest.version = releaseVersion;
  await writeJson(rootManifestPath, rootManifest);

  for (const platform of PLATFORMS) {
    const manifestPath = path.join(npmRoot, "packages", packageDirectory(platform), "package.json");
    const manifest = await readJson(manifestPath);
    manifest.version = releaseVersion;
    await writeJson(manifestPath, manifest);
  }

  const wrapperManifestPath = path.join(npmRoot, "packages", "mjolnir", "package.json");
  const wrapperManifest = await readJson(wrapperManifestPath);
  wrapperManifest.version = releaseVersion;
  wrapperManifest.optionalDependencies = Object.fromEntries(
    PLATFORMS.map((platform) => [platform.packageName, releaseVersion]),
  );
  await writeJson(wrapperManifestPath, wrapperManifest);
}

function packageDirectory(platform) {
  return platform.packageName.split("/").at(-1);
}

function archiveName(version, platform) {
  return `brokk-mjolnir-v${version}-${platform.target}${platform.extension}`;
}

async function sha256(filename) {
  const contents = await readFile(filename);
  return createHash("sha256").update(contents).digest("hex");
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

async function copyReleaseBundle(platform, source) {
  const destination = path.join(npmRoot, "packages", packageDirectory(platform));
  const bundleFiles = ["bin", "README.md", "LICENSE", "licenses", "anvil-licenses"];
  await Promise.all(bundleFiles.map((file) => rm(path.join(destination, file), { recursive: true, force: true })));
  await cp(path.join(source, "README.md"), path.join(destination, "README.md"));
  await cp(path.join(source, "LICENSE"), path.join(destination, "LICENSE"));
  await cp(path.join(source, "licenses"), path.join(destination, "licenses"), { recursive: true });
  await cp(path.join(source, "anvil-licenses"), path.join(destination, "anvil-licenses"), { recursive: true });
  await mkdir(path.join(destination, "bin"), { recursive: true });
  await cp(path.join(source, platform.binary), path.join(destination, "bin", platform.binary), {
    recursive: true,
    force: true,
  });
  await cp(
    path.join(source, platform.binary === "mj.exe" ? "anvil.exe" : "anvil"),
    path.join(destination, "bin", platform.binary === "mj.exe" ? "anvil.exe" : "anvil"),
  );
  if (platform.desktop) {
    const worker = platform.binary === "mj.exe" ? "mj-voice-worker.exe" : "mj-voice-worker";
    await cp(path.join(source, worker), path.join(destination, "bin", worker));
    await ensureBinary(path.join(destination, "bin", worker), platform.binary !== "mj.exe");
  }
  await ensureBinary(path.join(destination, "bin", platform.binary), platform.binary !== "mj.exe");
  await ensureBinary(
    path.join(destination, "bin", platform.binary === "mj.exe" ? "anvil.exe" : "anvil"),
    platform.binary !== "mj.exe",
  );
}

export async function packageRelease({ releaseTag, assetsDirectory }) {
  const version = await cargoVersion();
  if (releaseTag !== `v${version}`) {
    throw new Error(`release tag ${releaseTag} does not match Cargo.toml version v${version}`);
  }
  await syncVersions(version);
  const temporaryRoot = await mkdtemp(path.join(os.tmpdir(), "mjolnir-npm-"));
  try {
    for (const platform of PLATFORMS) {
      const archive = path.join(assetsDirectory, archiveName(version, platform));
      await verifyChecksum(archive);
      const extractDirectory = path.join(temporaryRoot, packageDirectory(platform));
      await mkdir(extractDirectory, { recursive: true });
      const bundle = await extractArchive(archive, extractDirectory);
      await copyReleaseBundle(platform, bundle);
    }
  } finally {
    await rm(temporaryRoot, { recursive: true, force: true });
  }
}

function usage() {
  return "Usage: node scripts/package-release.mjs --sync-versions | --release-tag vX.Y.Z --assets DIRECTORY";
}

async function main() {
  const args = process.argv.slice(2);
  if (args.includes("--sync-versions")) {
    await syncVersions();
    return;
  }
  const releaseTag = args[args.indexOf("--release-tag") + 1];
  const assetsDirectory = args[args.indexOf("--assets") + 1];
  if (!releaseTag || !assetsDirectory) throw new Error(usage());
  await packageRelease({ releaseTag, assetsDirectory: path.resolve(assetsDirectory) });
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  main().catch((error) => {
    console.error(`npm packaging: ${error.message}`);
    process.exitCode = 1;
  });
}
