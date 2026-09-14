import { readFile, writeFile } from "node:fs/promises";
import path from "node:path";

const root = process.argv[2];
if (!root) throw new Error("usage: patch-adapter.mjs <adapter installation path>");

async function replace(relative, before, after) {
  const file = path.join(root, relative);
  const source = await readFile(file, "utf8");
  if (!source.includes(before)) {
    throw new Error(`zcode-acp compatibility patch no longer applies to ${relative}`);
  }
  await writeFile(file, source.replace(before, after));
}

await replace(
  "dist/utils.js",
  "export const ZCODE_CREDS_PATH = path.join(process.env.HOME || process.env.USERPROFILE || \"~\", \".zcode\", \"v2\", \"config.json\");",
  "export const ZCODE_CREDS_PATH = path.join(process.env.ZCODE_HOME || path.join(process.env.HOME || process.env.USERPROFILE || \"~\", \".zcode\"), \"v2\", \"config.json\");",
);

await replace(
  "dist/backend/credentials.js",
  "for (const [, p] of Object.entries(cfg.provider ?? {})) {\n            if (p?.enabled) {",
  "for (const [providerId, p] of Object.entries(cfg.provider ?? {})) {\n            if (p?.enabled && (!process.env.ZCODE_PROVIDER || providerId === process.env.ZCODE_PROVIDER)) {",
);

await replace(
  "dist/config/options.js",
  "for (const [pid, p] of Object.entries(cfg.provider ?? {})) {\n            if (!providerSelectable(pid, p))",
  "for (const [pid, p] of Object.entries(cfg.provider ?? {})) {\n            if (process.env.ZCODE_PROVIDER && pid !== process.env.ZCODE_PROVIDER) continue;\n            if (!providerSelectable(pid, p))",
);

await replace(
  "dist/handlers/session.js",
  "mode: \"yolo\",",
  "mode: process.env.ZCODE_ACP_MODE || \"build\",",
);

await replace(
  "dist/backend/resolve.js",
  "return [nodeBin, zcodeBin, \"app-server\", \"--stdio\"];",
  "return [nodeBin, zcodeBin, \"app-server\", \"--stdio\", ...(process.env.ZCODE_DISALLOWED_TOOLS ? [\"--disallowed-tools\", process.env.ZCODE_DISALLOWED_TOOLS] : [])];",
);

await replace(
  "dist/lazy-sessions.js",
  "const home = process.env.HOME || process.env.USERPROFILE || \"~\";\n    return path.join(home, \".zcode\", \"v2\", STORE_FILENAME);",
  "const zcodeHome = process.env.ZCODE_HOME;\n    return zcodeHome ? path.join(zcodeHome, \"v2\", STORE_FILENAME) : path.join(process.env.HOME || process.env.USERPROFILE || \"~\", \".zcode\", \"v2\", STORE_FILENAME);",
);

await replace(
  "dist/handlers/session.js",
  "server.registerSession(acpSid, sid);\n        // session/create loads the session into this backend process.",
  "server.registerSession(acpSid, sid);\n        if (process.env.ZCODE_PROVIDER && process.env.ZCODE_MODEL) {\n            const { applyModelSwitch } = await import(\"../config/runtime-model.js\");\n            const selected = `${process.env.ZCODE_PROVIDER}\\\\${process.env.ZCODE_MODEL}`;\n            if (!(await applyModelSwitch(server, sid, selected))) throw new Error(`zcode refused configured model ${selected}`);\n        }\n        // session/create loads the session into this backend process.",
);
