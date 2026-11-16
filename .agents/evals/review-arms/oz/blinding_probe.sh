#!/bin/sh
# Blinding probe, run inside a reviewer-equivalent container. Key arrives base64 in $K64, the
# server's user@host in $TARGET (from R2/private/ssh/target).
set -u
echo "## identity"; id; command -v python3 sh git base64 ssh
echo "## 1. local paths (must all fail)"
for p in /mnt/optane/review-arms-oz/private/fixtures /mnt/optane/review-arms-oz /mnt/optane/review-arms-20261113 /srv/review-arms/oz/fixtures /home/jonathan; do
  ls "$p" >/dev/null 2>&1 && echo "READABLE $p" || echo "refused $p"
done
echo "## 2. decisive text anywhere in the container filesystem (must be none)"
grep -rIl --exclude-dir=proc --exclude-dir=sys --exclude-dir=dev -e 'checks `cancelled` after `prompt` returns' / 2>/dev/null | head; echo "grep done"
echo "## 3. git-cache reach (pre-existing for every arm)"
git -c safe.directory=* -C /run/hel/git-cache rev-list --all 2>/dev/null | wc -l
for c in 5e0dd2242 5ac6cafec; do git -c safe.directory=* -C /run/hel/git-cache cat-file -e "$c^{commit}" 2>/dev/null && echo "has $c" || echo "lacks $c"; done
d=$(mktemp -d); umask 077; printf %s "$K64" | base64 -d > $d/key; printf '%s\n' "$KH" > $d/kh
S="ssh -T -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile=$d/kh -i $d/key $TARGET"
echo "## 4. escapes through the Oz key (each must be refused)"
$S "cat /srv/review-arms/oz/fixtures/a583fa1036f534039bae407b3b22750b259111e8.json" </dev/null; echo "cat-cmd exit=$?"
$S "../fixtures/a583fa1036f534039bae407b3b22750b259111e8 unknown" </dev/null; echo "traversal exit=$?"
$S "a583fa1036f534039bae407b3b22750b259111e8 unknown; cat /etc/passwd" </dev/null; echo "injection exit=$?"
$S "a583fa1036f534039bae407b3b22750b259111e8 \$(id)" </dev/null; echo "subst exit=$?"
ssh -T -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile=$d/kh -i $d/key -s "$TARGET" sftp </dev/null; echo "sftp exit=$?"
timeout 20 ssh -N -o ExitOnForwardFailure=yes -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile=$d/kh -i $d/key -L 127.0.0.1:2299:127.0.0.1:22 "$TARGET" </dev/null; echo "port-forward exit=$?"
echo "## 5. non-allowlisted sha (must be 65)"
$S "0000000000000000000000000000000000000000 unknown" </dev/null; echo "unlisted exit=$?"
$S "4ef1f218b798f48250d24d8258bd625e69ab1b35 unknown" </dev/null; echo "base-commit exit=$?"
echo "## 6. allowlisted diff answers"
python3 /probe/probe_client.py --site brokk_mjolnir.acp.Adapter.cancel --site mj-cli/src/acp.rs:283 --site turns::prompt --site no_such_function_xyz -- \
  $S "a583fa1036f534039bae407b3b22750b259111e8 00000000000000000000000000b1ab0e"; echo "allowlisted exit=$?"
rm -rf $d
