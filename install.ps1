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
#   $env:TSUMUGI_FORCE = '1'         # reinstall even if already on that version
#
# Already installed? `tsg update` runs this same script for you.
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

# `tsg update` sets TSUMUGI_HAVE to the version it is running. If that is
# already the release we found, downloading 19 MB again buys nothing.
# A first install never sets it, so the plain `irm | iex` line is unaffected.
if ($env:TSUMUGI_HAVE -and -not $env:TSUMUGI_FORCE -and
    $release.tag_name -eq ('v' + $env:TSUMUGI_HAVE)) {
    Write-Host "Already on $($release.tag_name)."
    return
}

$asset = $release.assets | Where-Object { $_.name -eq 'tsg.exe' } | Select-Object -First 1
if (-not $asset) {
    throw "That release has no tsg.exe: $($release.tag_name)"
}

New-Item -ItemType Directory -Force -Path $Dir | Out-Null
$exe = Join-Path $Dir 'tsg.exe'

# Leftovers from an earlier install, once nothing holds them open any more.
Get-ChildItem -LiteralPath $Dir -Filter 'tsg.exe.old-*' -ErrorAction SilentlyContinue |
    ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue }

# A running tsumugi holds the file open, so it cannot be overwritten. It CAN be
# renamed though: Windows follows the file, so the window you have open keeps
# working. Telling people to kill their sessions first is a dead end - closing
# tsumugi means closing the shells inside it.
$moved = $null
if (Test-Path $exe) {
    $running = Get-Process -Name tsg -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq $exe }
    if ($running) {
        $moved = Join-Path $Dir ('tsg.exe.old-' + $PID)
        Move-Item -LiteralPath $exe -Destination $moved -Force
        Write-Host 'tsumugi is running: moved the old one aside.'
    }
}

try {
    Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $exe -UseBasicParsing
} catch {
    # Do not leave someone without a tsumugi because the download failed.
    if ($moved) { Move-Item -LiteralPath $moved -Destination $exe -Force }
    throw
}
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
    if ($moved) {
        Remove-Item -LiteralPath $exe -Force -ErrorAction SilentlyContinue
        Move-Item -LiteralPath $moved -Destination $exe -Force
        Write-Host 'Put the old one back.'
    }
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
if ($moved) {
    Write-Host 'The tsumugi you had open is still the old one. Close and reopen it,'
    Write-Host 'and stop the old multiplexer first:  tsg --list  then  tsg -s <name> --kill'
}
Write-Host 'Done. Open a new shell and type `tsg`, or use the Start Menu.'
Write-Host 'Shell integration and agent hooks went in too; `tsg --uninstall` takes it all back out.'
