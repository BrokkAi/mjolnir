---
title: Workspaces and projects
description: Organize Mjolnir sessions, compose multi-repository projects, use network-backed Git remotes, and understand persistent project memory.
---

A **workspace** groups sessions, drafts, and read state for one operator
context. A **project** is the repository or set of repositories an agent works
on. The same project can be used in several workspaces, and a workspace can
hold sessions for several projects. Persistent project memory follows the
project, independently of the workspace name.

Choose a project when starting a session; Mjolnir prepares its configuration
automatically. The configuration format calls a group of repositories a
*bundle*, but you do not need to create one separately.

## Workspaces organize the dashboard

Run `mj` to open a workspace. If none exists, Mjolnir creates one using the
current directory name, then leaves the dashboard ready for an explicit new
session. Otherwise it opens the requested workspace or the most recently
opened workspace. Use either of these forms when you want to choose explicitly:

```console
mj workspaces
mj --workspace "Release work"
```

`--workspace` matches names case-insensitively. In the terminal surface, a
bordered three-row Workspaces pane sits above Sessions. Its right-hand `☰`
button opens the workspace manager. From the keyboard, focus the Workspaces
pane, move from the tabs to `☰` with `Tab` or `Right`, then press `Enter`;
`Shift-Tab` from the Sessions pane also lands on it. The command palette also
lists **Workspaces** (`prefix+shift+n`). Selecting a tab, or
pressing an arrow while the workspace tabs have focus, changes the live-session
filter immediately. Tabs are local
views, so sessions in other workspaces continue running independently. The web
viewer shows each workspace as a separate tab. The terminal surface needs at
least 60 columns.

The manager can create, rename, and delete workspaces and recover drafts. Names
are trimmed, must be 1–64 Unicode characters, cannot contain control characters,
and are unique case-insensitively. `Release work` and `release work` therefore
name the same workspace. Deleting the last workspace leaves the manager open;
Mjolnir does not create a session automatically.

### Workspaces without a terminal

A script does not need the dashboard. `mj workspaces list` prints the
workspaces, and `mj workspaces create <name>` creates one — or selects the one
that already carries the name, so it is safe to run before every session. Both
are thin clients for `GET` and `POST /api/v1/workspaces`; see the
[HTTP API reference](/api-reference/#list-workspaces).

`mj new` does not require one at all. With no `--workspace-id` and no global
`--workspace`, it uses the instance's only workspace, or the `default` workspace
when the instance has none, so `mj -i <name> new ...` works on a brand-new
instance. An instance with several workspaces must name one.

### What belongs to a workspace

A workspace owns the active presentation of:

- live sessions and their selected order;
- per-client read frontiers;
- detached terminal drafts; and
- per-browser conversation drafts.

Read state and browser drafts are client-specific, so opening a session on your
phone does not consume another terminal's unread marker or steal its unsent
text. The daemon remains the owner of the actual sessions.

Suspended histories are global resume candidates. Resuming one moves it into the
workspace from which you resume, even if its former workspace was deleted.
This is why deleting an otherwise empty workspace does not erase stopped
session history.

### Delete safely

Ordinary deletion is allowed only when the workspace has no active sessions and
no recoverable detached drafts. Force deletion first destroys its active
sessions and drops its drafts. This is destructive session lifecycle work, not
just sidebar cleanup; review the confirmation carefully.

Suspended and otherwise inactive histories remain available in the global resume
picker after either form of workspace deletion. For session-level destruction
and recovery guarantees, see [Durability and recovery](/durability/).

Workspaces are stored in `mj.sqlite3`, not `config.toml`. Do not add a
`[workspaces]` table to the configuration file.

<a id="bundles-define-managed-projects"></a>

## Advanced: configure repositories together

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
  SSH URL.
- `local` names an absolute controller-side Git repository whose network remote
  configuration is resolved at session creation. Its local commits and working
  tree are never copied into an isolated session.

`destination` is a non-empty relative path beneath the bundle root. It cannot
contain `.` or `..`, and two destinations cannot overlap. For example, `api`
and `api/generated` cannot coexist because one would contain the other.

See the [Configuration reference](/configuration/) for every accepted source
form and validation rule.

<a id="create-a-bundle-from-the-wizard"></a>

### Choose a project in the wizard

When the terminal was opened inside a repository, selecting an isolated target
prepares that project automatically and opens review. Use **Back** to choose a
different project. The project picker offers saved projects and recent local
folders. **Choose another project…** lets you browse folders or paste a GitHub
repository link; **Next** prepares it and continues directly to review.
**Add repository** optionally combines several repositories, with the first as
primary.

In the browser, choose a project or recent local folder, use **Browse folders**,
or paste a repository link, then select **Next**. Folder browsing lists the
controller's filesystem for isolated sessions and the selected host's filesystem
for bare sessions. It does not select files on your phone or browser device.

Isolated sessions clone network remotes; review shows the source and explains
that unpublished local changes are excluded. Sessions begin at the resolved
fetch remote's default branch; `git_ref` is obsolete and is rejected with
migration guidance.

Bare runtimes work differently. A new bare session, on this machine or on an
SSH machine, selects an existing absolute Git project directory instead of a
configured bundle.
When the selected path is a primary checkout, Mjolnir creates a session-specific
clone under the repository's `.mj/clones/` tree so the primary
checkout is not used directly. Source working-tree changes stay there. Selecting an
existing linked worktree keeps that worktree. See
[Targets](/targets/#bare-runtimes).

## GitHub repositories

GitHub members clone the default branch of their configured source. For private HTTPS
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

## Local paths and network remotes

A `local` bundle member is a controller-side path used to resolve its network
fetch and push configuration. Mjolnir creates an independent target clone on
the fetch remote's advertised default branch. The target's `origin` keeps the fetch URL and the configured
push destination(s), including a separate push repository when one is set.

The source checkout is never mounted or copied into the target, and Mjolnir does
not provide a Git service backed by it. Unpublished commits, staged changes,
unstaged changes, and untracked files stay on the host. If the source has no
usable network remote, isolated creation fails before provisioning; a raw local
session is the only no-remote exception. Closing an isolated session saves its
checkpoint and does not publish a branch to the host checkout. New
network-backed sessions can resume from their saved checkpoint, including work
made after the clone.

Managed targets inherit a small allowlist of useful controller Git settings,
including identity, pull/rebase behavior, conflict style, rerere, and pruning.
Each new clone uses `push.default=current` so a normal push publishes its session
branch to the configured push destination(s). Targets do not inherit arbitrary
Git configuration or credential helpers.

## Multi-root behavior

The primary repository is the session `cwd`. Other bundle repositories are
sent through ACP as additional workspace directories, so a capable harness can
reason across the set without pretending the repositories are one Git tree.
Each repository keeps its own `.git`, origin, session branch, dirty state, and
archive material.

Muse Code ACP supports one workspace root only. Pair it with a
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
| `list` | List documents below an optional virtual path prefix, 50 entries at a time. |
| `read` | Read one document and its version token. |
| `write` | Create or replace a whole UTF-8 document using compare-and-swap. |

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

`write` replaces a complete document. To create one, the agent passes
`if_version = "new"`; to update one, it must pass the version returned by
`read`. If the document changed meanwhile, the write returns the current
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
