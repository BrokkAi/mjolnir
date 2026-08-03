const PLATFORM_PACKAGES = Object.freeze({
  "android-arm64": Object.freeze({
    alias: "@brokkai/mjolnir-android-arm64",
    cpu: ["arm64"],
    npmTag: "android-arm64",
    os: ["android"],
    target: "aarch64-linux-android",
  }),
  "darwin-universal": Object.freeze({
    alias: "@brokkai/mjolnir-darwin-universal",
    cpu: ["arm64", "x64"],
    npmTag: "darwin-universal",
    os: ["darwin"],
    target: "universal-apple-darwin",
  }),
  "linux-arm64": Object.freeze({
    alias: "@brokkai/mjolnir-linux-arm64",
    cpu: ["arm64"],
    npmTag: "linux-arm64",
    os: ["linux"],
    target: "aarch64-unknown-linux-gnu",
  }),
  "linux-x64": Object.freeze({
    alias: "@brokkai/mjolnir-linux-x64",
    cpu: ["x64"],
    npmTag: "linux-x64",
    os: ["linux"],
    target: "x86_64-unknown-linux-gnu",
  }),
  "win32-x64": Object.freeze({
    alias: "@brokkai/mjolnir-win32-x64",
    cpu: ["x64"],
    npmTag: "win32-x64",
    os: ["win32"],
    target: "x86_64-pc-windows-msvc",
  }),
});

export function platformPackages() {
  return PLATFORM_PACKAGES;
}

export function platformPackageFor(platform, arch) {
  if (platform === "darwin" && (arch === "arm64" || arch === "x64")) {
    return PLATFORM_PACKAGES["darwin-universal"];
  }

  const key = `${platform}-${arch}`;
  const selected = PLATFORM_PACKAGES[key];
  if (selected) {
    return selected;
  }

  const supported = Object.values(PLATFORM_PACKAGES)
    .map((entry) => `${entry.os.join("/")}-${entry.cpu.join("/")}`)
    .join(", ");
  throw new Error(
    `@brokkai/mjolnir does not publish a native package for ${platform}-${arch}. ` +
      `Supported platforms: ${supported}.`,
  );
}

export function nativeBinaryName(platform) {
  return platform === "win32" ? "mj.exe" : "mj";
}
