# tsumugi shell integration (PowerShell)
#
# Marks where the prompt starts and ends (OSC 133) and reports the working
# directory (OSC 7). Without this, tsumugi's [[ ]] [e ]e, the left gutter and
# the `ac` / `io` text objects have no idea where a prompt is, which is most of
# what makes tsumugi different from any other terminal.
#
# Install:  tsg --install-shell-integration pwsh
# Or by hand, in $PROFILE:
#     tsg --shell-integration pwsh | Out-String | Invoke-Expression
#
# NOTE: this file is deliberately ASCII only. Windows PowerShell 5.1 reads a
# BOM-less .ps1 as the ANSI code page, so non-ASCII comments can turn into
# bytes that break parsing. Every other script here is Japanese; this one is not.
#
# cmd.exe has no equivalent hook. Use PowerShell, or accept that the
# prompt-aware features stay dark.

if ($env:TSG_SHELL_INTEGRATION) { return }
$env:TSG_SHELL_INTEGRATION = "1"

$Global:__TsgEsc = [char]27
$Global:__TsgBel = [char]7
$Global:__TsgStarted = $false

# Wrap whatever prompt is already there instead of replacing it.
if (-not (Test-Path Function:\__TsgOrigPrompt)) {
    Copy-Item Function:\prompt Function:\__TsgOrigPrompt
}

function Global:prompt {
    # Exit status of the previous command. $? is false for a failed native
    # command too, and $LASTEXITCODE carries the number when there is one.
    $ok = $?
    $code = if ($ok) { 0 } elseif ($LASTEXITCODE) { $LASTEXITCODE } else { 1 }

    $out = ""
    if ($Global:__TsgStarted) {
        $out += "$__TsgEsc]133;D;$code$__TsgBel"
    }
    $Global:__TsgStarted = $true

    # Working directory (OSC 7). C:\x becomes file:///C:/x
    $p = (Get-Location).Path -replace '\\', '/'
    $p = $p -replace ' ', '%20'
    $out += "$__TsgEsc]7;file:///$p$__TsgBel"

    $out += "$__TsgEsc]133;A$__TsgBel"
    $out += (__TsgOrigPrompt)
    $out += "$__TsgEsc]133;B$__TsgBel"
    return $out
}

# Start of the command output (C). PSReadLine owns Enter, so hook it there.
# Without PSReadLine only A/B/D are emitted: the prompt position and the exit
# code still work, so the gutter, [[ ]] and [e ]e keep working.
if (Get-Module -Name PSReadLine) {
    Set-PSReadLineKeyHandler -Key Enter -BriefDescription "tsumugi accept line" -ScriptBlock {
        [Microsoft.PowerShell.PSConsoleReadLine]::AcceptLine()
        [Console]::Write("$([char]27)]133;C$([char]7)")
    }
}

# ---------------------------------------------------------------------------
# `go` - lay out a workspace.
#
# After cd-ing somewhere, type `go`: the current pane becomes the middle one,
# a directory tree opens on the left, an AI agent on the right, and the tab is
# renamed after the directory.
#
# The name collides with the Go toolchain, so anything with arguments is
# forwarded to the real go.exe, and so is a bare `go` outside tsumugi.
# Set TSUMUGI_NO_GO=1 before sourcing this to keep the name for Go.
if (-not $env:TSUMUGI_NO_GO) {
    function Global:go {
        if ($args.Count -eq 0 -and $env:TSUMUGI_SESSION) {
            tsg --workspace (Get-Location).Path
            return
        }
        $real = Get-Command go.exe -CommandType Application -ErrorAction SilentlyContinue |
            Select-Object -First 1
        if ($real) {
            & $real.Source @args
        } else {
            Write-Error "go: command not found"
        }
    }
}
