---
title: Workspaces and bundles
description: Organize Mjolnir sessions, compose multi-repository projects, bridge local Git repositories, and understand persistent project memory.
---

Workspaces and bundles solve different problems:

- A **workspace** is a named control-plane view. It groups live sessions,
  drafts, and read state for one operator context.
- A **bundle** is a configured project shape. It tells a managed target which
  repositories to check out and which one is the agent's primary working
  directory.

They are deliberately independent. The same bundle can be used by sessions in
several workspaces, and a workspace can hold sessions from several bundles.
Persistent project memory follows the project identity, not the workspace name.

## Workspaces organize the dashboard

Run `mj` to open a workspace. If there is one usable workspace, Mjolnir selects
it automatically; with several, it opens the workspace picker. Use either of
these forms when you want to choose explicitly:

```console
mj workspaces
mj --workspace "Release work"
```

`--workspace` matches names case-insensitively. In the terminal surface, `F3`
opens the same picker from anywhere. The web viewer shows each workspace as a
separate tab.

The picker can create, rename, and delete workspaces. Names are trimmed, must be
1–64 Unicode characters, cannot contain control characters, and are unique
case-insensitively. `Release work` and `release work` therefore name the same
workspace.

### What belongs to a workspace

A workspace owns the active presentation of:

- live sessions and their selected order;
- per-client read frontiers;
- detached terminal drafts; and
- per-browser conversation drafts.

Read state and browser drafts are client-specific, so opening a session on your
phone does not consume another terminal's unread marker or steal its unsent
text. The daemon remains the owner of the actual sessions.

Stopped histories are global resume candidates. Resuming one moves it into the
workspace from which you resume, even if its former workspace was deleted.
This is why deleting an otherwise empty workspace does not erase stopped
session history.

### Delete safely

Ordinary deletion is allowed only when the workspace has no active sessions and
no recoverable detached drafts. Force deletion first destroys its active
sessions and drops its drafts. This is destructive session lifecycle work, not
just sidebar cleanup; review the confirmation carefully.

Stopped and otherwise inactive histories remain available in the global resume
picker after either form of workspace deletion. For session-level destruction
and recovery guarantees, see [Durability and recovery](/durability/).

Workspaces are stored in `mj.sqlite3`, not `config.toml`. Do not add a
`[workspaces]` table to the configuration file.

## Bundles define managed projects

Container, SSH Podman, and EC2 targets start from a bundle. A bundle can contain
one repository or assemble several repositories into a virtual monorepo. Its
`primary_repo` becomes the ACP session working directory; every other member is
provided to the harness as an additional workspace root.

```toml
[bundles.product]
primary_repo = "app"

[[bundles.product.repositories]]
id = "app"
github = "acme/app"
destination = "app"
git_ref = "main"

[[bundles.product.repositories]]
id = "shared"
local = "/home/me/src/shared"
destination = "shared"
```

This produces a target workspace conceptually like:

```text
<workspace-root>/
├── app/       ← primary working directory
└── shared/    ← additional workspace root
```

Bundle and repository IDs use 1–64 ASCII letters, digits, `.`, `-`, or `_`;
`.` and `..` are invalid. Repository IDs must be unique within the bundle, and
`primary_repo` must name one of them.

Each repository declares exactly one source:

- `github` accepts `owner/repository`, a GitHub HTTPS URL, or a supported GitHub
  SSH URL. An optional non-blank `git_ref` selects a branch, tag, or commit.
- `local` names an absolute controller-side Git repository. It cannot be
  combined with `git_ref`.

`destination` is a non-empty relative path beneath the bundle root. It cannot
contain `.` or `..`, and two destinations cannot overlap. For example, `api`
and `api/generated` cannot coexist because one would contain the other.

See the [Configuration reference](/configuration/) for every accepted source
form and validation rule.

### Create a bundle from the wizard

The terminal new-session wizard can quick-add a simple bundle from an existing
local repository path or a GitHub `owner/repository`/URL. Edit `config.toml`
when you need a stable multi-repository layout, a particular `git_ref`, or a
different primary repository.

