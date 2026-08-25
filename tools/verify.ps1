# One-shot GUI verification driver.
#
# Everything happens in a single PowerShell process: Add-Type compiles C# and
# steals the foreground window, so doing it once up front and never again is
# what keeps the synthetic keys inside the target window.
#
# Before every key burst the foreground window is checked against the target
# hwnd. If it does not match we click the window and check again; if it still
# does not match we abort instead of typing into whatever is in front.
param(
    [Parameter(Mandatory = $true)][string]$Script,
    [string]$OutDir = (Join-Path $PSScriptRoot "..\target\v"),
    [int]$W = 1240,
    [int]$H = 800
)

$env:TMP = (Join-Path $PSScriptRoot "..\target\pstmp")
$env:TEMP = $env:TMP
New-Item -ItemType Directory -Force $env:TMP | Out-Null

Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public class Tsg {
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr h, int x, int y, int w, int t, bool r);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int c);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, IntPtr e);
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint f, IntPtr e);
    public const uint DOWN = 0x0002, UP = 0x0004, RDOWN = 0x0008, RUP = 0x0010, WHEEL = 0x0800;
    public const uint KEYUP = 0x0002;
    public const byte VK_CONTROL = 0x11;
}
'@
[Tsg]::SetProcessDPIAware() | Out-Null

$proc = Get-Process tsg -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if ($null -eq $proc) { throw "tsg is not running" }
$hwnd = $proc.MainWindowHandle
[Tsg]::ShowWindow($hwnd, 9) | Out-Null
[Tsg]::MoveWindow($hwnd, 0, 0, $W, $H, $true) | Out-Null
[Tsg]::SetForegroundWindow($hwnd) | Out-Null
Start-Sleep -Milliseconds 600

function Invoke-Click([int]$X, [int]$Y, [int]$Count = 1, [string]$Button = 'left') {
    [Tsg]::SetCursorPos($X + 6, $Y + 6) | Out-Null
    Start-Sleep -Milliseconds 60
    [Tsg]::SetCursorPos($X, $Y) | Out-Null
    Start-Sleep -Milliseconds 120
    $d = if ($Button -eq 'right') { [Tsg]::RDOWN } else { [Tsg]::DOWN }
    $u = if ($Button -eq 'right') { [Tsg]::RUP } else { [Tsg]::UP }
    for ($i = 0; $i -lt $Count; $i++) {
        [Tsg]::mouse_event($d, 0, 0, 0, [IntPtr]::Zero)
        Start-Sleep -Milliseconds 25
        [Tsg]::mouse_event($u, 0, 0, 0, [IntPtr]::Zero)
        Start-Sleep -Milliseconds 50
    }
    Start-Sleep -Milliseconds 200
}

