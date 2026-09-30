# Mac launch verification, 2026-09-30

This is internal evidence for #1135 and #1184, not a claim that every manual
launch mission is complete. The test host was `jonathans-macbook-air`, Apple
silicon, macOS 15.7.3. Local sessions and Docker Desktop work on this OS;
Apple's optional container runtime requires macOS 26.

## Builds and isolation

The existing Homebrew installation remains 2.21.0. The ordinary release installer
successfully installed 2.24.0 into
`~/mj-campaign/tier1135-20260930/fresh-home/.local/bin`, with its own profile file.
This was an isolated install directory, not a fresh macOS account. No global
Homebrew upgrade or login replacement was performed.

The candidate was built in `~/Projects/mjolnir-tier1135-20260930`, from base
`c0d16feacc0b43caafcfe4fe20e125bfffc80cb5` plus these task changes, using native
dev-profile Cargo storage. Its advertised version is 2.24.0. Every application
probe used a named instance and explicit disposable config/data directories.
The data path included `Library/Application Support/mjolnir` to exercise spaces.

## Discovered upgrade defect and fix

A real 2.21.0 worker finished its accepted turn after replacement by the released
2.24.0 daemon, but checkpoint failed with "project memory replica is outside the
harness home". The upgraded daemon creates a `workers/<session>/profile` link
to the old worker's original profile home. The checkpoint spec named the link,
while the worker's launch file and memory replica named the original home.
Lexical containment rejected the two spellings of the same directory.

The installed worker's launch configuration now owns the checkpoint source
spelling. A shared resolver verifies that the requested and installed homes
resolve to the same physical directory, then uses the worker's installed path.
Local controller exports also use that resolver, so an old compatible exporter
works without replacing a busy worker. Real outside-home memory remains refused.
The regression covers both explicit and environment-derived legacy homes, staged
and single-shot archives, and memory larger than 64 KiB.

## Fresh evidence

`tests/e2e/macos_launch.py` is an opt-in reusable real-harness probe. It copies
Codex login into a disposable profile and uses four prompts. Claude's current
login was unavailable (`claude auth status` reported signed out), so this run
used Codex with `gpt-6-luna`; it does not prove fresh Claude login.

The final run's evidence is on the Mac under
`~/mj-campaign/tier1135-20260930/fixed-handoff-v4/results.json`. It passed:

- Automatic 2.21.0-to-candidate handoff during a real foreground 45-second tool.
  Daemon PID 6829 became 7607 in 0.813 seconds, worker PID 7250 remained, and
  accepted turn 31 returned `MAC_HANDOFF_SURVIVED`.
- Repeated startup reused the replacement daemon.
- Checkpoint, suspend, resume, and a real follow-up reply with a space-containing
  data path. The resumed harness used a real session-owned staged home.
- Viewer HTTP login and logout, including rejection of the revoked cookie after
  daemon restart. This is normal viewer-cookie evidence, not native desktop
  ephemeral-cookie evidence.
- A real session reply after daemon restart. Cleanup reported no failures.

Reproduce on a signed-in Mac with matching candidate CLI and worker binaries:

    python3 tests/e2e/macos_launch.py --harness codex --model gpt-6-luna \
      --previous-mj /opt/homebrew/Cellar/mjolnir/2.21.0/bin/mj \
      --mj ~/Projects/mjolnir-tier1135-20260930/target/debug/mj \
      --artifacts ~/mj-campaign/new-mac-launch-run

The released 2.24.0 binary also opened a real isolated kitty window. Ctrl+b then
`?` opened help; Settings saved `ctrl+]` as the prefix, and Ctrl+] then `?`
opened help with the saved prefix. Captured screen text is in
`~/mj-campaign/tier1135-20260930/kitty-probe/`. Only the test window was closed.

Docker Desktop was initially stopped. Starting it, then running the installed
2.24.0 `doctor --json --smoke` in the isolated configured instance returned zero.
Docker 28.0.1 and the packaged aarch64 Linux worker were ready; disposable
run/exec/remove and read-only attachment smoke checks passed for
`ghcr.io/brokkai/mjolnir/agent-dev:latest`. Evidence:
`~/mj-campaign/tier1135-20260930/doctor-packaged-docker-smoke.json`.
Doctor honestly reported Apple container and local Podman as unsupported on this
host, and an optional Bifrost warning. No containers remained after the smoke
tests; Docker Desktop was restored to its original stopped state. Setup instructions correctly described
local bare and Docker Desktop support without requiring an OS upgrade.

## Focused validation

- Linux daemon startup integration: 12 passed; executable identity: 3 passed.
- Mac daemon startup integration: 11 passed.
- Checkpoint suite on Linux and Mac: 69 passed, 2 ignored on each host.
- Linux controller checkpoint suite: 46 passed; legacy profile-home suite:
  3 passed.
- Mac terminal PTY integration: 11 passed, including terminal restoration,
  signals, cancellation, and automatic upgrade reexecution preserving drafts.
- Rust formatting, Python Ruff/compile/help, and macOS CI path checks passed.

Mac test logs are `~/mj-tier1135-build-tests.log`,
`~/mj-tier1135-checkpoint-tests.log`, and `~/mj-tier1135-termination-pty.log`.
The new Mac probe is included in the macOS-sensitive CI path list.
Full integrated validation and publication are recorded in the accompanying
ExecPlan and issue comments after they complete.

## Remaining mission coverage

#1135 remains open for Terminal.app/iTerm2 interactive automation (System Events
still timed out), fresh Claude setup-token login, current native desktop GUI
ephemeral-cookie logout, and the remaining manual clipboard/mouse/bell checks.
Earlier 2.21.0 evidence includes kitty image paste and unread titles; those were
not repeated on the current candidate. A real global Homebrew formula upgrade
was not performed. Installer coverage and a real active-turn release-to-source
handoff were exercised instead. Apple container needs a separate macOS 26 host;
this is an optional runtime gap, not a reason to upgrade this Mac for Mjolnir.