Bare targets work differently. A new `local-bare` or `ssh-bare` session selects
an existing absolute Git project directory instead of a configured bundle.
When the selected path is a primary checkout, Mjolnir creates a session-specific
linked worktree under the repository's `.mj/worktrees/` tree so the primary
checkout is not used directly. Before it can do that, the primary checkout must
be completely clean: no staged, unstaged, or untracked files. Selecting an
existing linked worktree keeps that worktree. See
[Targets](/targets/#bare-targets).

## GitHub repositories

GitHub members clone using their configured source and ref. For private HTTPS
repositories, Mjolnir looks for a token in `GH_TOKEN`, then `GITHUB_TOKEN`, then
the authenticated GitHub CLI. The active token is injected into managed
non-local sessions and kept out of checkpoints and recovery archives.

Local Podman, Docker, SSH Podman, and Apple Container targets maintain a
read-only Git object cache on the container host under
`~/.cache/mjolnir/git`. Mjolnir refreshes a bare mirror, makes an isolated
per-session snapshot with hardlinked immutable objects, and lets the target
clone borrow from it. If cache setup is unavailable, the launch falls back to a
normal network clone.

The cache can contain objects from private repositories and is created with
user-only permissions. Unused mirrors are pruned after 30 days and the mirror
set has a 20 GiB least-recently-used soft cap. Session snapshots are removed
with their containers.

## Local repositories and the Git bridge

A `local` bundle member is not mounted writable into a target. Instead, Mjolnir
starts a per-session Git protocol bridge over the session's existing transport.
The target sees that bridge as its `origin`.

This design provides:

- normal `git fetch` from the controller-side repository;
- fast-forward `git push origin` back to it;
- no inbound listening port;
- no SSH-key copy; and
- no general writable mount of your source checkout.

Force pushes, ref deletion, and receive hooks are disabled. A push to the
controller repository's currently checked-out branch is rejected while that
checkout is dirty. Git LFS is not supported through the bridge.

At initial launch Mjolnir can seed the target from the local repository's
current branch and uncommitted state. Because this copies work the agent can
change independently, the new-session flow requires explicit acknowledgment
when a local repository is dirty. The session's checkpoint then becomes the
durable source for later resumes rather than reseeding the user's checkout.

Managed targets inherit a small allowlist of useful controller Git settings,
including identity, pull/rebase behavior, conflict style, rerere, pruning, and
push defaults. They do not inherit arbitrary Git configuration or credential
helpers.

## Multi-root behavior

The primary repository is the session `cwd`. Other bundle repositories are
sent through ACP as additional workspace directories, so a capable harness can
reason across the set without pretending the repositories are one Git tree.
Each repository keeps its own `.git`, origin, ref, dirty state, and archive
material.

DeepSeek Harness ACP supports one workspace root only. Pair it with a
single-repository bundle or one bare project directory, and do not add attached
directories. The other four supported harnesses accept multi-root bundles.

Attached directories are not bundle members. They are per-session supplemental
resources selected in the creation or resume wizard, do not acquire Git origins,
and have target-specific copy-on-write or snapshot behavior. See
[Session lifecycle](/sessions/) and [Targets](/targets/).

## Persistent project memory

Mjolnir gives new sessions a small persistent knowledge store scoped to the
project. It is shared across harnesses and sessions that resolve to the same
project identity:

- a GitHub repository is identified by lowercased owner and repository;
- a local project is identified by its canonical repository root;
- a raw remote project includes its target ID and canonical remote path; and
- a bundle is identified by its primary repository and member set, independent
  of member order.

The workspace name is not part of that identity. Two workspaces using the same
project therefore share project memory; two different bundles do not merely
because their display names happen to match.

### What the agent sees

The Mjolnir project-memory service provides three tools:

| Tool | Purpose |
| --- | --- |
| `memory_list` | List documents below an optional virtual path prefix, 50 entries at a time. |
| `memory_read` | Read one document and its version token. |
| `memory_write` | Create or replace a whole UTF-8 document using compare-and-swap. |

Virtual paths start at `/`; target and controller filesystem paths never cross
the tool boundary. `/MEMORY.md` is the concise index automatically supplied as
hidden startup context. New sessions receive at most its first 200 lines or
25 KiB, so keep it short and link to focused documents elsewhere in memory.

Delivery is harness-specific. Claude Code uses its native project-memory
integration; Kimi managed targets receive the service through their staged MCP
configuration; the remaining supported paths receive it through ACP.

For a multi-root bundle, bundle-wide material lives at the virtual root and
repository-specific material may live below `/roots/<repository-id>/`.
The startup context also tells the agent which repository ID maps to each
workspace root.

### Writes, limits, and conflicts

`memory_write` replaces a complete document. To create one, the agent passes
`if_version = "new"`; to update one, it must pass the version returned by
`memory_read`. If the document changed meanwhile, the write returns the current
version and content instead of overwriting it.

Project memory has these limits:

- 100 KiB per document;
- 1 MiB per synchronized snapshot;
- 1024 bytes per virtual path; and
- 50 entries per listing page.

Empty documents, unsafe path components, hidden/reserved path segments, and
symbolic-link traversal are rejected. There is no delete operation.

Every session works on a private replica. At explicit durable/checkpoint
boundaries, Mjolnir reconciles that replica with the canonical controller copy.
Edits to different files merge. If two sessions change the same file from the
same baseline, the controller's current version remains at the original path
and the other version is preserved under
`/conflicts/<session>-<digest>.md`. Nothing is silently discarded.

The canonical copy lives below:

```text
<MJ_DATA_DIR or platform data directory>/projects/<project-key>/memory/
```

Memory is background context, not authoritative project state. Agents are told
to verify it against the working tree. Do not store credentials or other
secrets there; it is deliberately available to every session for that project
and participates in checkpoint reconciliation.

## Choose the right scope

Use a workspace for “which sessions and drafts do I want in this view?” Use a
bundle for “which repositories make up this project?” Use project memory for
small durable facts the agent should carry between sessions. Use the repository
itself for source code, design documents, and anything that belongs under
version control.
