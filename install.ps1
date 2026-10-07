# Install the latest Mjolnir release on native Windows.
#
# Usage:
#   irm https://raw.githubusercontent.com/BrokkAi/mjolnir/master/install.ps1 | iex
#
# Environment:
#   MJOLNIR_INSTALL_DIR      Install directory. Defaults to %LOCALAPPDATA%\Programs\Mjolnir\bin.
#   MJOLNIR_VERSION          Optional release tag to install, for example v2.33.0.
#   MJOLNIR_GITHUB_OWNER     GitHub owner to download from. Defaults to BrokkAi.
#   MJOLNIR_GITHUB_API       GitHub API base URL. Defaults to https://api.github.com.
#   MJOLNIR_NO_MODIFY_PATH   Set to leave the user Path unchanged.
#   GITHUB_TOKEN             Optional token for GitHub API rate limits.
#
# The script runs inside a script block so `iex` leaves no variables behind,
# and it reports failures by throwing: `exit` would close the caller's shell.

& {
    Set-StrictMode -Version 3.0
    $ErrorActionPreference = 'Stop'
    # Windows PowerShell 5.1 renders download progress so slowly that it
    # dominates the transfer time.
    $ProgressPreference = 'SilentlyContinue'
    [Net.ServicePointManager]::SecurityProtocol =
        [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $ScriptVersion = '1.0.0'
    # The x64 build also serves Windows on ARM through its x64 emulation.
    $Target = 'x86_64-pc-windows-msvc'
    # Shared with mj's own updater, which sweeps the same leftovers.
    $ReplacedPrefix = '.mj-replaced-'

    function Write-Step([string] $Message) {
        Write-Host "mjolnir-installer: $Message"
    }

    function Get-Setting([string] $Name, [string] $Default) {
        $value = [Environment]::GetEnvironmentVariable($Name)
        if ([string]::IsNullOrEmpty($value)) { $Default } else { $value }
    }

    $owner = Get-Setting 'MJOLNIR_GITHUB_OWNER' 'BrokkAi'
    $api = (Get-Setting 'MJOLNIR_GITHUB_API' 'https://api.github.com').TrimEnd('/')
    $version = Get-Setting 'MJOLNIR_VERSION' ''
    $defaultDir = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'Programs\Mjolnir\bin'
    $installDir = [IO.Path]::GetFullPath((Get-Setting 'MJOLNIR_INSTALL_DIR' $defaultDir))

    $headers = @{ 'User-Agent' = "mjolnir-install-ps1/$ScriptVersion"; 'Accept' = 'application/vnd.github+json' }
    $token = Get-Setting 'GITHUB_TOKEN' ''
    if ($token) { $headers['Authorization'] = "Bearer $token" }

    $endpoint = if ($version) {
        "$api/repos/$owner/mjolnir/releases/tags/$version"
    } else {
        "$api/repos/$owner/mjolnir/releases/latest"
    }
    $release = Invoke-RestMethod -Uri $endpoint -Headers $headers -UseBasicParsing
    $archives = @($release.assets | Where-Object { $_.name -like "brokk-mjolnir-*-$Target.zip" })
    if ($archives.Count -ne 1) {
        $names = ($release.assets | ForEach-Object { $_.name }) -join ', '
        throw "release $($release.tag_name) has no single $Target archive; assets: $names"
    }
    $archive = $archives[0]
    $sidecar = @($release.assets | Where-Object { $_.name -eq "$($archive.name).sha256" })
    if ($sidecar.Count -ne 1) {
        throw "release $($release.tag_name) is missing the checksum $($archive.name).sha256"
    }

    $work = Join-Path ([IO.Path]::GetTempPath()) ("mjolnir-install-" + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $work | Out-Null
    try {
        $zip = Join-Path $work $archive.name
        $sum = "$zip.sha256"
        Write-Step "downloading $($archive.name)"
        Invoke-WebRequest -Uri $archive.browser_download_url -OutFile $zip -UseBasicParsing
        Invoke-WebRequest -Uri $sidecar[0].browser_download_url -OutFile $sum -UseBasicParsing

        # A sidecar reads "<hash>  <name>"; only the hash matters.
        $expected = ((Get-Content -LiteralPath $sum -Raw).Trim() -split '\s+')[0].ToLowerInvariant()
        $actual = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($expected -ne $actual) {
            throw "checksum mismatch for $($archive.name): expected $expected, got $actual"
        }

        $extracted = Join-Path $work 'extracted'
        Expand-Archive -LiteralPath $zip -DestinationPath $extracted
        $bundles = @(Get-ChildItem -LiteralPath $extracted -Directory)
        if ($bundles.Count -ne 1) {
            throw "$($archive.name) must contain exactly one top-level directory"
        }
        # The same application binaries mj's updater replaces; documents stay behind.
        $binaries = @(Get-ChildItem -LiteralPath $bundles[0].FullName -File | Where-Object {
            $stem = $_.Name -replace '\.exe$', ''
            $stem -in @('mj', 'mj-desktop', 'mj-voice-worker', 'mj-worker') -or $stem -like 'mj-worker-*'
        })
        $controller = $binaries | Where-Object { $_.Name -eq 'mj.exe' }
        if (-not $controller) {
            throw "$($archive.name) does not contain mj.exe"
        }
        $reported = & $controller.FullName --version
        if ($LASTEXITCODE -ne 0) {
            throw "the downloaded mj.exe failed to run (exit code $LASTEXITCODE)"
        }

        New-Item -ItemType Directory -Path $installDir -Force | Out-Null
        # Earlier upgrades leave replaced binaries behind while something still
        # runs them, and Windows refuses to delete those until they exit. An
        # interrupted run can also leave a staged copy.
        Get-ChildItem -LiteralPath $installDir -Force |
            Where-Object { $_.Name.StartsWith($ReplacedPrefix) -or $_.Name.StartsWith('.mj-install-') } |
            Remove-Item -Force -ErrorAction SilentlyContinue

        # Windows refuses to overwrite a running executable but lets it be
        # renamed, so each installed file moves aside before its replacement
        # moves in. Staging in the install directory keeps both renames on one
        # volume. The controller goes last, once every companion is in place.
        $ordered = @($binaries | Where-Object { $_.Name -ne 'mj.exe' }) + @($controller)
        foreach ($binary in $ordered) {
            $nonce = [Guid]::NewGuid().ToString('N')
            $destination = Join-Path $installDir $binary.Name
            $staged = Join-Path $installDir ".mj-install-$($binary.Name)-$nonce"
            Copy-Item -LiteralPath $binary.FullName -Destination $staged
            if (Test-Path -LiteralPath $destination) {
                $aside = Join-Path $installDir "$ReplacedPrefix$($binary.Name)-$nonce"
                Move-Item -LiteralPath $destination -Destination $aside
                try {
                    Move-Item -LiteralPath $staged -Destination $destination
                } catch {
                    Move-Item -LiteralPath $aside -Destination $destination
                    Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue
                    throw
                }
                Remove-Item -LiteralPath $aside -Force -ErrorAction SilentlyContinue
            } else {
                Move-Item -LiteralPath $staged -Destination $destination
            }
        }
        Write-Step "installed $reported to $installDir"
    } finally {
        Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    }

    if (Get-Setting 'MJOLNIR_NO_MODIFY_PATH' '') {
        return
    }
    $environment = Get-Item -Path 'HKCU:\Environment'
    # Keep entries such as %USERPROFILE%\bin unexpanded when writing Path back.
    $userPath = $environment.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
    $entries = @($userPath -split ';' | Where-Object { $_ })
    $present = $entries | Where-Object {
        [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\') -ieq $installDir.TrimEnd('\')
    }
    if (-not $present) {
        $kind = if ($userPath -match '%') { 'ExpandString' } else { 'String' }
        New-ItemProperty -Path 'HKCU:\Environment' -Name 'Path' -Value (($entries + $installDir) -join ';') `
            -PropertyType $kind -Force | Out-Null
        # Setting any user variable through .NET broadcasts the environment
        # change, so terminals opened from now on see the new Path.
        [Environment]::SetEnvironmentVariable('MJOLNIR_INSTALLER_REFRESH', '1', 'User')
        [Environment]::SetEnvironmentVariable('MJOLNIR_INSTALLER_REFRESH', $null, 'User')
        $env:Path = "$env:Path;$installDir"
        Write-Step "added $installDir to your user Path; open a new terminal, then run mj"
    } else {
        Write-Step 'run mj to start'
    }
}
