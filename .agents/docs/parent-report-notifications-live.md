# Manual parent report notification check

This paid, live-harness check uses one named isolated instance and separate
configuration and data directories under `/home/jonathan/.cache/mjolnir/build`.
It never reads Codex or Claude native conversation files. Use the mj transcript,
events, session records, and isolated database as evidence.
Keep `[github.watch] enabled = true` for this run. The e2e driver's optional
`--no-github-events` flag only keeps the session off the GitHub watcher;
durable child report events and mailbox hooks remain active.

Set the build paths, harness, and model. The model defaults below match the
Codex e2e driver; use models supported by the chosen harness.

```sh
export MJ_TEST_ROOT=/home/jonathan/.cache/mjolnir/build/parent-notify-manual
export MJ_INSTANCE=parent-notify-manual
export MJ_CONFIG_DIR="$MJ_TEST_ROOT/config"
export MJ_DATA_DIR="$MJ_TEST_ROOT/data"
export MJ_PROFILE_HOME="$MJ_TEST_ROOT/profile"
export MJ_PROJECT="$MJ_TEST_ROOT/project"
export MJ_HOME="$MJ_TEST_ROOT/home"
export MJ_BIN=/path/to/mj
export MJ_WORKER_BIN=/path/to/mj-worker
export HARNESS=codex                 # or claude
export MODEL=gpt-6.1-sol              # Codex driver default
export CHILD_MODEL=gpt-6-luna         # Codex driver default
mkdir -m 700 -p "$MJ_CONFIG_DIR" "$MJ_DATA_DIR" "$MJ_PROFILE_HOME" "$MJ_PROJECT" "$MJ_HOME"
```

Copy only the credential file into the new profile home. Never point `home` at
the real harness directory. For Codex:

```sh
cp "$HOME/.codex/auth.json" "$MJ_PROFILE_HOME/auth.json"
chmod 600 "$MJ_PROFILE_HOME/auth.json"
printf 'model = "%s"\n' "$MODEL" > "$MJ_PROFILE_HOME/config.toml"
```

For Claude, use this instead:

```sh
cp "$HOME/.claude/.credentials.json" "$MJ_PROFILE_HOME/.credentials.json"
chmod 600 "$MJ_PROFILE_HOME/.credentials.json"
```

Write an isolated config using the selected harness. Reserve an available loopback
port for `bind`; the example uses 3765. Preserve the model choices for the parent
and children in the sub-agent settings.

```toml
version = 18
[phone]
enabled = true
bind = "127.0.0.1:3765"
tailscale_detect = false
[github.watch]
enabled = true
[profiles.live]
kind = "codex" # set to "claude" for Claude
home = "/home/jonathan/.cache/mjolnir/build/parent-notify-manual/profile"
[targets.localhost]
kind = "bare"
[targets.podman]
kind = "podman"
pull_policy = "never"
[targets.docker]
kind = "docker"
pull_policy = "never"
[continuation]
enabled = true
[subagents]
max_concurrent = 2
[subagents.straightforward]
model = "gpt-6-luna"
effort = "high"
[subagents.subtle]
model = "gpt-6.1-sol"
effort = "medium"
```

Set `kind`, profile model, and sub-agent model to values supported by the chosen
harness. For the Claude profile used in the API run, the advertised choices
were `sonnet` for the parent and `haiku` for children; use the choices shown by
the current profile if they differ. In the config, set the straightforward
sub-agent model to `CHILD_MODEL` and the subtle model to `MODEL`. Audit that the
isolated config contains no source-home path before starting mj. Isolate ambient
state and credentials, then start a private tmux
server on its own socket. The real `mj` TUI starts the named instance:

```sh
export HOME="$MJ_HOME"
export MJ_WORKER_BINARY="$MJ_WORKER_BIN"
unset CODEX_API_KEY OPENAI_API_KEY ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN CLAUDE_CODE_OAUTH_TOKEN
if [ "$HARNESS" = codex ]; then export CODEX_HOME="$MJ_PROFILE_HOME"; else export CLAUDE_CONFIG_DIR="$MJ_PROFILE_HOME"; fi
export TMUX_SOCKET="$MJ_TEST_ROOT/tmux.sock"
tmux -S "$TMUX_SOCKET" new-session -d -s parent-notify \
  "$MJ_BIN" --instance "$MJ_INSTANCE"
tmux -S "$TMUX_SOCKET" capture-pane -p -t parent-notify
```

