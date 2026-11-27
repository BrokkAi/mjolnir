# Migrating Mjolnir from `hel` (`MinasMorgulWSL`) to `morannon`

Runbook for an external agent executing a live control-plane migration of the default Mjolnir instance from `hel` (`MinasMorgulWSL`) to `morannon` using `mj migrate`.

## Environment Snapshot

| Property | Source (`hel` / `MinasMorgulWSL`) | Destination (`morannon`) |
| --- | --- | --- |
| OS / Arch / GLIBC | Ubuntu `x86_64` (WSL2), GLIBC `2.43` | Ubuntu `x86_64`, GLIBC `2.43` |
| Home directory | `/home/jonathan` | `/home/jonathan` |
| LAN / Tailscale | WSL NAT `172.30.214.21`, Tailscale `minasmorgul-wsl.tail5caf7.ts.net` (`100.73.108.9`) | LAN `192.168.1.77`, Tailscale **not installed** |
| Public IPv4 | `70.113.48.129` | `70.113.48.129` (same NAT; active EC2 security groups already admit it) |
| Free disk on `/` | — | `1.7 TB` free (`/dev/nvme0n1p2`) |
| Services | Live daemon + web viewer on `https://minasmorgul-wsl.tail5caf7.ts.net:3765/` | `Linger=yes`, `/usr/bin/podman`, `/usr/bin/git`, **no `mj` installed yet** |

Expected transfer size (default flags, excluding `backups/`/`logs/`/`moves/`): **~90 GB** (`mj.sqlite3` ~30 GB, `sessions/` ~41 GB, `tool-details/` ~1.4 GB, `projects/` ~66 MB, shared SessionWiki `~/.local/share/sessionwiki/index.db` ~17.9 GB, plus unmanaged profile homes). `workers/` (~89 GB) stays on `hel`.

---

## 0. Run from Outside Mjolnir

Execute this runbook from a plain shell on `hel` (not inside a Mjolnir session). If the shell inherited any Mjolnir session variables, clear them first:

```bash
unset MJ_SESSION_ID MJ_SESSION_BASH_ENV MJ_INSTANCE MJ_CONFIG_DIR MJ_DATA_DIR
```

Verify basic reachability from `hel` to `morannon`:

```bash
ssh -o BatchMode=yes morannon true
```

---

## 1. Install Identical `mj` Binaries on Both Hosts

Both hosts must run the **same** build (commit including the `mj migrate` fixes, schema 165+). Because `hel` and `morannon` share the exact same `x86_64` Ubuntu GLIBC 2.43 userland, build once on `hel` and copy the binaries atomically to `morannon`.

1. On `hel`, from an up-to-date `master` checkout of `mjolnir`, build and install:

   ```bash
   scripts/install.sh
   ```

   This installs `mj`, `mj-worker`, `mj-worker-x86_64-unknown-linux-musl`, and `mj-voice-worker` into `/home/jonathan/.cargo/bin/`.

2. Trigger the automatic daemon handoff on `hel` so the live daemon matches the new binary:

   ```bash
   mj daemon status
   ```

3. Copy the four binaries atomically to `morannon:~/.cargo/bin/` (plus `mj-worker-aarch64-apple-darwin` if keeping `[machines.macbook]`):

   ```bash
   ssh morannon 'mkdir -p ~/.cargo/bin'
   for bin in mj mj-worker mj-worker-x86_64-unknown-linux-musl mj-voice-worker; do
     scp "/home/jonathan/.cargo/bin/$bin" "morannon:.cargo/bin/$bin.next"
     ssh morannon "chmod 755 ~/.cargo/bin/$bin.next && mv -f ~/.cargo/bin/$bin.next ~/.cargo/bin/$bin"
   done
   ```

4. Confirm both hosts report the exact same version string:

   ```bash
   mj --version
   ssh morannon '~/.cargo/bin/mj --version'
   ```

---

## 2. Prepare SSH (and Tailscale) on `morannon`

### 2a. Mandatory: Enable `morannon` Self-SSH

The live store contains ~487 `ssh-podman` targets bound to `[machines.morannon]`, which remain SSH-managed after migration. `morannon` currently lacks an `~/.ssh/id_ed25519` key and cannot SSH to itself.

