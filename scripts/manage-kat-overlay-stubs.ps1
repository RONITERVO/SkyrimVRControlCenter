param(
    [ValidateSet("status", "install", "restore")]
    [string]$Action = "status"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$ccExe = Join-Path $root "skyrim-vr-control-center.exe"
$stubExe = Join-Path $root "kat-overlay-stub.exe"
$journal = Join-Path $root ".local\kat-overlay-stub-journal.json"

if (-not (Test-Path -LiteralPath $ccExe)) {
    throw "Missing $ccExe. Run .\scripts\build-release.ps1 first."
}
if ($Action -eq "install" -and -not (Test-Path -LiteralPath $stubExe)) {
    throw "Missing $stubExe. Run .\scripts\build-release.ps1 first."
}

if ($Action -eq "status") {
    $out = & $ccExe --kat-stub-status --root $root | Out-String
    $code = $LASTEXITCODE
    if ($out.Trim()) { Write-Host $out.Trim() }
    exit $code
}

$isAdmin = ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator
)

$flag = if ($Action -eq "install") { "--kat-stub-install" } else { "--kat-stub-restore" }

if ($isAdmin) {
    $out = & $ccExe $flag --root $root | Out-String
    $code = $LASTEXITCODE
    if ($out.Trim()) {
        Write-Host $out.Trim()
    } elseif ($code -ne 0 -and (Test-Path -LiteralPath $journal)) {
        Get-Content -LiteralPath $journal -Raw | Write-Host
    }
    exit $code
}

if (Test-Path -LiteralPath $journal) {
    Remove-Item -LiteralPath $journal -Force
}

Write-Host "Requesting Windows UAC elevation for '$Action'..."
$proc = Start-Process -FilePath $ccExe -ArgumentList @($flag, "--root", "`"$root`"") -WorkingDirectory $root -Verb RunAs -PassThru -Wait
if (Test-Path -LiteralPath $journal) {
    Get-Content -LiteralPath $journal -Raw | Write-Host
}
if ($proc.ExitCode -ne 0) {
    throw "Elevated KAT overlay stub '$Action' failed with exit code $($proc.ExitCode)."
}

Write-Host "==> Verified current KAT overlay stub status:"
$out = & $ccExe --kat-stub-status --root $root | Out-String
$code = $LASTEXITCODE
if ($out.Trim()) { Write-Host $out.Trim() }
exit $code

