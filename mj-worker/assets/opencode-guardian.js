// Mjolnir's guardian hook for OpenCode 2.x.
//
// OpenCode settles its own permission rules before this hook runs, and a
// deny is final there: OpenCode's built-in agent limits (Plan's edit deny,
// the read-only Explore agent), the person's own deny rules, and Mjolnir's
// tool denials all stand. The hook only narrows what those rules allow.
//
// "session": an allowed action that can change something outside the
// workspace asks, so the Mjolnir guardian reviews it. Reads, searches,
// planning tools and edits inside the workspace run, as Codex's sandbox lets
// them run without its guardian; like Codex, workspace metadata (.git,
// .agents, .codex, .aws) stays protected, and so do OpenCode's own
// configuration and the AGENTS.md files the reviewer trusts.
//
// "reviewer": the guardian's own reviewer may only read. Anything else, or
// anything that would ask, is denied, which OpenCode reports to the model as
// a tool error instead of ending the review.
import { lstatSync, realpathSync } from "node:fs";
import path from "node:path";

const mode = __MJ_GUARDIAN_MODE__;
// The session's working directory first, then its attached directories.
const roots = __MJ_GUARDIAN_ROOTS__;

const READS = new Set(["read", "glob", "grep", "external_directory"]);
const UNREVIEWED = new Set([
  "read", "glob", "grep", "external_directory",
  "todowrite", "question", "skill",
  // A sub-agent's own tool calls pass through this hook too.
  "subagent",
]);
// Compared without case: macOS and Windows file systems ignore it, so
// `.GIT/hooks` is git's hooks directory there.
const PROTECTED_DIRECTORIES = new Set([".git", ".agents", ".codex", ".aws", ".opencode"]);
const PROTECTED_FILES = new Set(["agents.md", "opencode.json", "opencode.jsonc"]);

// Where a path really lands: the real path of its nearest existing ancestor,
// so a symbolic link inside the workspace cannot carry an edit outside it.
// `null` when that cannot be known: a link whose target does not exist yet
// still sends the write wherever it points.
function real(target) {
  const rest = [];
  let current = target;
  for (;;) {
    try {
      return path.join(realpathSync(current), ...rest.reverse());
    } catch {
      if (lstatSync(current, { throwIfNoEntry: false })?.isSymbolicLink()) return null;
      const parent = path.dirname(current);
      if (parent === current) return target;
      rest.push(path.basename(current));
      current = parent;
    }
  }
}

const realRoots = roots.map((root) => real(path.resolve(root)) ?? path.resolve(root));

function workspaceEdit(resource) {
  const target = real(path.resolve(roots[0], resource));
  if (target === null) return false;
  const root = realRoots.find((root) => target === root || target.startsWith(root + path.sep));
  if (root === undefined) return false;
  const parts = path.relative(root, target).toLowerCase().split(path.sep);
  return !parts.some((part) => PROTECTED_DIRECTORIES.has(part))
    && !PROTECTED_FILES.has(parts[parts.length - 1]);
}

export default {
  id: "mjolnir.guardian",
  async setup({ permission }) {
    const registration = await permission.hook("evaluate", async (event) => {
      if (event.effect === "deny") return;
      if (mode === "reviewer") {
        if (event.effect === "ask" || !READS.has(event.action)) event.effect = "deny";
        return;
      }
      if (event.effect !== "allow" || UNREVIEWED.has(event.action)) return;
      if (event.action === "edit" && event.resources.length > 0 && event.resources.every(workspaceEdit)) return;
      event.effect = "ask";
    });
    return () => registration.dispose();
  },
};
