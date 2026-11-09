<#
.SYNOPSIS
    Restores SkyrimNet OpenRouter configuration from the latest backup.

.DESCRIPTION
    1. Verifies that Skyrim VR and Mod Organizer 2 are not running.
    2. Finds the latest backup in backups/*-before-antigravity/.
    3. Restores OpenRouter.yaml cleanly.
#>

[CmdletBinding()]
param(
    [string]$SkyrimNetConfigDir = "D:\SkyrimVR MGO\overwrite\SKSE\Plugins\SkyrimNet\config",
    [string]$BackupPath,
    [switch]$Force
)

$ErrorActionPreference = "Stop"

# 1. Check for running processes
$blockers = @("SkyrimVR", "ModOrganizer", "skyrim-bard-engine")
$running = Get-Process -ErrorAction SilentlyContinue | Where-Object { $blockers -contains $_.ProcessName }
if ($running -and -not $Force) {
    Write-Error "Cannot restore SkyrimNet configuration while the game or tools are running:`n$($running | Select-Object -ExpandProperty ProcessName | Out-String)`nPlease close Skyrim and Mod Organizer 2 before running this script."
}

$repoRoot = Split-Path -Parent $PSScriptRoot

if (-not $BackupPath) {
    $latestBackup = Get-ChildItem -Path (Join-Path $repoRoot "backups") -Directory -Filter "*-before-antigravity" | Sort-Object CreationTime -Descending | Select-Object -First 1
    if (-not $latestBackup) {
        Write-Error "No *-before-antigravity backups found in: $(Join-Path $repoRoot 'backups')"
    }
    $BackupPath = $latestBackup.FullName
}

$sourceYaml = Join-Path $BackupPath "OpenRouter.yaml"
if (-not (Test-Path $sourceYaml)) {
    Write-Error "Backup OpenRouter.yaml not found at: $sourceYaml"
}

$destYaml = Join-Path $SkyrimNetConfigDir "OpenRouter.yaml"
Copy-Item -LiteralPath $sourceYaml -Destination $destYaml -Force

Write-Host "Successfully restored OpenRouter.yaml from: $BackupPath" -ForegroundColor Green