Read `morannon`'s own `/etc/ssh/ssh_host_*_key.pub` files locally on `morannon` (rather than using unauthenticated `ssh-keyscan`) before verifying self-SSH:

```bash
ssh morannon '
  set -euo pipefail
  mkdir -p ~/.ssh && chmod 700 ~/.ssh
  if [ ! -f ~/.ssh/id_ed25519 ]; then
    ssh-keygen -t ed25519 -N "" -f ~/.ssh/id_ed25519
  fi
  grep -qxFf ~/.ssh/id_ed25519.pub ~/.ssh/authorized_keys 2>/dev/null \
    || cat ~/.ssh/id_ed25519.pub >> ~/.ssh/authorized_keys
  chmod 600 ~/.ssh/authorized_keys
  for pub in /etc/ssh/ssh_host_*_key.pub; do
    read -r keytype key _ < "$pub"
    entry="morannon $keytype $key"
    grep -qxF "$entry" ~/.ssh/known_hosts 2>/dev/null || printf "%s\n" "$entry" >> ~/.ssh/known_hosts
  done
  chmod 600 ~/.ssh/known_hosts
  ssh -o BatchMode=yes morannon true
'
```

### 2b. Choose How to Handle `hel`'s Local Sessions and `[machines.macbook]`

`hel` has ~271 `local-bare` sessions (62 running, 209 parked) and a configured `[machines.macbook]` target (`jonathans-macbook-air`, currently 0 sessions). Choose **Path A** or **Path B**:

#### Path A (Recommended): Keep `hel`'s live sessions reachable and keep `[machines.macbook]`

1. Install and connect Tailscale on `morannon` (`sudo tailscale up`), so `morannon` can reach `minasmorgul-wsl.tail5caf7.ts.net` and `jonathans-macbook-air`.
2. Authorize `morannon`'s public key (`ssh morannon 'cat ~/.ssh/id_ed25519.pub'`) in:
   - `hel:~/.ssh/authorized_keys`
   - `jonathans-macbook-air:~/.ssh/authorized_keys`