In the TUI, create/select a workspace and start a session on `localhost` using
profile `live`, with sub-agents enabled. Ask the parent to spawn exactly two
children named `delayed-a` and `delayed-b`. Each child should wait in one shell
command, checking once per second for its gate file under `$MJ_PROJECT`, then
report `REPORT_A_1302` or `REPORT_B_1302`.

Have the parent create `$MJ_PROJECT/midturn-ready` after both children start.
It should then run four separate shell tool calls, each `sleep 15`. After each
call, it should read any mailbox context delivered by the harness hook. When the
A report arrives, it writes `REPORT_A_1302` to `$MJ_PROJECT/report-a` during
that initial turn, then completes its remaining sleep calls and ends the turn
with a brief waiting status. The parent must not inspect the gates. Release A
after seeing `midturn-ready`; capture panes while the same parent turn is still
active and after it ends:

Use the dedicated tmux socket so captures and cleanup cannot affect another
tmux server:

```sh
tmux -S "$TMUX_SOCKET" capture-pane -p -t parent-notify > "$MJ_TEST_ROOT/pane-before-release-a.txt"
touch "$MJ_PROJECT/release-a"
tmux -S "$TMUX_SOCKET" capture-pane -p -t parent-notify > "$MJ_TEST_ROOT/pane-a-midturn.txt"
```

Confirm A's token is written before the original turn ends, and that no new
turn started for A. Once the parent is idle, keep B gated and confirm the parent
stays between turns for 30 seconds without automatic continuation. Restart only
the named isolated daemon, then confirm the idle parent and B child remain
present and working. Steer the parent to write `STEERING_ACK_1302` to
`$MJ_PROJECT/steering-ack`, then end that turn with a brief waiting status.
Release B while the parent is idle. Its mailbox event should wake exactly one
turn; the parent reads the embedded report, writes `REPORT_B_1302` to
`$MJ_PROJECT/report-b`, and finishes.

```sh
"$MJ_BIN" --instance "$MJ_INSTANCE" daemon restart
tmux -S "$TMUX_SOCKET" capture-pane -p -t parent-notify > "$MJ_TEST_ROOT/pane-after-restart.txt"
touch "$MJ_PROJECT/release-b"
tmux -S "$TMUX_SOCKET" capture-pane -p -t parent-notify > "$MJ_TEST_ROOT/pane-b-wake.txt"
```

Read the transcript through mj and the report event rows from the isolated
daemon database. Each `SubagentFinished` record must have `wake = 1`, a
`mailbox-` accepted command id, child id, task name, state facts, report output,
and report directory. The A event must be accepted while the original command
is active; A's marker must appear before that turn ends. The B wake turn uses a
`mailbox-wake-` command id and must appear once in completed turns. Expect
exactly three parent turns: initial work, steering, and B's mailbox wake. There must be no
`auto-continue-` turn.

```sh
"$MJ_BIN" --instance "$MJ_INSTANCE" transcript --session SESSION_ID --limit 1000 --json \
  > "$MJ_TEST_ROOT/parent-transcript.json"
sqlite3 -readonly "$MJ_DATA_DIR/mj.sqlite3" \
  'select command_id from session_turn_usage where session_id="SESSION_ID" order by completed_ordinal;'
sqlite3 -readonly "$MJ_DATA_DIR/mj.sqlite3" \
  'select event_key, wake, accepted_at, accepted_command_id, json_extract(event_json,"$.body.type"), json_extract(event_json,"$.body.report.child_session_id"), json_extract(event_json,"$.body.report.task_name"), json_extract(event_json,"$.body.report.state"), json_extract(event_json,"$.body.report.output"), json_extract(event_json,"$.body.report.report_dir") from mailbox_outbox where target_session_id="SESSION_ID" and event_key like "subagent-finished-%" order by created_at;'
```

After recording evidence, release any gates still closed, suspend/destroy only
the sessions created for this instance, and stop its daemon. Terminate the tmux
server only through `$TMUX_SOCKET`; do not use a global `tmux kill-server`.
