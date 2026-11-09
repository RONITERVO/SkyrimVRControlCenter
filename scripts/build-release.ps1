param(
    [switch]$Online,
    [switch]$SkipTests
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    if (-not $SkipTests) {
        Write-Host "==> Running workspace tests..."
        $testArgs = @("test", "--workspace")
        if (-not $Online) { $testArgs += "--offline" }
        & cargo @testArgs
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

        Write-Host "==> Running clippy..."
        $clippyArgs = @("clippy", "--workspace")
        if (-not $Online) { $clippyArgs += "--offline" }
        $clippyArgs += @("--", "-D", "warnings")
        & cargo @clippyArgs
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    }

    Write-Host "==> Building release binaries..."
    $buildArgs = @("build", "--workspace", "--release")
    if (-not $Online) { $buildArgs += "--offline" }
    & cargo @buildArgs
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    foreach ($bin in @("skyrim-vr-control-center.exe", "skyrim-bard-engine.exe", "kat-overlay-stub.exe")) {
        $src = Join-Path $root "target\release\$bin"
        $dst = Join-Path $root $bin
        if (Test-Path -LiteralPath $dst) {
            $srcHash = (Get-FileHash -LiteralPath $src -Algorithm SHA256).Hash
            $dstHash = (Get-FileHash -LiteralPath $dst -Algorithm SHA256).Hash
            if ($srcHash -eq $dstHash) {
                Write-Host "Up-to-date $dst (unchanged)"
                continue
            }
        }
        Copy-Item -LiteralPath $src -Destination $dst -Force
        Write-Host "Staged $dst"
    }

    & (Join-Path $root "kat-overlay-stub.exe") --self-test
    if ($LASTEXITCODE -ne 0) { throw "kat-overlay-stub.exe --self-test failed with code $LASTEXITCODE" }

    Write-Host "==> Build and staging complete."
} finally {
    Pop-Location
}
