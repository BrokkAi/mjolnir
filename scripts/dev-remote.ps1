# Native Windows + Linux development install. Requires Git, OpenSSH, Rust/MSVC.
# Configure the selected client's [daemon] SSH placement before running.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string] $SshHost,
    [string] $Commit = 'HEAD',
    [string] $RemoteCheckout = '~/Projects/mjolnir',
    [string] $WindowsCheckout = (Split-Path $PSScriptRoot -Parent),
    [string] $Instance = 'remote-dev'
)
Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'

function Invoke-Git([string[]] $GitArgs) {
    $result = & git @GitArgs
    if ($LASTEXITCODE -ne 0) { throw "git $GitArgs failed (exit $LASTEXITCODE)" }
    $result
}
function Quote-Sh([string] $Value) {
    "'" + $Value.Replace("'", "'`"'`"'") + "'"
}
function Invoke-Remote([string] $Body) {
    # One quoted command avoids Windows OpenSSH's argv reassembly ambiguities.
    $command = 'bash -lc ' + (Quote-Sh $Body)
    $result = & ssh -o BatchMode=yes $SshHost $command
    if ($LASTEXITCODE -ne 0) { throw "SSH operation failed on $SshHost (exit $LASTEXITCODE)" }
    $result
}
function Assert-Clean([string] $Tree, [string] $Sha) {
    $dirty = Invoke-Git -GitArgs @('-C', $Tree, 'status', '--porcelain', '--untracked-files=all')
    if ($dirty) { throw "Build worktree $Tree is dirty. Preserve its edits elsewhere, then commit or remove them there and rerun. No reset or clean was performed." }
    if ($Sha -and (Invoke-Git -GitArgs @('-C', $Tree, 'rev-parse', 'HEAD')) -ne $Sha) {
        throw "HEAD changed in $Tree during the build. Stop other writers and rerun at $Sha."
    }
}

# Expand only a leading ~/ remotely; quote all caller-controlled text.
$remotePath = if ($RemoteCheckout.StartsWith('~/')) {
    '"$HOME"/' + (Quote-Sh $RemoteCheckout.Substring(2))
} else { Quote-Sh $RemoteCheckout }
$sha = (Invoke-Remote ("set -e; cd $remotePath; git rev-parse --verify " + (Quote-Sh ($Commit + '^{commit}')))).Trim()
if ($sha -notmatch '^[0-9a-f]{40}$') { throw "Expected one full commit SHA from $SshHost; received: $sha" }
Write-Host "Installing source $sha on both machines. To recover a partial install, rerun with -Commit $sha."

$WindowsCheckout = (Resolve-Path -LiteralPath $WindowsCheckout).Path
$buildTree = "$WindowsCheckout-build"
# A persistent lock file is intentionally outside the Git worktree. FileShare.None
# releases automatically on exit; never delete a lock another process may hold.
$lock = [IO.File]::Open("$buildTree.lock", 'OpenOrCreate', 'ReadWrite', 'None')
try {
    # Always attempt origin, then the box for commits not yet published.
    & git -C $WindowsCheckout fetch origin $sha
    if ($LASTEXITCODE -ne 0) {
        $repository = "${SshHost}:$RemoteCheckout"
        & git -C $WindowsCheckout fetch $repository $sha
        if ($LASTEXITCODE -ne 0) {
            throw "Cannot fetch $sha. On the box run: git -C $remotePath push origin ${sha}:refs/heads/mj-dev-build . Then rerun this command with -Commit $sha. Alternatively allow Git-over-SSH access to '$repository'."
        }
    }
    if (Test-Path -LiteralPath $buildTree) {
        Assert-Clean $buildTree ''
        Invoke-Git -GitArgs @('-C', $buildTree, 'checkout', '--detach', $sha)
    } else {
        Invoke-Git -GitArgs @('-C', $WindowsCheckout, 'worktree', 'add', '--detach', $buildTree, $sha)
    }
    Assert-Clean $buildTree $sha

    $remoteBuild = @'
set -euo pipefail
cd REMOTE_PATH
source_tree=$PWD
build_tree="${source_tree}-build"
exec 9>"${build_tree}.lock"
flock -n 9 || { echo "Another install holds ${build_tree}.lock; wait for it to finish." >&2; exit 1; }
check_clean() {
    if test -n "$(git -C "$build_tree" status --porcelain --untracked-files=all)"; then
        echo "Build worktree $build_tree is dirty. Preserve edits elsewhere, then commit or remove them there and rerun. No reset or clean was performed." >&2
        exit 1
    fi
}
if test -e "$build_tree"; then
    check_clean
    git -C "$build_tree" checkout --detach BUILD_SHA
else
    git worktree add --detach "$build_tree" BUILD_SHA
fi
check_clean
cd "$build_tree"
./scripts/install.sh
check_clean
test "$(git rev-parse HEAD)" = BUILD_SHA || { echo "HEAD changed in $build_tree; stop other writers and rerun." >&2; exit 1; }
installed="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin/mj"
echo "Linux installed executable: $installed"
"$installed" --version
'@
    Invoke-Remote ($remoteBuild.Replace('REMOTE_PATH', $remotePath).Replace('BUILD_SHA', $sha))
    Write-Host "Linux installation completed for $sha; building Windows."
    Push-Location $buildTree
    try {
        & cargo build --release --locked -p brokk-mjolnir --bin mj --target x86_64-pc-windows-msvc
        if ($LASTEXITCODE -ne 0) { throw 'Windows build failed; Linux is already installed. Rerun at the same SHA.' }
        Assert-Clean $buildTree $sha
        # Cargo metadata respects normal workspace/Cargo configuration; do not
        # assume target/ if the user has configured a different build directory.
        $metadataText = & cargo metadata --no-deps --format-version 1 --locked
        if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed' }
        $metadata = ($metadataText -join "`n") | ConvertFrom-Json
        $binary = Get-Item -LiteralPath (Join-Path $metadata.target_directory 'x86_64-pc-windows-msvc/release/mj.exe')
        . (Join-Path (Split-Path $PSScriptRoot -Parent) 'install.ps1') -FunctionsOnly
        $installDir = Get-MjInstallDirectory
        Install-MjBinaries @($binary) $installDir
        Update-MjInstallPath $installDir
        Assert-Clean $buildTree $sha
        $installed = Join-Path $installDir 'mj.exe'
        # This read-only control query uses normal placement startup/handoff,
        # including a remote instance override in the client configuration.
        & $installed --instance $Instance daemon afk status
        if ($LASTEXITCODE -ne 0) { throw 'Remote startup/handoff failed; check the selected instance SSH placement and rerun at the same SHA.' }
        $status = & $installed --instance $Instance daemon status
        if ($LASTEXITCODE -ne 0) { throw 'Installed client could not verify the remote daemon. Check the selected instance SSH placement, then rerun at the same SHA.' }
        $status | Write-Host
        if (($status -join "`n") -notmatch '(?m)^\s*same_source:\s*true\s*$') {
            throw 'Both installs ran, but daemon status did not confirm same_source: true. Check SSH placement host, remote_command and instance; rerun at the same SHA.'
        }
        Write-Host "Both machines installed $sha; same_source: true."
    } finally { Pop-Location }
} finally { $lock.Dispose() }
