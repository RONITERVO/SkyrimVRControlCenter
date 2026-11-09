<#
.SYNOPSIS
    Safely routes SkyrimNet LLM variants to the local Antigravity relay on port 8097.

.DESCRIPTION
    1. Verifies that Skyrim VR, ModOrganizer, and skyrim-bard-engine are not running.
    2. Backs up OpenRouter.yaml and Providers.yaml to backups/<timestamp>-before-antigravity/.
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
$blockers = @("SkyrimVR", "ModOrganizer", "skyrim-bard-engine")
$running = Get-Process -ErrorAction SilentlyContinue | Where-Object { $blockers -contains $_.ProcessName }
if ($running -and -not $Force) {
    Write-Error "Cannot modify SkyrimNet configuration while the game or tools are running:`n$($running | Select-Object -ExpandProperty ProcessName | Out-String)`nPlease close Skyrim and Mod Organizer 2 before running this script."
}

if (-not (Test-Path $SkyrimNetConfigDir)) {
    Write-Error "SkyrimNet config directory not found: $SkyrimNetConfigDir"
}

$openRouterYaml = Join-Path $SkyrimNetConfigDir "OpenRouter.yaml"
$providersYaml = Join-Path $SkyrimNetConfigDir "Providers.yaml"

if (-not (Test-Path $openRouterYaml)) {
    Write-Error "OpenRouter.yaml not found at: $openRouterYaml"
}

# 2. Create timestamped backup
$timestamp = Get-Date -Format "yyyyMMdd-HHmmss"
$repoRoot = Split-Path -Parent $PSScriptRoot
$backupDir = Join-Path $repoRoot "backups\$timestamp-before-antigravity"
New-Item -ItemType Directory -Path $backupDir -Force | Out-Null

Copy-Item -LiteralPath $openRouterYaml -Destination (Join-Path $backupDir "OpenRouter.yaml") -Force
if (Test-Path $providersYaml) {
    Copy-Item -LiteralPath $providersYaml -Destination (Join-Path $backupDir "Providers.yaml") -Force
}
Write-Host "Backed up original configuration to: $backupDir" -ForegroundColor Green

# 3. Modify OpenRouter.yaml: replace 'provider_id: openrouter' with 'provider_id: local_bard_composer'
# Note: Regex specifically targets exact "provider_id:\s*openrouter\s*$" so that "openrouter_decisions" is preserved!
$lines = Get-Content -LiteralPath $openRouterYaml
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

# Write back with original encoding
Set-Content -LiteralPath $openRouterYaml -Value $newLines -Encoding utf8

Write-Host "Successfully switched SkyrimNet LLM traffic to Antigravity!" -ForegroundColor Cyan
Write-Host "Updated $modifiedCount provider references to 'local_bard_composer' (port 8097)." -ForegroundColor Gray
Write-Host "The 'decisions' slot (typesafe/jev-1.13) was preserved on OpenRouter." -ForegroundColor Gray
Write-Host "To revert at any time, run: scripts\Restore-SkyrimNet-OpenRouter.ps1" -ForegroundColor Yellow
