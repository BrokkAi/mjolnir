#!/bin/sh
# Regenerates site_context_view.schema.json from bifrost-dev master with schemars.
# Usage: dump_schema.sh [bifrost clone, default ~/Projects/bifrost]
# Needs the scratch test body below (crates/bifrost-tools/tests/h1_schema_dump.rs),
# which this script writes into a detached worktree and removes afterwards.
set -eu
OUT="$(cd "$(dirname "$0")" && pwd)"
CLONE="${1:-$HOME/Projects/bifrost}"
WT="$HOME/Projects/bifrost-dev-wt/h1-schema"
git -C "$CLONE" fetch origin
mkdir -p "$(dirname "$WT")"
git -C "$CLONE" worktree add --detach "$WT" origin/master
# Remove the worktree on every exit, so a failed build can be rerun. On failure, print the
# build log's tail first: cargo's output goes only to that file.
trap 'st=$?; if [ "$st" -ne 0 ] && [ -f "$OUT/.schema-build.log" ]; then tail -n 60 "$OUT/.schema-build.log" >&2; fi; rm -f "$OUT/.schema-build.log"; git -C "$CLONE" worktree remove --force "$WT"' EXIT
COMMIT="$(git -C "$WT" rev-parse HEAD)"
cat > "$WT/crates/bifrost-tools/tests/h1_schema_dump.rs" <<'RS'
use brokk_bifrost_tools::site_context::SiteContextView;

#[test]
fn dump_site_context_view_schema() {
    let schema = schemars::schema_for!(SiteContextView);
    println!("SCHEMA_BEGIN\n{}\nSCHEMA_END", serde_json::to_string_pretty(&schema).unwrap());
}
RS
(cd "$WT" && cargo test -p brokk-bifrost-tools --test h1_schema_dump -- --nocapture) > "$OUT/.schema-build.log" 2>&1
sed -n '/^SCHEMA_BEGIN$/,/^SCHEMA_END$/p' "$OUT/.schema-build.log" | sed '1d;$d' > "$OUT/site_context_view.schema.json"
printf 'bifrost-dev commit: %s\nsource: crates/bifrost-tools/src/site_context.rs (SiteContextView)\ncommand: dump_schema.sh (cargo test -p brokk-bifrost-tools --test h1_schema_dump -- --nocapture)\n' "$COMMIT" > "$OUT/site_context_view.schema.SOURCE.txt"