3. Populate `morannon:~/.ssh/known_hosts` from authenticated host public keys (do **not** append unauthenticated `ssh-keyscan` output without verifying each fingerprint via `ssh-keygen -lf` against the host's `/etc/ssh/ssh_host_*_key.pub`), then verify non-interactive SSH from `morannon`:

   ```bash
   # On hel: read hel's local host keys and macbook's host keys over hel's
   # already-trusted SSH connection, and write them into morannon:~/.ssh/known_hosts.
   {
     for pub in /etc/ssh/ssh_host_*_key.pub; do
       read -r keytype key _ < "$pub"
       printf "minasmorgul-wsl.tail5caf7.ts.net %s %s\n" "$keytype" "$key"
     done
     ssh -o BatchMode=yes jonathans-macbook-air '
       for pub in /etc/ssh/ssh_host_*_key.pub; do
         read -r keytype key _ < "$pub"
         printf "jonathans-macbook-air %s %s\n" "$keytype" "$key"
       done
     '
   } | ssh morannon '
     set -euo pipefail
     touch ~/.ssh/known_hosts && chmod 600 ~/.ssh/known_hosts
     while IFS= read -r entry; do
       grep -qxF "$entry" ~/.ssh/known_hosts 2>/dev/null || printf "%s\n" "$entry" >> ~/.ssh/known_hosts
     done
     ssh -o BatchMode=yes minasmorgul-wsl.tail5caf7.ts.net true
     ssh -o BatchMode=yes jonathans-macbook-air true
   '
   ```

4. Use `--source-host minasmorgul-wsl.tail5caf7.ts.net --source-machine-id hel` in Steps 4–5.

#### Path B: Retire `hel` (no reverse SSH / no Tailscale on `morannon`)

`--disconnect-local` only skips the `morannon -> hel` SSH probe; `mj migrate` still rewrites `hel`'s `local-bare` session targets to `ssh-host-bare` pointing at `hel`, and their worker directories remain on `hel`.

1. **Move** (`mj move`) every `local-bare` session you intend to keep using or resuming after retiring `hel` onto `morannon` (or another reachable target) *before* migrating.
2. Any remaining `local-bare` sessions left on `hel` (whether suspended or parked) will retain their transcripts and history in the migrated store, but their migrated placements will point at an unreachable `hel` host and their worker directories will remain on `hel` — they cannot be resumed or run until `hel` is reachable again (or they are destroyed). Suspend or destroy any unneeded `local-bare` sessions before cutover.
3. Remove `[machines.macbook]` from `hel:~/.config/mjolnir/config.toml` if `morannon` cannot reach `jonathans-macbook-air` (otherwise the preflight third-host SSH probe will refuse).
4. Use `--disconnect-local --source-machine-id hel` in Steps 4–5.

*(EC2 hosts `ec2-m8id16-b/c/d` and `ec2-p4-meas` need no manual SSH key setup: `mj migrate` copies `~/.config/mjolnir/ec2/*` keys and known-hosts files automatically, and `morannon` shares `hel`'s public IPv4 `70.113.48.129`.)*

---

## 3. Clear Destination Collisions & Resolve Out-of-`$HOME` Config

1. **Move colliding directories aside on `morannon`** (preflight refuses if any of these exist):
   - `~/.local/share/mjolnir/recovery` (old Sep 6 snapshot)
   - `~/.codex` (stale Sep 27 standalone directory)
   - `~/.kimi-code` (existing directory colliding with `hel`'s `kimi` profile)

   ```bash
   ssh morannon '
     set -euo pipefail
     mkdir -p ~/.pre-migrate-backup
     for p in ~/.local/share/mjolnir/recovery ~/.codex ~/.kimi-code; do
       if [ -e "$p" ] || [ -L "$p" ]; then
         mv "$p" ~/.pre-migrate-backup/
       fi
     done
   '
   ```

2. **Resolve profile `carrier` on `hel`**:
   `~/.config/mjolnir/config.toml` defines `[profiles.carrier]` with an out-of-`$HOME` directory (`/mnt/optane/review-arms-20261113/instance/carrier-dsh`) that does not exist on `morannon`. Preflight refuses out-of-`$HOME` paths missing on the destination. Either:
   - Remove `[profiles.carrier]` from `hel:~/.config/mjolnir/config.toml` (if no longer needed), **or**
   - Copy `/mnt/optane/review-arms-20261113/instance/carrier-dsh` to the exact same path on `morannon`.

3. **Optional cleanup & parity copies**:
   - **Stale SessionWiki copy on `hel` (saves 2.0 GB)**: `hel` has both the active 17.9 GB `~/.local/share/sessionwiki/index.db` and a stale 2.0 GB `~/.local/share/mjolnir/sessionwiki/`. Remove `~/.local/share/mjolnir/sessionwiki` on `hel` before migrating if you do not want that stale copy transferred as data.
   - **AWS credentials for `claude-aws`**: `hel`'s `claude-aws` profile uses `AWS_PROFILE = "default"`, and `morannon` has no `~/.aws/`. Copy `~/.aws/` to `morannon` (`chmod 700 ~/.aws && chmod 600 ~/.aws/credentials`) if Bedrock is needed.
   - **Host SSH config**: `morannon` has no `~/.ssh/config`. Copy any desired entries from `hel:~/.ssh/config` (such as `ServerAliveInterval 15`, `ServerAliveCountMax 4`, `vast-serve`, `precision-3260`) — avoid copying `hel`-only WSL entries.

---

## 4. Run `--dry-run` First

Run dry-run preflight from `hel` using your chosen path:

```bash
# Path A (keep hel live):
mj migrate morannon \
  --source-host minasmorgul-wsl.tail5caf7.ts.net \
  --source-machine-id hel \
  --dry-run

# Path B (retire hel):
# mj migrate morannon \
#   --disconnect-local \
#   --source-machine-id hel \
#   --dry-run
```

Confirm before proceeding:
- Exit code is `0` with **zero blockers**.
- `SessionWiki` shows `migrate` (auto-detected from `~/.local/share/sessionwiki/index.db`, absent on `morannon`).
- Expected warnings only (e.g., 31 historical null-`machine_id` placements, `go.json` host-local paths, any `cleaned_up` EC2 notes). The 13 stuck `retiring` environments are permitted and rewritten automatically.
- `~/.secrets/gh_agents_pat` and all `$HOME` profile homes (`~/.claude*`, `~/.codex*`, `~/.grok`, `~/.kimi-code`, `~/.config/muse`, `~/.dsh`, `~/.ds8bit/opencode`, `~/.claude-bedrock`, `~/.gemini*`) appear in the copy plan.
- `morannon` remains untouched (`ssh morannon 'test ! -e ~/.local/share/mjolnir/mj.sqlite3'`).

---

## 5. Execute the Live Migration

Run the **exact same** command without `--dry-run`:

```bash
# Path A (keep hel live):
mj migrate morannon \
  --source-host minasmorgul-wsl.tail5caf7.ts.net \
  --source-machine-id hel
```

What happens:
1. **Phase 1 (zero downtime, ~15–30 min over 1 Gbps LAN)**: Streams `mj.sqlite3`, `sessions/`, `tool-details/`, `projects/`, SessionWiki `index.db`, profile homes, and config into a temporary staging directory on `morannon` while `hel` stays live.
2. **Phase 2 (seconds-long cutover)**: Stops and fences `hel`'s store (writes the `store_fence` row in `mj.sqlite3`), streams pinned SQLite WAL deltas and changed files, rewrites `mj.sqlite3` and `config.toml` for `morannon`, atomically publishes data/config/SessionWiki, unfences `morannon`'s store, starts `morannon`'s daemon, and detaches Codex `auth.json` symlinks on `hel` so existing Codex workers on `hel` do not mutate shared login files.

Add `--include-logs` only if you also want `backups/`, `logs/`, and `moves/` (~13.5 GB) copied.

---

## 6. Post-Migration Verification

1. **On `morannon`**:

   ```bash
   ssh morannon '
     set -euo pipefail
     ~/.cargo/bin/mj daemon status
     ~/.cargo/bin/mj sessions | head -n 40
     ls -lh ~/.local/share/sessionwiki/index.db
   '
   ```

2. **Verify instance identity matches**:

   ```bash
   diff -u ~/.local/share/mjolnir/instance-identity <(ssh morannon 'cat ~/.local/share/mjolnir/instance-identity')
   ```

3. **On `hel` (confirm fence is active)**:

   ```bash
   mj daemon status || true
   ```

   `hel` must report that the instance was migrated to `morannon` and refuse to start a local daemon.

4. **Optional — Web viewer / Tailscale Serve on `morannon`**:
   If `hel`'s web viewer (`https://minasmorgul-wsl.tail5caf7.ts.net:3765/`) was exposed via Tailscale Serve, configure the equivalent Tailscale Serve endpoint on `morannon` once Tailscale is connected.

---

## 7. Failure, Resume, and Rollback Rules

- **Failure before Phase 2 cutover**: `hel` is never fenced and its daemon is left running (or automatically restarted). Fix the reported error and re-run the same command.
- **Interrupted Phase 1 or Phase 2 (dropped SSH, Ctrl-C, reboot)**: Re-run the **exact same** `mj migrate` command. The staging `stage_id` and receiver `publication.json` bind recovery to the original destination instance and resume cleanly without split-brain.
- **Uncertain disconnect right at the end of Phase 2**: **Never** clear `hel`'s `store_fence` row before checking whether `morannon` finished **unfencing and activating**:

  ```bash
  ssh morannon '~/.cargo/bin/mj daemon status'
  ssh morannon 'python3 -c "import os, sqlite3; p = os.path.expanduser(\"~/.local/share/mjolnir/mj.sqlite3\"); print(\"no-db\" if not os.path.exists(p) else sqlite3.connect(f\"file:{p}?mode=ro\", uri=True).execute(\"SELECT COUNT(*) FROM store_fence\").fetchone()[0])"'
  ```

  - Do **not** treat the mere existence of `~/.local/share/mjolnir/mj.sqlite3` on `morannon` as success: if SSH dropped after the receiver placed `mj.sqlite3` but before it cleared the destination fence, `mj.sqlite3` exists while **both** hosts are still fenced (`store_fence` count is `1` and `mj daemon status` refuses).
  - If `morannon`'s store is **unfenced** (`store_fence` count is `0` and `mj daemon status` does not report a migration fence — if `--no-start` was not passed, the daemon is running), cutover **succeeded** — do **not** unfence or start `hel`.
  - If `morannon` has no `mj.sqlite3` yet **or** its `store_fence` count is still non-zero (still fenced), re-run the **exact same** `mj migrate` command from `hel` so recovery finishes unfencing `morannon`, cleans up staging, and starts `morannon`'s daemon.
