# Claw Plus Windows one-click installer
#
# Mirrors install.sh (Unix): detect environment -> check Rust toolchain ->
# build claw binaries -> deploy to ~/.cargo/bin (already on PATH) -> verify ->
# guide API key setup.
#
# Usage (PowerShell, from repo root):
#   ./install.ps1                 # debug build (faster)
#   ./install.ps1 -Release        # release build (optimized, slower compile)
#   ./install.ps1 -SkipVerify     # skip post-install verification
#   ./install.ps1 -Help           # show help
#
# Environment overrides:
#   CLAW_BUILD_PROFILE=release    same as -Release
#   CLAW_SKIP_VERIFY=1            same as -SkipVerify

[CmdletBinding()]
param(
    [switch]$Release,
    [switch]$SkipVerify,
    [switch]$Help
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

# ---- colored output (mirrors install.sh) ----
$ColorReset = "$([char]27)[0m"
$ColorBold  = "$([char]27)[1m"
$ColorDim   = "$([char]27)[2m"
$ColorRed   = "$([char]27)[31m"
$ColorGreen = "$([char]27)[32m"
$ColorYellow= "$([char]27)[33m"
$ColorBlue  = "$([char]27)[34m"
$ColorCyan  = "$([char]27)[36m"

$CURRENT_STEP = 0
$TOTAL_STEPS = 6

function Step($Title) {
    $script:CURRENT_STEP += 1
    Write-Host ""
    Write-Host "${ColorBlue}[$($script:CURRENT_STEP)/$TOTAL_STEPS]${ColorReset} ${ColorBold}$Title${ColorReset}"
}
function Info($Msg)  { Write-Host "  ${ColorCyan}->${ColorReset} $Msg" }
function Ok($Msg)    { Write-Host "  ${ColorGreen}ok${ColorReset} $Msg" }
function Warn($Msg)  { Write-Host "  ${ColorYellow}warn${ColorReset} $Msg" }
function Error2($Msg){ Write-Host "  ${ColorRed}error${ColorReset} $Msg" }

function Print-Banner {
    Write-Host "${ColorBold}"
    Write-Host "   ____  _                   ____          _"
    Write-Host "  / ___|| |  __ _ __      __ / ___|___   __| | ___"
    Write-Host " | |    | | / _` |\ \ /\ / /| |   / _ \ / _` |/ _ \"
    Write-Host " | |___ | || (_| | \ V  V / | |__| (_) | (_| |  __/"
    Write-Host "  \____||_| \__,_|  \_/\_/   \____\___/ \__,_|\___|"
    Write-Host "${ColorReset}"
    Write-Host "${ColorDim}Claw Plus Windows installer${ColorReset}"
}

function Print-Help {
    Write-Host "Usage: ./install.ps1 [options]"
    Write-Host ""
    Write-Host "Options:"
    Write-Host "  -Release        Build the optimized release profile."
    Write-Host "  -SkipVerify     Skip the post-install verification step."
    Write-Host "  -Help           Show this help text and exit."
    Write-Host ""
    Write-Host "Environment overrides:"
    Write-Host "  CLAW_BUILD_PROFILE   debug | release"
    Write-Host "  CLAW_SKIP_VERIFY     set to 1 to skip verification"
}

# ---- argument parsing ----
if ($Help) { Print-Help; exit 0 }

$BUILD_PROFILE = if ($env:CLAW_BUILD_PROFILE) { $env:CLAW_BUILD_PROFILE } else { 'debug' }
if ($Release -or $env:CLAW_BUILD_PROFILE -eq 'release') { $BUILD_PROFILE = 'release' }
if ($env:CLAW_SKIP_VERIFY -eq '1') { $SkipVerify = $true }

# ---- locate repo root (script dir = repo root) ----
$SCRIPT_DIR = Split-Path -Parent $MyInvocation.MyCommand.Path
$RUST_DIR = Join-Path $SCRIPT_DIR 'rust'

Print-Banner

# =========================================================
# Step 1: detect environment
# =========================================================
Step "Detecting environment"
$OsVer = [System.Environment]::OSVersion.VersionString
Info "OS: $OsVer"
if (-not (Test-Path (Join-Path $RUST_DIR 'Cargo.toml'))) {
    Error2 "rust/Cargo.toml not found. Run this script from the repo root (same dir as install.sh)."
    exit 1
}
Ok "Rust workspace located: $RUST_DIR"

# =========================================================
# Step 2: check Rust toolchain
# =========================================================
Step "Checking Rust toolchain"
$MISSING = @()
foreach ($cmd in @('rustc', 'cargo')) {
    if (Get-Command $cmd -ErrorAction SilentlyContinue) {
        $ver = & $cmd --version 2>$null
        Ok "$cmd found: $ver"
    } else {
        Error2 "$cmd not found in PATH"
        $MISSING += $cmd
    }
}
if ($MISSING.Count -gt 0) {
    Error2 "Missing required tools: $($MISSING -join ', ')"
    Write-Host ""
    Write-Host "${ColorBold}How to install Rust:${ColorReset}"
    Write-Host "  Option 1 (recommended): download rustup-init.exe from https://rustup.rs"
    Write-Host "  Option 2 (winget):       winget install Rustlang.Rustup"
    Write-Host "  Reopen PowerShell after installing, then run this script again."
    exit 1
}

# =========================================================
# Step 3: build claw binaries
# =========================================================
Step "Building claw ($BUILD_PROFILE)"
$CARGO_FLAGS = @('build', '-p', 'rusty-claude-cli', '--bin', 'claw', '--bin', 'claw-plus-headless')
if ($BUILD_PROFILE -eq 'release') { $CARGO_FLAGS += '--release' }
Info "Running: cargo $($CARGO_FLAGS -join ' ')"
Info "First build may take a few minutes. Please wait..."
Push-Location $RUST_DIR
try {
    cargo $CARGO_FLAGS
    if ($LASTEXITCODE -ne 0) {
        Error2 "Build failed (cargo exit code $LASTEXITCODE). See errors above."
        exit 1
    }
} finally {
    Pop-Location
}
Ok "Build complete"

# =========================================================
# Step 4: deploy to ~/.cargo/bin (already on PATH)
# =========================================================
Step "Deploying to ~/.cargo/bin"
$DEPLOY_DIR = Join-Path $HOME '.cargo\bin'
if (-not (Test-Path $DEPLOY_DIR)) {
    New-Item -ItemType Directory -Path $DEPLOY_DIR -Force | Out-Null
}
$BIN_SRC = Join-Path $RUST_DIR "target\$BUILD_PROFILE"
$BINARIES = @('claw.exe', 'claw-plus-headless.exe')
foreach ($bin in $BINARIES) {
    $src = Join-Path $BIN_SRC $bin
    if (-not (Test-Path $src)) {
        Error2 "Build artifact not found: $src"
        exit 1
    }
    Copy-Item -Path $src -Destination (Join-Path $DEPLOY_DIR $bin) -Force
    Ok "Deployed $bin -> $DEPLOY_DIR"
}
Write-Host ""
Info "If 'claw' is not on this terminal yet, open a new PowerShell window ($DEPLOY_DIR is on PATH)."

# =========================================================
# Step 5: verify
# =========================================================
Step "Verifying installation"
if ($SkipVerify) {
    Warn "Verification skipped (-SkipVerify)"
} else {
    $claw = Join-Path $DEPLOY_DIR 'claw.exe'
    Info "Running: claw --version"
    $ver = & $claw --version 2>&1
    if ($LASTEXITCODE -eq 0) { Ok "claw --version -> $ver" } else { Error2 $ver; exit 1 }
    Info "Running: claw --help (smoke test)"
    & $claw --help *> $null
    if ($LASTEXITCODE -eq 0) { Ok "claw --help responded" } else { Error2 "claw --help failed"; exit 1 }
}

# =========================================================
# Step 6: guide API key setup
# =========================================================
Step "API Key setup (optional)"
if ($env:DEEPSEEK_API_KEY) {
    Ok "DEEPSEEK_API_KEY already set in environment"
} else {
    Write-Host ""
    Write-Host "  Claw needs a DeepSeek API Key to work."
    Write-Host "  Get one at: https://platform.deepseek.com/api_keys"
    Write-Host ""
    $choice = Read-Host "  Configure now? (y/N)"
    if ($choice -match '^(y|Y|yes)$') {
        $key = Read-Host "  Paste your API Key"
        if (-not [string]::IsNullOrWhiteSpace($key)) {
            [System.Environment]::SetEnvironmentVariable('DEEPSEEK_API_KEY', $key.Trim(), 'User')
            # also set for current session
            $env:DEEPSEEK_API_KEY = $key.Trim()
            Ok "DEEPSEEK_API_KEY saved (user-level env var, permanent)"
        } else {
            Warn "No key entered, skipping. Configure later with: setx DEEPSEEK_API_KEY your_key"
        }
    } else {
        Warn "Skipped. Configure later with: setx DEEPSEEK_API_KEY your_key"
    }
}

# =========================================================
# done
# =========================================================
Write-Host ""
Write-Host "${ColorGreen}Claw Plus installed successfully!${ColorReset}"
Write-Host ""
Write-Host "  Try these commands:"
Write-Host "    ${ColorBold}claw${ColorReset}                  # interactive REPL"
Write-Host "    ${ColorBold}claw acp serve${ColorReset}       # ACP server (IDE integration)"
Write-Host ""
Write-Host "  VS Code IDE integration: install the Claw Plus extension from the"
Write-Host "  marketplace, then set the API key in extension settings."
Write-Host ""
