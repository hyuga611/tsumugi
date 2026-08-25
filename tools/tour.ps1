# 実画面のひと通り。機能の確認と、紹介動画の絵コンテを兼ねる。
#
# 使い方:
#   1. tsumugi を 1 枚だけ開いておく
#   2. powershell -NoProfile -File tools\verify.ps1 -Script tools\tour.ps1
#
# 各場面のあとで画像を残す。ファイル名の番号がそのまま流れの順番。
# **画面が前に出ていないと合成キーは送らない**（verify.ps1 が止める）。

$Beat = 0
function Beat([string]$Name, [string]$Say) {
    $script:Beat++
    $n = "{0:d2}" -f $script:Beat
    Save-Shot "tour_${n}_$Name" | Out-Null
    "[$n] $Name -- $Say"
}

# --- 1. 普通のターミナルとして開く -----------------------------------------
Assert-Focus
Send-Keys "{ESC}" 300
Send-Keys "i" 200
Send-Keys "echo hello{ENTER}" 800
Beat "typing" "打てば動く。まずは普通のターミナル"

# --- 2. 失敗したコマンド（左ガターが赤くなる） -----------------------------
Send-Keys "no-such-command{ENTER}" 900
Beat "gutter" "左のふちに成否が出る（OSC 133）"

# --- 3. 読むモードへ -------------------------------------------------------
Send-Keys "{ESC}" 400
Beat "normal" "Esc で読むモード。下の帯の色が変わる"

# --- 4. 語をダブルクリックで選ぶ -------------------------------------------
$r = New-Object Tsg+RECT
[Tsg]::GetWindowRect($hwnd, [ref]$r) | Out-Null
$midX = [int]((($r.L + $r.R) / 2) * 0.35)
Invoke-Click $midX ($r.T + 120) 2
Beat "word" "ダブルクリックで語・パス・URL をまるごと"

# --- 5. 右クリックメニュー -------------------------------------------------
Invoke-Click $midX ($r.T + 160) 1 'right'
Beat "menu" "右クリックでいまできることの一覧"
Send-Keys "{ESC}" 300

# --- 6. コマンドパレット ---------------------------------------------------
Invoke-Click 88 ($r.B - 14)
Beat "palette" "下の ≡ からすべてのコマンド。打って絞り込める"
Send-Keys "{ESC}" 300

# --- 7. ファイルを開く -----------------------------------------------------
Send-Keys ":" 400
Send-Keys "e README.md" 400
Send-Keys "{ENTER}" 1200
Beat "file" "同じペインがエディタになる（構文強調つき）"

# --- 8. Markdown を読む形に -----------------------------------------------
Send-Keys "{ESC}" 300
Send-Keys " m" 900
Beat "render" "Space m で読む形。下の ◱ でも切り替わる"

# --- 9. 端末へ戻る ---------------------------------------------------------
Send-Keys " m" 600
Send-Keys ":" 300
Send-Keys "q" 200
Send-Keys "{ENTER}" 800
Beat "back" "✕ 端末へ戻る で元のシェルへ"

# --- 10. 分割 --------------------------------------------------------------
Send-Keys "{ESC}" 300
Send-Keys " s" 900
Beat "split" "Space s で分割。開いた場所を引き継ぐ"

# --- 11. ズーム ------------------------------------------------------------
Send-Keys " z" 700
Beat "zoom" "Space z で 1 枚を画面いっぱいに"
Send-Keys " z" 700

# --- 12. タブ --------------------------------------------------------------
Send-Keys " c" 900
Beat "tab" "Space c で新しいタブ"

# --- 13. 使い方 ------------------------------------------------------------
Send-Keys "{F1}" 700
Beat "help" "F1。マウスだけで使えるところから書いてある"
Send-Keys "{ESC}" 400

# --- 14. 配色 --------------------------------------------------------------
Send-Keys ":" 300
Send-Keys "theme 白磁" 300
Send-Keys "{ENTER}" 800
Beat "theme-light" "配色は 3 つ。設定は保存した瞬間に効く"
Send-Keys ":" 300
Send-Keys "theme 夜霧" 300
Send-Keys "{ENTER}" 800
Beat "theme-dark" "既定へ戻す"

'--- done: target/v/tour_*.png ---'