function Invoke-Drag([int]$X1, [int]$Y1, [int]$X2, [int]$Y2, [string]$Button = 'left') {
    $d = if ($Button -eq 'right') { [Tsg]::RDOWN } else { [Tsg]::DOWN }
    $u = if ($Button -eq 'right') { [Tsg]::RUP } else { [Tsg]::UP }
    [Tsg]::SetCursorPos($X1, $Y1) | Out-Null
    Start-Sleep -Milliseconds 200
    [Tsg]::mouse_event($d, 0, 0, 0, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 120
    for ($i = 1; $i -le 6; $i++) {
        $x = $X1 + [int](($X2 - $X1) * $i / 6)
        $y = $Y1 + [int](($Y2 - $Y1) * $i / 6)
        [Tsg]::SetCursorPos($x, $y) | Out-Null
        Start-Sleep -Milliseconds 60
    }
    [Tsg]::mouse_event($u, 0, 0, 0, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 300
}

# Just move the pointer (hover). Two steps so the app sees a real move.
function Invoke-Move([int]$X, [int]$Y) {
    [Tsg]::SetCursorPos($X - 10, $Y - 6) | Out-Null
    Start-Sleep -Milliseconds 100
    [Tsg]::SetCursorPos($X, $Y) | Out-Null
    Start-Sleep -Milliseconds 400
}

function Invoke-CtrlClick([int]$X, [int]$Y) {
    [Tsg]::SetCursorPos($X - 8, $Y - 4) | Out-Null
    Start-Sleep -Milliseconds 80
    [Tsg]::SetCursorPos($X, $Y) | Out-Null
    Start-Sleep -Milliseconds 150
    [Tsg]::keybd_event([Tsg]::VK_CONTROL, 0, 0, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 150
    [Tsg]::mouse_event([Tsg]::DOWN, 0, 0, 0, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 40
    [Tsg]::mouse_event([Tsg]::UP, 0, 0, 0, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 150
    [Tsg]::keybd_event([Tsg]::VK_CONTROL, 0, [Tsg]::KEYUP, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 400
}

function Invoke-CtrlWheel([int]$X, [int]$Y, [int]$Notches) {
    [Tsg]::SetCursorPos($X, $Y) | Out-Null
    Start-Sleep -Milliseconds 150
    [Tsg]::keybd_event([Tsg]::VK_CONTROL, 0, 0, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 150
    $step = if ($Notches -lt 0) { [uint32]4294967176 } else { [uint32]120 }
    for ($i = 0; $i -lt [Math]::Abs($Notches); $i++) {
        [Tsg]::mouse_event([Tsg]::WHEEL, 0, 0, $step, [IntPtr]::Zero)
        Start-Sleep -Milliseconds 150
    }
    [Tsg]::keybd_event([Tsg]::VK_CONTROL, 0, [Tsg]::KEYUP, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 500
}

function Assert-Focus {
    if ([Tsg]::GetForegroundWindow() -eq $hwnd) { return }
    [Tsg]::SetForegroundWindow($hwnd) | Out-Null
    Start-Sleep -Milliseconds 300
    if ([Tsg]::GetForegroundWindow() -eq $hwnd) { return }
    # Windows refuses SetForegroundWindow from a background process. Minimize
    # and restore forces the activation the honest way.
    [Tsg]::ShowWindow($hwnd, 6) | Out-Null   # SW_MINIMIZE
    Start-Sleep -Milliseconds 250
    [Tsg]::ShowWindow($hwnd, 9) | Out-Null   # SW_RESTORE
    Start-Sleep -Milliseconds 450
    if ([Tsg]::GetForegroundWindow() -eq $hwnd) { return }
    # Click inside the window itself. A fixed coordinate misses whenever the
    # window is smaller than the default size, and then this throws for the
    # wrong reason.
    $r = New-Object Tsg+RECT
    [Tsg]::GetWindowRect($hwnd, [ref]$r) | Out-Null
    Invoke-Click ([int](($r.L + $r.R) / 2)) ([int]($r.T + 12))
    if ([Tsg]::GetForegroundWindow() -ne $hwnd) {
        throw "lost focus: refusing to type into another window"
    }
}

function Send-Keys([string]$Keys, [int]$Pause = 250) {
    Assert-Focus
    [System.Windows.Forms.SendKeys]::SendWait($Keys)
    Start-Sleep -Milliseconds $Pause
}

# PrintWindow(PW_RENDERFULLCONTENT): capture the window itself.
# CopyFromScreen goes through GDI and returns pure white for windows that
# present via DirectComposition -- that once looked like a rendering bug.
function Save-Shot([string]$Name) {
    $r = New-Object Tsg+RECT
    [Tsg]::GetWindowRect($hwnd, [ref]$r) | Out-Null
    $w = $r.R - $r.L
    $h = $r.B - $r.T
    $bmp = New-Object System.Drawing.Bitmap $w, $h
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $dc = $g.GetHdc()
    [Tsg]::PrintWindow($hwnd, $dc, 2) | Out-Null
    $g.ReleaseHdc($dc)
    $path = Join-Path $OutDir "$Name.png"
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $g.Dispose(); $bmp.Dispose()
    "shot $path ($w x $h)"
}

. $Script
