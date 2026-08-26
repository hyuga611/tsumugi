# tsumugi installer (Windows).
#
#   irm https://raw.githubusercontent.com/hyuga611/tsumugi/main/install.ps1 | iex
#
# Downloads the latest tsg.exe and runs `tsg --install` (Start Menu, desktop,
# user PATH, folder context menu). No admin rights: it only touches HKCU and
# the user PATH. To remove: `tsg --uninstall`, then delete the folder.
#
# Options are environment variables, set them before the line above:
#   $env:TSUMUGI_DIR = 'D:\tools'    # where to put it (default: %USERPROFILE%\bin)
#   $env:TSUMUGI_VERSION = 'v0.1.0'  # which release (default: latest)
#   $env:TSUMUGI_NO_REGISTER = '1'   # just drop the exe, no shortcuts
#
# This file is ASCII on purpose, and carries no BOM. Both are load-bearing:
#   - `irm | iex` chokes on a BOM (it reads it as a command name)
#   - Windows PowerShell 5.1 reads a BOM-less file as ANSI, so non-ASCII
#     comments would turn into a parse error when the file is run directly
# Japanese notes live in README.ja.md instead.

$ErrorActionPreference = 'Stop'

# Do not default to %LOCALAPPDATA%: on machines with application control
# (AppLocker / WDAC) executables there often refuse to start.
$Dir = if ($env:TSUMUGI_DIR) { $env:TSUMUGI_DIR } else { "$env:USERPROFILE\bin" }
$Version = if ($env:TSUMUGI_VERSION) { $env:TSUMUGI_VERSION } else { 'latest' }
$NoRegister = [bool]$env:TSUMUGI_NO_REGISTER

if ($env:OS -ne 'Windows_NT') {
    throw 'Windows only for now (build from source on macOS / Linux).'
}

$repo = 'hyuga611/tsumugi'
$api = if ($Version -eq 'latest') {
    "https://api.github.com/repos/$repo/releases/latest"
} else {
    "https://api.github.com/repos/$repo/releases/tags/$Version"
}

Write-Host "Fetching tsumugi ($Version)..."
$release = Invoke-RestMethod -Uri $api -Headers @{ 'User-Agent' = 'tsumugi-install' }
$asset = $release.assets | Where-Object { $_.name -eq 'tsg.exe' } | Select-Object -First 1
if (-not $asset) {
    throw "That release has no tsg.exe: $($release.tag_name)"
}

New-Item -ItemType Directory -Force -Path $Dir | Out-Null
$exe = Join-Path $Dir 'tsg.exe'

# A running tsumugi holds the file open; say so instead of failing obscurely.
if (Test-Path $exe) {
    $running = Get-Process -Name tsg -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq $exe }
    if ($running) {
        throw 'tsumugi is running. Close it first (taskkill /IM tsg.exe /F).'
    }
}

Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $exe -UseBasicParsing
Write-Host "Installed: $exe ($($release.tag_name))"

# Check it actually starts. Application control and antivirus show up here.
#
# Look at the exit code, not the output: tsumugi is a GUI app (windows
# subsystem), so `& $exe --version` hands PowerShell nothing to capture, and
# reading that emptiness as failure would flag a working install as broken.
$ok = $false
try {
    $p = Start-Process -FilePath $exe -ArgumentList '--version' -PassThru -Wait -WindowStyle Hidden
    $ok = ($p.ExitCode -eq 0)
} catch {
    $ok = $false
}
if (-not $ok) {
    throw @"
Could not start: $exe
Application control (AppLocker / WDAC) or antivirus may be blocking it.
To install somewhere else:
  `$env:TSUMUGI_DIR = 'D:\tools'
  irm https://raw.githubusercontent.com/hyuga611/tsumugi/main/install.ps1 | iex
"@
}
Write-Host "Verified it starts ($($release.tag_name))"

if (-not $NoRegister) {
    & $exe --install
}

Write-Host ''
Write-Host 'Done. Open a new shell and type `tsg`, or use the Start Menu.'
Write-Host 'Shell integration (prompt marks):  tsg --install-shell-integration'
Write-Host 'AI agent status hooks:             tsg --install-agent-hooks'
