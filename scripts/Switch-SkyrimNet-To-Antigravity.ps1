<#
.SYNOPSIS
    Safely routes SkyrimNet LLM variants to the local Antigravity relay on port 8097.

.DESCRIPTION
    1. Verifies that Skyrim VR and ModOrganizer are not running.
    2. Backs up OpenRouter.yaml to backups/<timestamp>-before-antigravity/. (Providers.yaml is untouched).
    3. Updates 'provider_id: openrouter' to 'provider_id: local_bard_composer' in OpenRouter.yaml,
       repointing all NPC dialogue, combat, evaluation, and vision slots to Antigravity.
    4. Leaves 'decisions' on 'openrouter_decisions' (cheap typesafe/jev model).
    5. Preserves all API keys, comments, and config structure untouched.
#>

[CmdletBinding()]
param(
    [string]$SkyrimNetConfigDir = "D:\SkyrimVR MGO\overwrite\SKSE\Plugins\SkyrimNet\config",
    [switch]$Force
)

$ErrorActionPreference = "Stop"

# 1. Check for running processes
$blockers = @("SkyrimVR", "ModOrganizer", "ModOrganizer_original")
$running = Get-Process -ErrorAction SilentlyContinue | Where-Object { $blockers -contains $_.ProcessName }
if ($running -and -not $Force) {
    Write-Error "Cannot modify SkyrimNet configuration while the game or MO2 is running:`n$($running | Select-Object -ExpandProperty ProcessName | Out-String)`nPlease close Skyrim and Mod Organizer 2 before running this script."
}

if (-not (Test-Path $SkyrimNetConfigDir)) {
    Write-Error "SkyrimNet config directory not found: $SkyrimNetConfigDir"
}

$openRouterYaml = Join-Path $SkyrimNetConfigDir "OpenRouter.yaml"
$providersYaml = Join-Path $SkyrimNetConfigDir "Providers.yaml"

if (-not (Test-Path $openRouterYaml)) {
    Write-Error "OpenRouter.yaml not found at: $openRouterYaml"
}

# 2. Inspect before backing up so repeated runs never overwrite clean pre-Antigravity backups
$lines = Get-Content -LiteralPath $openRouterYaml
$openRouterMatches = @($lines | Where-Object { $_ -match '^\s*-\s*provider_id:\s*openrouter\s*$' }).Count
if ($openRouterMatches -eq 0) {
    Write-Host "OpenRouter.yaml is already routed to Antigravity (0 openrouter slots left); preserving existing backups." -ForegroundColor Yellow
    return
}

# 3. Create timestamped backup
$timestamp = Get-Date -Format "yyyyMMdd-HHmmss"
$repoRoot = Split-Path -Parent $PSScriptRoot
$backupDir = Join-Path $repoRoot "backups\$timestamp-before-antigravity"
New-Item -ItemType Directory -Path $backupDir -Force | Out-Null

Copy-Item -LiteralPath $openRouterYaml -Destination (Join-Path $backupDir "OpenRouter.yaml") -Force
Write-Host "Backed up original OpenRouter.yaml configuration to: $backupDir" -ForegroundColor Green

# 4. Modify OpenRouter.yaml: replace 'provider_id: openrouter' with 'provider_id: local_bard_composer'
# Note: Regex specifically targets exact "provider_id:\s*openrouter\s*$" so that "openrouter_decisions" is preserved!
$modifiedCount = 0
$newLines = @()

foreach ($line in $lines) {
    if ($line -match '^(\s*-\s*provider_id:\s*)openrouter\s*$') {
        $newLine = "$($matches[1])local_bard_composer"
        $newLines += $newLine
        $modifiedCount++
    } else {
        $newLines += $line
    }
}

# Write back cleanly as UTF-8 without BOM
[System.IO.File]::WriteAllLines($openRouterYaml, $newLines, (New-Object System.Text.UTF8Encoding($false)))

Write-Host "Successfully switched SkyrimNet LLM traffic to Antigravity!" -ForegroundColor Cyan
Write-Host "Updated $modifiedCount provider references to 'local_bard_composer' (port 8097)." -ForegroundColor Gray
Write-Host "The 'decisions' slot (typesafe/jev-1.13) was preserved on OpenRouter." -ForegroundColor Gray
Write-Host "To revert at any time, run: scripts\Restore-SkyrimNet-OpenRouter.ps1" -ForegroundColor Yellow

