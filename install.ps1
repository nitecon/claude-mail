#Requires -Version 5.1
<#
.SYNOPSIS
    Install or upgrade agent-comms client tools on Windows.
.DESCRIPTION
    Downloads the latest agent-comms release from GitHub and installs
    agent-comms.exe and agent-sync.exe to %USERPROFILE%\.agentic\bin\.
    Adds the directory to the user's PATH if not already present.
#>

$ErrorActionPreference = "Stop"

$Repo = "nitecon/agent-comms"
$Binaries = @("agent-comms.exe", "agent-sync.exe")
$InstallDir = Join-Path $env:USERPROFILE ".agentic\bin"

# --- Helpers ----------------------------------------------------------------

function Info($msg)  { Write-Host "[INFO]  $msg" -ForegroundColor Green }
function Warn($msg)  { Write-Host "[WARN]  $msg" -ForegroundColor Yellow }
function Fail($msg)  { Write-Host "[ERROR] $msg" -ForegroundColor Red; exit 1 }

# --- Resolve latest version -------------------------------------------------

Info "Resolving latest release..."

try {
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -UseBasicParsing
    $LatestTag = $release.tag_name
} catch {
    Fail "Could not determine latest release from GitHub: $_"
}

if (-not $LatestTag) {
    Fail "Could not determine latest release tag."
}

Info "Latest version: $LatestTag"

$ArchiveName = "agent-comms-${LatestTag}-windows-x86_64.zip"
$DownloadUrl = "https://github.com/$Repo/releases/download/$LatestTag/$ArchiveName"

# --- Check existing installation --------------------------------------------

$BinaryPath = Join-Path $InstallDir "agent-comms.exe"
if (Test-Path $BinaryPath) {
    try {
        $currentVersion = & $BinaryPath --version 2>$null
        Info "Existing installation found: $currentVersion"
    } catch {
        Info "Existing installation found (version unknown)"
    }
    Info "Upgrading to $LatestTag..."
} else {
    Info "No existing installation found. Installing fresh."
}

# --- Download and extract ---------------------------------------------------

$TmpDir = Join-Path $env:TEMP "agent-comms-install-$(Get-Random)"
New-Item -ItemType Directory -Path $TmpDir -Force | Out-Null

try {
    Info "Downloading $ArchiveName..."
    $archivePath = Join-Path $TmpDir $ArchiveName
    Invoke-WebRequest -Uri $DownloadUrl -OutFile $archivePath -UseBasicParsing

    Info "Extracting..."
    Expand-Archive -Path $archivePath -DestinationPath $TmpDir -Force

    # --- Install ----------------------------------------------------------------

    if (-not (Test-Path $InstallDir)) {
        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    }

    foreach ($bin in $Binaries) {
        $src = Get-ChildItem -Path $TmpDir -Recurse -Filter $bin | Select-Object -First 1
        if ($src) {
            Copy-Item -Path $src.FullName -Destination (Join-Path $InstallDir $bin) -Force
            Info "Installed $bin"
        } else {
            Write-Host "[WARN]  Binary '$bin' not found in archive, skipping." -ForegroundColor Yellow
        }
    }

} finally {
    Remove-Item -Path $TmpDir -Recurse -Force -ErrorAction SilentlyContinue
}

# --- Add to PATH ------------------------------------------------------------

$userPath = [Environment]::GetEnvironmentVariable("PATH", "User")
if ($userPath -notlike "*$InstallDir*") {
    [Environment]::SetEnvironmentVariable("PATH", "$userPath;$InstallDir", "User")
    $env:PATH = "$env:PATH;$InstallDir"
    Info "Added $InstallDir to user PATH"
} else {
    Info "$InstallDir already in PATH"
}

# --- Done -------------------------------------------------------------------

Write-Host ""
Info "Installation complete!"
Write-Host ""
Write-Host "  Install dir: $InstallDir"
Write-Host "  Version:     $LatestTag"
Write-Host ""
Write-Host "Quick start:"
Write-Host "  agent-comms init                    # Interactive setup"
Write-Host "  agent-sync skills push .\skill-dir   # Push a skill to gateway"
Write-Host ""
Write-Host "Register as MCP server for Claude Code:"
Write-Host "  claude mcp add agent-comms -- `"$InstallDir\agent-comms.exe`""
Write-Host ""
