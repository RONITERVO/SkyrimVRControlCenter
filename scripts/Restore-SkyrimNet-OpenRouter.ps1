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
$blockers = @("SkyrimVR", "ModOrganizer", "ModOrganizer_original")
$running = Get-Process -ErrorAction SilentlyContinue | Where-Object { $blockers -contains $_.ProcessName }
if ($running -and -not $Force) {
    Write-Error "Cannot restore SkyrimNet configuration while the game or MO2 is running:`n$($running | Select-Object -ExpandProperty ProcessName | Out-String)`nPlease close Skyrim and Mod Organizer 2 before running this script."
}

$repoRoot = Split-Path -Parent $PSScriptRoot

if (-not $BackupPath) {
    $candidates = Get-ChildItem -Path (Join-Path $repoRoot "backups") -Directory -Filter "*-before-antigravity" | Sort-Object Name -Descending
    $latestBackup = $candidates | Where-Object {
        $yaml = Join-Path $_.FullName "OpenRouter.yaml"
        (Test-Path $yaml) -and ((Get-Content -LiteralPath $yaml | Where-Object { $_ -match '^\s*-\s*provider_id:\s*openrouter\s*$' }).Count -gt 0)
    } | Select-Object -First 1
    if (-not $latestBackup) {
        Write-Error "No valid pre-Antigravity *-before-antigravity backups containing provider_id: openrouter found in: $(Join-Path $repoRoot 'backups')"
    }
    $BackupPath = $latestBackup.FullName
}

$sourceYaml = Join-Path $BackupPath "OpenRouter.yaml"
if (-not (Test-Path $sourceYaml)) {
    Write-Error "Backup OpenRouter.yaml not found at: $sourceYaml"
}

$destYaml = Join-Path $SkyrimNetConfigDir "OpenRouter.yaml"

# Safety backup of the current state before restoring
$timestamp = Get-Date -Format "yyyyMMdd-HHmmss"
$safetyBackupDir = Join-Path $repoRoot "backups\$timestamp-before-openrouter-restore"
New-Item -ItemType Directory -Path $safetyBackupDir -Force | Out-Null
Copy-Item -LiteralPath $destYaml -Destination (Join-Path $safetyBackupDir "OpenRouter.yaml") -Force
Write-Host "Backed up current OpenRouter.yaml to: $safetyBackupDir" -ForegroundColor Gray

Copy-Item -LiteralPath $sourceYaml -Destination $destYaml -Force

Write-Host "Successfully restored OpenRouter.yaml from: $BackupPath" -ForegroundColor Green

