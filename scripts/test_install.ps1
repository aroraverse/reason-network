<#
.SYNOPSIS
    Tests for install.ps1 against local release fixtures.

.DESCRIPTION
    pwsh -File scripts/test_install.ps1 -Binary target/debug/reason-network.exe

    Builds fake releases with scripts/package.py from a real Windows build,
    then installs into a temporary directory with a temporary data directory.
    The user PATH, the real installation and real data are never touched, and
    nothing is downloaded.
#>
param(
    [Parameter(Mandatory = $true)][string]$Binary,
    [string]$Python = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$Root = Split-Path -Parent $PSScriptRoot
$Installer = Join-Path $Root "install.ps1"
if (-not $Python) {
    $Python = (Get-Command -Name python3, python -CommandType Application -ErrorAction SilentlyContinue |
            Where-Object { $_.Source -notlike "*WindowsApps*" } | Select-Object -First 1).Source
    if (-not $Python) { throw "python is required to build the test fixtures" }
}
$Binary = (Resolve-Path -LiteralPath $Binary).Path
$Work = Join-Path ([IO.Path]::GetTempPath()) ("rn-install-ps-test-" + [Guid]::NewGuid().ToString("N"))
$Target = "x86_64-pc-windows-msvc"
$script:Passed = 0

function Fail([string]$Message) { throw "FAIL: $Message" }
function Pass([string]$Message) { $script:Passed++; Write-Output "ok - $Message" }

# Run the installer in a child process; return @{ Code; Output }.
function Invoke-Installer([string[]]$Arguments) {
    $shell = (Get-Process -Id $PID).Path
    # Windows PowerShell 5.1 turns redirected native stderr into errors.
    $ErrorActionPreference = "Continue"
    $output = & $shell -NoProfile -ExecutionPolicy Bypass -File $Installer @Arguments 2>&1 |
        ForEach-Object { "$_" } | Out-String
    return @{ Code = $LASTEXITCODE; Output = $output }
}

function Build-Release([string]$Dir, [string[]]$Versions) {
    New-Item -ItemType Directory -Path $Dir -Force | Out-Null
    foreach ($version in $Versions) {
        # Distinct bytes per version: an appended overlay does not affect a PE image.
        $copy = Join-Path $Work "build-$version.exe"
        Copy-Item -LiteralPath $Binary -Destination $copy
        [IO.File]::AppendAllText($copy, "reason-network-test-$version")
        & $Python (Join-Path $Root "scripts\package.py") package --binary $copy --target $Target --version $version --out $Dir | Out-Null
        if ($LASTEXITCODE -ne 0) { Fail "packaging $version failed" }
    }
    & $Python (Join-Path $Root "scripts\package.py") sums --out $Dir | Out-Null
}

try {
    New-Item -ItemType Directory -Path $Work | Out-Null
    $env:REASON_NETWORK_DATA_DIR = Join-Path $Work "data"
    New-Item -ItemType Directory -Path $env:REASON_NETWORK_DATA_DIR | Out-Null
    Set-Content -LiteralPath (Join-Path $env:REASON_NETWORK_DATA_DIR "reason-network.db") -Value "precious"
    $Dest = Join-Path $Work "bin"
    $Exe = Join-Path $Dest "reason-network.exe"
    $Rel = Join-Path $Work "rel"
    Build-Release $Rel @("0.1.0", "0.2.0")
    $common = @("-Target", $Target, "-InstallDir", $Dest)

    function Test-DataIntact {
        if ((Get-Content -LiteralPath (Join-Path $env:REASON_NETWORK_DATA_DIR "reason-network.db")) -ne "precious") { Fail "data was modified" }
    }
    function Get-Hash([string]$Path) { (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash }

    # Fresh install picks the newest version.
    $r = Invoke-Installer ($common + @("-FromDir", $Rel))
    if ($r.Code -ne 0) { Fail "install failed: $($r.Output)" }
    if ((Get-Hash $Exe) -ne (Get-Hash (Join-Path $Rel "reason-network-0.2.0-$Target.exe"))) { Fail "0.2.0 not installed" }
    if ($r.Output -notmatch "verified SHA-256") { Fail "no verification message" }
    if ($r.Output -notmatch "is not on your PATH") { Fail "no PATH hint" }
    if ((& $Exe --version) -notlike "reason-network *") { Fail "installed executable does not run" }
    Test-DataIntact
    Pass "fresh install selects the latest version and explains PATH"

    # Running again is safe, also while the executable is in use.
    $proc = Start-Process -FilePath $Exe -ArgumentList "daemon-status" -PassThru -WindowStyle Hidden
    $r = Invoke-Installer ($common + @("-FromDir", $Rel))
    if ($r.Code -ne 0) { Fail "re-run failed: $($r.Output)" }
    $proc.WaitForExit()
    $r = Invoke-Installer ($common + @("-FromDir", $Rel))
    if ($r.Code -ne 0) { Fail "third run failed: $($r.Output)" }
    if (Get-ChildItem -LiteralPath $Dest -Filter ".reason-network.new-*") { Fail "staged file left behind" }
    Pass "repeated install is idempotent"

    # A specific version (downgrade), then an upgrade back.
    $r = Invoke-Installer ($common + @("-FromDir", $Rel, "-Version", "0.1.0"))
    if ($r.Code -ne 0) { Fail "pinned install failed: $($r.Output)" }
    if ((Get-Hash $Exe) -ne (Get-Hash (Join-Path $Rel "reason-network-0.1.0-$Target.exe"))) { Fail "0.1.0 not installed" }
    $r = Invoke-Installer ($common + @("-FromDir", $Rel, "-Version", "v0.2.0"))
    if ($r.Output -notmatch "upgraded") { Fail "no upgrade message: $($r.Output)" }
    if ((Get-Hash $Exe) -ne (Get-Hash (Join-Path $Rel "reason-network-0.2.0-$Target.exe"))) { Fail "upgrade not installed" }
    Test-DataIntact
    Pass "specific versions and upgrades preserve data"

    # Unknown version.
    $r = Invoke-Installer ($common + @("-FromDir", $Rel, "-Version", "9.9.9"))
    if ($r.Code -eq 0 -or $r.Output -notmatch "not listed in SHA256SUMS") { Fail "unknown version: $($r.Output)" }
    Pass "an unknown version is rejected"

    # Tampered package.
    $Tampered = Join-Path $Work "tampered"
    Copy-Item -LiteralPath $Rel -Destination $Tampered -Recurse
    [IO.File]::AppendAllText((Join-Path $Tampered "reason-network-0.1.0-$Target.zip"), "x")
    $before = Get-Hash $Exe
    $r = Invoke-Installer ($common + @("-FromDir", $Tampered, "-Version", "0.1.0"))
    if ($r.Code -eq 0 -or $r.Output -notmatch "checksum mismatch") { Fail "tampered package: $($r.Output)" }
    if ((Get-Hash $Exe) -ne $before) { Fail "install changed after a failed verification" }
    Pass "a tampered package is rejected without touching the install"

    # Errors with clear messages.
    $r = Invoke-Installer ($common + @("-FromDir", (Join-Path $Work "missing")))
    if ($r.Code -eq 0 -or $r.Output -notmatch "is not a directory") { Fail "missing dir: $($r.Output)" }
    $saved = $env:REASON_NETWORK_REPO; $env:REASON_NETWORK_REPO = ""
    $r = Invoke-Installer $common
    $env:REASON_NETWORK_REPO = $saved
    if ($r.Code -eq 0 -or $r.Output -notmatch "no repository given") { Fail "no repo: $($r.Output)" }
    $r = Invoke-Installer @("-Target", "aarch64-apple-darwin", "-InstallDir", $Dest, "-FromDir", $Rel)
    if ($r.Code -eq 0 -or $r.Output -notmatch "unsupported target") { Fail "unsupported target: $($r.Output)" }
    $r = Invoke-Installer ($common + @("-FromDir", $Rel, "-PurgeData"))
    if ($r.Code -eq 0 -or $r.Output -notmatch "only valid with -Uninstall") { Fail "purge without uninstall: $($r.Output)" }
    Pass "missing inputs and unsupported platforms are reported"

    # Uninstall keeps data.
    $r = Invoke-Installer ($common + @("-Uninstall"))
    if ($r.Code -ne 0) { Fail "uninstall failed: $($r.Output)" }
    if (Test-Path -LiteralPath $Exe) { Fail "executable not removed" }
    if ($r.Output -notmatch "data directory kept") { Fail "no data message: $($r.Output)" }
    if ($r.Output -notmatch "was not installed|removed service") { Fail "service not handled: $($r.Output)" }
    Test-DataIntact
    $r = Invoke-Installer ($common + @("-Uninstall"))
    if ($r.Output -notmatch "is not installed") { Fail "second uninstall: $($r.Output)" }
    Pass "uninstall removes the executable and service but keeps data"

    # Purge on request only.
    Invoke-Installer ($common + @("-FromDir", $Rel)) | Out-Null
    $r = Invoke-Installer ($common + @("-Uninstall", "-PurgeData"))
    if ($r.Code -ne 0 -or (Test-Path -LiteralPath $env:REASON_NETWORK_DATA_DIR)) { Fail "data not purged: $($r.Output)" }
    Pass "-PurgeData deletes the data directory"

    Write-Output ""
    Write-Output "$($script:Passed) install.ps1 tests passed"
} finally {
    Remove-Item -LiteralPath $Work -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item Env:\REASON_NETWORK_DATA_DIR -ErrorAction SilentlyContinue
}
