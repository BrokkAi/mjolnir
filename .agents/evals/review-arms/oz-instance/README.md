# R2: the Wizard-of-Oz `site_context` instance (brief/PLAN-oz.md)

Files: `review_arms_oz.config.toml` (Codex overlay, MCP server `bifrost`), `config-J/` and `config-O/`
(rendered `config.toml` with host placeholders), `setup.py` (renders, or applies with real host files).
R2 = `/mnt/optane/review-arms-oz`; jobs in `R2/oz-jobs.json`. The phone listener is moved to port 38518
so R's daemon can run at once.

Variant J (homes stay `/home/jonathan/.codexN`): Jonathan copies `review_arms_oz.config.toml` into each of
the four homes. Profiles without `codex_profile` (the main instance) ignore the overlay.
`setup.py apply --variant J --target-file T --known-hosts-file K --key-file KEY`, then
`REVIEW_ARMS_DIR=R2 ./mjx doctor --json` and `./mjx workspaces create "Review arms oz" --json > R2/instance/workspace.json`.

Variant O (own homes): the same `apply --variant O`, which also creates the four homes with copies of
config.toml/AGENTS.md and the overlay; then `REVIEW_ARMS_DIR=R2 ./mjx login --profile codexN` for N in "", 2, 3, 4
(Jonathan), then doctor and workspace creation as above.
Then `REVIEW_ARMS_DIR=R2 python3 driver.py --jobs R2/oz-jobs.json`.

Host files (made by H1, 2026-11-16; private, 0600 in a 0700 directory): `--target-file
R2/private/ssh/target` (`rvtools@<server host>`), `--known-hosts-file R2/private/ssh/known_hosts`,
`--key-file R2/private/ssh/oz_site_context`. `R2/private/ssh/bifrost_mcp` is the separate key for H2's
real-Bifrost forced command; no profile here uses it. The server, wrapper and fixtures live on the host in
`/srv/review-arms/oz/` (user `rvtools`, 0700); per-call logs are in `/srv/review-arms/oz/logs/`.
The host's security group admits SSH only from hel's current address (recorded in the security group, not here).
