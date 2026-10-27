<#
.SYNOPSIS
    Reason Network installer for Windows.

.DESCRIPTION
    Downloads (or takes from a local directory) the release package for this
    machine, verifies it against the release's SHA256SUMS, and installs
    reason-network.exe to a user-writable directory. Configuration and
    application data are never modified by an install or upgrade. No
    administrator rights are needed.

.PARAMETER Version
    Install this version (default: the latest release).

.PARAMETER InstallDir
    Where to put reason-network.exe (default: %LOCALAPPDATA%\Programs\reason-network).

.PARAMETER FromDir
    Install from a local directory holding SHA256SUMS and the release packages.

.PARAMETER Repo
    GitHub repository to download from, OWNER/NAME (default: $env:REASON_NETWORK_REPO).

.PARAMETER Target
    Override platform detection (testing).

.PARAMETER AddToPath
    Add the install directory to the user PATH.

.PARAMETER Uninstall
    Remove the executable, its background service and its PATH entry. User data is kept.

.PARAMETER PurgeData
    With -Uninstall: also delete the data directory.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File install.ps1 -Repo owner/reason-network

.EXAMPLE
    .\install.ps1 -FromDir .\dist -AddToPath
#>
[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$InstallDir = "",
    [string]$FromDir = "",
    [string]$Repo = $env:REASON_NETWORK_REPO,
    [string]$Target = "",
    [switch]$AddToPath,
    [switch]$Uninstall,
    [switch]$PurgeData
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$Product = "reason-network"
$SupportedTargets = @("x86_64-pc-windows-msvc")

function Fail([string]$Message) {
    # A plain line on stderr and exit code 1; `finally` blocks still clean up.
    [Console]::Error.WriteLine("error: $Message")
    exit 1
}

function Get-DataDir {
    if ($env:REASON_NETWORK_DATA_DIR) { return $env:REASON_NETWORK_DATA_DIR }
    return (Join-Path $env:APPDATA $Product)
}

function Test-OnUserPath([string]$Dir) {
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (-not $userPath) { return $false }
    $wanted = $Dir.TrimEnd("\")
    return [bool]($userPath -split ";" | Where-Object { $_.TrimEnd("\") -ieq $wanted })
}

if (-not $InstallDir) {
    if ($env:REASON_NETWORK_INSTALL_DIR) {
        $InstallDir = $env:REASON_NETWORK_INSTALL_DIR
    } else {
        $InstallDir = Join-Path $env:LOCALAPPDATA "Programs\$Product"
    }
}
$Exe = Join-Path $InstallDir "$Product.exe"

# ----------------------------------------------------------------- uninstall
if ($Uninstall) {
    if (Test-Path -LiteralPath $Exe) {
        # Stop and unregister the background service; this never deletes data.
        & $Exe service uninstall
        if ($LASTEXITCODE -ne 0) {
            Write-Warning "could not remove the background service; remove the ReasonNetwork task in Task Scheduler if it exists"
        }
        Remove-Item -LiteralPath $Exe -Force
        Get-ChildItem -LiteralPath $InstallDir -Filter "$Product.exe.old*" -ErrorAction SilentlyContinue |
            Remove-Item -Force -ErrorAction SilentlyContinue
        Write-Output "removed $Exe"
    } else {
        Write-Output "$Exe is not installed"
    }
    if ((Test-Path -LiteralPath $InstallDir) -and -not (Get-ChildItem -LiteralPath $InstallDir -Force)) {
        Remove-Item -LiteralPath $InstallDir -Force
    }
    if (Test-OnUserPath $InstallDir) {
        $kept = ([Environment]::GetEnvironmentVariable("Path", "User") -split ";" |
                Where-Object { $_ -and ($_.TrimEnd("\") -ine $InstallDir.TrimEnd("\")) }) -join ";"
        [Environment]::SetEnvironmentVariable("Path", $kept, "User")
        Write-Output "removed $InstallDir from the user PATH"
    }
    $data = Get-DataDir
    if ($PurgeData) {
        if (Test-Path -LiteralPath $data) {
            Remove-Item -LiteralPath $data -Recurse -Force
            Write-Output "deleted data directory $data"
        }
    } else {
        Write-Output "data directory kept: $data (pass -PurgeData to delete it)"
    }
    Write-Output "agent configurations may still list the MCP server; remove it with"
    Write-Output "  claude mcp remove reason-network --scope user"
    Write-Output "  codex mcp remove reason-network"
    return
}
if ($PurgeData) { Fail "-PurgeData is only valid with -Uninstall" }

# ------------------------------------------------------------------ platform
if (-not $Target) {
    $arch = $env:PROCESSOR_ARCHITECTURE
    if ($env:PROCESSOR_ARCHITEW6432) { $arch = $env:PROCESSOR_ARCHITEW6432 }
    switch ($arch) {
        "AMD64" { $Target = "x86_64-pc-windows-msvc" }
        "ARM64" { Fail "no ARM64 Windows build is published; the x64 build runs under emulation: pass -Target x86_64-pc-windows-msvc" }
        default { Fail "no release is built for Windows $arch" }
    }
}
if ($SupportedTargets -notcontains $Target) { Fail "unsupported target $Target (use install.sh on macOS and Linux)" }

# ------------------------------------------------------------------- source
if (-not $FromDir) {
    if (-not $Repo) { Fail "no repository given: pass -Repo OWNER/NAME or set REASON_NETWORK_REPO (or use -FromDir)" }
    if ($Repo -notmatch '^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$') { Fail "-Repo must look like OWNER/NAME" }
    # Windows PowerShell 5.1 defaults to TLS 1.0.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} elseif (-not (Test-Path -LiteralPath $FromDir -PathType Container)) {
    Fail "-FromDir $FromDir is not a directory"
}

$Tmp = Join-Path ([IO.Path]::GetTempPath()) ("$Product-install-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $Tmp | Out-Null

function Get-ReleaseFile([string]$Name, [string]$Destination, [string]$ForVersion) {
    if ($FromDir) {
        $source = Join-Path $FromDir $Name
        if (-not (Test-Path -LiteralPath $source)) { Fail "$source does not exist" }
        Copy-Item -LiteralPath $source -Destination $Destination
        return
    }
    if ($ForVersion) {
        $url = "https://github.com/$Repo/releases/download/v$ForVersion/$Name"
    } else {
        $url = "https://github.com/$Repo/releases/latest/download/$Name"
    }
    try {
        Invoke-WebRequest -Uri $url -OutFile $Destination -UseBasicParsing
    } catch {
        Fail "cannot download $url ($($_.Exception.Message)); has the release been published?"
    }
}

try {
    # --------------------------------------------------------------- version
    $Version = $Version.TrimStart("v")
    $sumsPath = Join-Path $Tmp "SHA256SUMS"
    Get-ReleaseFile "SHA256SUMS" $sumsPath $Version
    $sums = @{}
    foreach ($line in Get-Content -LiteralPath $sumsPath) {
        if ($line -match '^([0-9A-Fa-f]{64})\s+\*?(\S+)\s*$') { $sums[$Matches[2]] = $Matches[1].ToLowerInvariant() }
    }
    if (-not $Version) {
        $pattern = '^' + [regex]::Escape("$Product-") + '(\d+\.\d+\.\d+(?:-[0-9A-Za-z.]+)?)' + [regex]::Escape("-$Target.zip") + '$'
        $versions = @($sums.Keys | ForEach-Object { if ($_ -match $pattern) { $Matches[1] } })
        if ($versions.Count -eq 0) { Fail "SHA256SUMS lists no package for $Target" }
        $Version = $versions | Sort-Object { [version](($_ -split '-')[0]) }, { $_ } | Select-Object -Last 1
    }
    if ($Version -notmatch '^\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?$') { Fail "invalid version $Version" }
    $package = "$Product-$Version-$Target.zip"
    if (-not $sums.ContainsKey($package)) { Fail "$package is not listed in SHA256SUMS (version $Version has no $Target package)" }

    # -------------------------------------------------------------- download
    Write-Output "downloading $package"
    $packagePath = Join-Path $Tmp $package
    Get-ReleaseFile $package $packagePath $Version
    $actual = (Get-FileHash -LiteralPath $packagePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $sums[$package]) {
        Fail "checksum mismatch for ${package}: expected $($sums[$package]), got $actual; nothing was installed"
    }
    Write-Output "verified SHA-256 $actual"

    # --------------------------------------------------------------- install
    $extract = Join-Path $Tmp "extract"
    Expand-Archive -LiteralPath $packagePath -DestinationPath $extract
    $new = Join-Path $extract "$Product-$Version-$Target\$Product.exe"
    if (-not (Test-Path -LiteralPath $new)) { Fail "$package does not contain $Product.exe" }
    $reported = ""
    try { $reported = (& $new --version) -join "" } catch { $reported = "" }
    if ($reported -notlike "$Product *") { Fail "the downloaded executable does not run on this machine; nothing was installed" }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    $previous = ""
    if (Test-Path -LiteralPath $Exe) {
        try { $previous = (& $Exe --version) -join "" } catch { $previous = "" }
    }
    $staged = Join-Path $InstallDir ".$Product.new-$PID.exe"
    Copy-Item -LiteralPath $new -Destination $staged -Force
    if (Test-Path -LiteralPath $Exe) {
        # A running executable cannot be overwritten, but it can be renamed.
        $old = "$Exe.old"
        if (Test-Path -LiteralPath $old) {
            try { Remove-Item -LiteralPath $old -Force } catch { $old = "$Exe.old-$PID" }
        }
        Move-Item -LiteralPath $Exe -Destination $old
        try {
            Move-Item -LiteralPath $staged -Destination $Exe
        } catch {
            Move-Item -LiteralPath $old -Destination $Exe
            Fail "cannot install the new executable; the previous one was restored"
        }
    } else {
        Move-Item -LiteralPath $staged -Destination $Exe
    }

    if ($previous) {
        Write-Output "upgraded $previous -> $reported at $Exe"
    } else {
        Write-Output "installed $reported at $Exe"
    }
    Write-Output "configuration and data were not modified (data directory: $(Get-DataDir))"

    if ($AddToPath) {
        if (-not (Test-OnUserPath $InstallDir)) {
            $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
            $newPath = if ($userPath) { "$($userPath.TrimEnd(';'));$InstallDir" } else { $InstallDir }
            [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
            Write-Output "added $InstallDir to the user PATH (open a new terminal)"
        }
    } elseif (-not (Test-OnUserPath $InstallDir)) {
        Write-Output ""
        Write-Output "$InstallDir is not on your PATH. Re-run with -AddToPath, or add it yourself:"
        Write-Output "  [Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path','User') + ';$InstallDir', 'User')"
    }
    Write-Output ""
    Write-Output "next steps:"
    Write-Output "  $Product setup --agent all      # configure Claude Code and Codex"
    Write-Output "  $Product service install; $Product service start"
    Write-Output "  $Product doctor"
    if ($previous) {
        Write-Output "  restart a running daemon to use the new version: $Product service stop; $Product service start"
    }
} finally {
    Remove-Item -LiteralPath $Tmp -Recurse -Force -ErrorAction SilentlyContinue
}
