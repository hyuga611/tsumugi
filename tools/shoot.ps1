# 紹介動画の素材を撮る。
#
#   powershell -NoProfile -File tools\verify.ps1 -Script tools\shoot.ps1
#
# `tsg --run` / `--send` で動かすので、合成キーを人の手元へ漏らさない。
# 1 場面 = 1 枚ではなく、**動きの前後を細かく**撮る。編集で早送りや
# ズームを掛けられるよう、変化のある瞬間を多めに残す。
#
# `-Lang en` で英語の画面を撮る（英語版の動画用）。

param([string]$Lang = "ja")

$Exe = Join-Path $root "..\target\debug\tsg.exe"
$Shots = Join-Path $root "..\target\v\shots-$Lang"
New-Item -ItemType Directory -Force $Shots | Out-Null

$N = 0
function Shot([string]$Name) {
    $script:N++
    $n = "{0:d3}" -f $script:N
    $r = New-Object Tsg+RECT
    [Tsg]::GetWindowRect($hwnd, [ref]$r) | Out-Null
    $w = $r.R - $r.L; $h = $r.B - $r.T
    $bmp = New-Object System.Drawing.Bitmap $w, $h
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $dc = $g.GetHdc()
    [Tsg]::PrintWindow($hwnd, $dc, 2) | Out-Null
    $g.ReleaseHdc($dc)
    $bmp.Save((Join-Path $Shots "${n}_$Name.png"), [System.Drawing.Imaging.ImageFormat]::Png)
    $g.Dispose(); $bmp.Dispose()
}

function Run([string]$Id) { & $Exe --run $Id | Out-Null; Start-Sleep -Milliseconds 600 }
function Send([string]$S) { & $Exe --send $S | Out-Null; Start-Sleep -Milliseconds 900 }
function Beat([string]$Name, [int]$Frames = 3) {
    for ($i = 0; $i -lt $Frames; $i++) { Shot $Name; Start-Sleep -Milliseconds 220 }
}

# --- 0. シェル統合の入った PowerShell を、このペインだけで起こす ----------
#
# OSC 133 が無いと左のふち・コマンドブロック・畳みが手がかりを持てない。
# ユーザの $PROFILE は触らず、このペインの中だけで読み込ませる。
# **人の名前が写る場所で撮らない。** 画面のプロンプトはそのまま公開物に
# なるので、中立な場所へ置いた写しの中で撮る。
$demo = "C:\demo\tsumugi"
$si = (Join-Path $demo "shell-integration\tsumugi.ps1")
Send "powershell -NoLogo -NoExit -Command `". '$si'`"\n"
Start-Sleep -Milliseconds 2500
Send 'Clear-Host\n'
Beat "open" 4

# --- 1. 打てば動く。成功と失敗が左のふちに出る ----------------------------
Send 'Get-ChildItem -Name\n'
Beat "typed" 4
if ($Lang -eq "en") { Send 'git checkout no-such-branch\n' } else { Send 'no-such-command\n' }
Beat "failed" 4

# --- 2. 検索：打つたびに飛び、一致が光る ----------------------------------
& $Exe --search "README" | Out-Null
Start-Sleep -Milliseconds 500
Beat "search" 5

# --- 3. ラベル：画面のパスに a s d f… ------------------------------------
Run "hints"
Beat "hints" 5

# --- 4. 出力を畳む（何を畳んだかが出る） ----------------------------------
Run "fold.all"
Beat "folded" 5
Run "fold.all"
Beat "unfolded" 2

# --- 5. Markdown を読む形に ----------------------------------------------
$doc = if ($Lang -eq "en") { "README.md" } else { "README.ja.md" }
& $Exe --open $doc --render | Out-Null
Start-Sleep -Milliseconds 1200
Beat "render" 5
Run "file.preview"       # 素のまま（構文強調）
Beat "raw" 4
Run "file.close"
Start-Sleep -Milliseconds 600

# --- 6. git diff を色付きで、ファイル単位に畳む ---------------------------
Run "git.diff"
Start-Sleep -Milliseconds 1200
Beat "diff" 5
Run "fold.all"
Beat "difffold" 4
Run "file.close"
Start-Sleep -Milliseconds 600

# --- 7. エージェント：返事待ちが光る --------------------------------------
Run "layout.split"
Start-Sleep -Milliseconds 1000
Beat "split" 3
& $Exe --agent-state working | Out-Null
Beat "working" 3
& $Exe --agent-state blocked --cost '$0.42' | Out-Null
Beat "blocked" 6
Run "agent.next"
Beat "jumped" 4
Run "layout.close"
Start-Sleep -Milliseconds 600

# --- 8. 使い方（F1）：マウスだけで使えます --------------------------------
Run "ui.help"
Beat "help" 5
Run "ui.help"

# --- 9. 配色 --------------------------------------------------------------
Run "ui.theme.hakuji"
Beat "light" 4
Run "ui.theme.sumi"
Beat "sumi" 3
Run "ui.theme.yogiri"
Beat "back" 3

"shots: $Shots"
