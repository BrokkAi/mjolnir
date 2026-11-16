# Wizard-of-Oz `site_context` stand-in (H1)

- `dump_schema.sh` regenerates `site_context_view.schema.json` (+ `.SOURCE.txt`) from bifrost-dev master with schemars.
- `build_fixtures.py` writes `/mnt/optane/review-arms-oz/private/fixtures/<intro sha>.json` (0700) and `unresolved-anchors.md`.
- `site_context_server.py`, `serve-site-context.sh`: copy to the host as `/srv/review-arms/oz/` (fixtures/ and logs/ beside them).
- `probe_client.py --site X ... -- <command>`: JSON-RPC probe; local run needs `SITE_CONTEXT_ROOT`, `SSH_ORIGINAL_COMMAND`.
Validation output: `/mnt/optane/tmp/h1-oz/validation/validation.out`.
- `blinding_probe.sh`: run inside a reviewer-equivalent container (`podman run --rm --user 0:0 --network pasta`,
  the pinned review-arms image, a reviewer's git-cache read-only at `/run/hel/git-cache`, this directory at
  `/probe`, the Oz key base64 in `K64` and the known_hosts line in `KH`). Never use `--userns=keep-id` on hel:
  it chowns the whole image and holds the Podman storage lock for every session.
Retirement (PLAN-oz.md): delete this directory, `oz-instance/` and the host's `/srv/review-arms/oz/` when an arm
with the real toolset exists behind the same interface.
