# Milestones 6–7 validation (2026-11-12)

Passed: git diff 0854dc78d --check; node --check docs/astro.config.mjs;
extracted Linux payload from scripts/dev-remote.ps1, substituted quoted checkout,
SHA and instance, passed bash -n through stdin; verified guide page link targets.

Unavailable: pwsh (command -v found none); docs/node_modules absent. No new
tools installed. PowerShell syntax and Windows behavior are unvalidated.
Docs build not run. Cargo checks not applicable to script/documentation changes.

On native Windows, from the final checkout, parse both scripts:

```powershell
foreach ($file in @('install.ps1', 'scripts/dev-remote.ps1')) {
    $tokens = $null
    $errors = $null
    [System.Management.Automation.Language.Parser]::ParseFile(
        (Join-Path $PWD $file), [ref]$tokens, [ref]$errors) | Out-Null
    if ($errors.Count) { $errors; throw "Parse failed: $file" }
}
```

Configure isolated client instance remote-dev with SSH placement pointing to the
Linux box's remote-dev instance. Run, replacing host/path/SHA with actual values:

```powershell
pwsh -File scripts/dev-remote.ps1 -SshHost linux-box -RemoteCheckout '~/Projects/mjolnir' -Commit <full-SHA> -Instance remote-dev
pwsh -File scripts/dev-remote.ps1 -SshHost linux-box -RemoteCheckout '~/Projects/mjolnir' -Commit <same-full-SHA> -Instance remote-dev
```

Both runs must finish with matching build identities and same_source: true.
Exercise once with a stopped remote daemon and once with an older running build.
Check dirty-worktree refusal and preservation of source checkout branches.
Use the actual native Windows executable, not WSL. No acceptance claim yet.

With docs dependencies available: cd docs; npm ci; npm run build.

Review fixes: startup now uses installed Windows `daemon afk status`, a read-only
control request through normal placement startup/handoff. This respects remote
instance overrides without parsing TOML in PowerShell. Recovery commands expand
remote ~/ via quoted $HOME rather than quoting a literal tilde.
