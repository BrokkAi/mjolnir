// Port of mj-worker/assets: the OpenCode plugin scripts the worker installs.
//
// The Rust crate `include_str!`s these from worker_runtime/json_gh_rules.rs
// (opencode-guardian.js) and worker_runtime/subagents.rs
// (opencode-mailbox.js). The copies under data/assets/ must stay identical to
// mj-worker/assets/.

package worker

import _ "embed"

// opencodeGuardianScript is mj-worker/assets/opencode-guardian.js: the
// OpenCode plugin that routes tool calls through the Mjolnir guardian. The
// installer replaces `__MJ_GUARDIAN_MODE__` and `__MJ_GUARDIAN_ROOTS__`.
//
//go:embed data/assets/opencode-guardian.js
var opencodeGuardianScript string

// opencodeMailboxScript is mj-worker/assets/opencode-mailbox.js: the
// OpenCode plugin that drains the session mailbox through the worker. The
// installer replaces `__MJ_WORKER__` and `__MJ_SOCKET__`.
//
//go:embed data/assets/opencode-mailbox.js
var opencodeMailboxScript string
