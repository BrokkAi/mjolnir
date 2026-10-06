---
title: GitHub App credentials
description: Configure controller-owned GitHub App installation tokens for Mjolnir sessions and CI.
---

Mjolnir can mint GitHub App installation tokens for bundle repositories. The
private key stays on the Mjolnir controller host, and the controller shares a
cached token across provisioning, resume, running-session credential sync, and
the `mj github-token` command.

## Create and install the App

1. In GitHub, create a private GitHub App under your account or organization.
   See GitHub's guides for [registering an App](https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app)
   and [choosing permissions](https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app/choosing-permissions-for-a-github-app).
2. Grant repository **Contents: Read and write** and **Metadata: Read-only**.
   Add **Workflows: Read and write** only if sessions must push changes to
   `.github/workflows` files.
3. Install the App on each organization or user account whose repositories
   Mjolnir should access. Limit the installation to the needed repositories.
4. Create a private key for the App and copy its PEM file to the controller
   host. Restrict the file to the daemon's user, for example with `chmod 600`.
5. Record the App ID and, optionally, each installation ID from the App's
   installation settings page.

## Configure Mjolnir

Add this to the controller's `config.toml`:

```toml
[github.app]
app_id = 1234
private_key_path = "/home/mj/.config/mjolnir/github-app.pem"

[github.app.installations]
acme = 987654
```

The installation map is optional. When an owner is missing, Mjolnir asks
GitHub for the repository or account installation and caches the result. The
key path must be readable on the controller host. Restart the daemon after
changing the App configuration.

Without `[github.app]`, Mjolnir continues to use `GH_TOKEN`, `GITHUB_TOKEN`, or
`gh auth token`. The private key and installation tokens are not stored in the
database or session checkpoints.

## Token scope and session limit

Each session can use repositories from one GitHub App installation. Session
creation reports an error when its bundle needs more than one installation.
Session tokens are limited to the repositories in that bundle; the controller
keeps a separate cache entry for each installation and sorted repository set.
Tokens refresh when fewer than 10 minutes of validity remain. Running remote
and container sessions receive the refreshed token through credential sync.
Local bare sessions are not periodically refreshed, so an App token used there
expires within at most one hour.

Use the daemon's shared cache to obtain a token for host-side CI or automation:

```sh
export GH_TOKEN="$(mj github-token --owner acme)"
gh api /user
```

`--owner` returns an installation-wide token. Use `--repo` to restrict a token
to the selected repository, or repeat it to select several repositories from
the same installation:

```sh
export GH_TOKEN="$(mj github-token --repo acme/project)"
export GH_TOKEN="$(mj github-token --repo acme/project --repo acme/tools)"
```

The command rejects repository sets that span installations.

The command requires a running daemon and a configured App. Its output is a
credential: keep it out of logs and discard it when the job finishes.
