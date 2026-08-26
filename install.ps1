# tsumugi を入れる。手作業はここで終わり。
#
#   irm https://raw.githubusercontent.com/hyuga611/tsumugi/main/install.ps1 | iex
#
# やること: 最新版の tsg.exe を取ってきて置き、`tsg --install` を走らせる
# （スタートメニュー・デスクトップ・PATH・フォルダの右クリックに登録）。
#
# 管理者権限は要らない（触るのは HKCU とユーザー PATH だけ）。
# 消すときは `tsg --uninstall` のあとフォルダごと削除。

param(
    # 置き場所。**%LOCALAPPDATA% を既定にしない** — アプリ制御の効いた PC では
    # そこから起動できないことがある（実際に踏んだ）。
    [string]$Dir = "$env:USERPROFILE\bin",
    # 特定の版を入れる（既定は最新）。例: -Version v0.1.0
    [string]$Version = "latest",
    # 登録（スタートメニュー等）はしない。exe を置くだけ。
    [switch]$NoRegister
)

$ErrorActionPreference = 'Stop'

if ($env:OS -ne 'Windows_NT') {
    throw 'いまのところ Windows 専用です（macOS / Linux は自分でビルドしてください）'
}

$repo = 'hyuga611/tsumugi'
$api = if ($Version -eq 'latest') {
    "https://api.github.com/repos/$repo/releases/latest"
} else {
    "https://api.github.com/repos/$repo/releases/tags/$Version"
}

Write-Host "tsumugi を取ってきます ($Version)..."
$release = Invoke-RestMethod -Uri $api -Headers @{ 'User-Agent' = 'tsumugi-install' }
$asset = $release.assets | Where-Object { $_.name -eq 'tsg.exe' } | Select-Object -First 1
if (-not $asset) {
    throw "この版に tsg.exe がありません: $($release.tag_name)"
}

New-Item -ItemType Directory -Force -Path $Dir | Out-Null
$exe = Join-Path $Dir 'tsg.exe'

# 走っている tsumugi が掴んでいると置き換えられない。先に知らせる。
if (Test-Path $exe) {
    $running = Get-Process -Name tsg -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq $exe }
    if ($running) {
        throw "tsumugi が動いています。閉じてから実行してください（taskkill /IM tsg.exe /F）"
    }
}

Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $exe -UseBasicParsing
Write-Host "置きました: $exe ($($release.tag_name))"

# 置いただけで動くかを確かめる。アプリ制御や AV で弾かれるのはここで分かる。
try {
    $ver = & $exe --version
} catch {
    throw @"
起動できませんでした: $exe
アプリ制御（AppLocker / WDAC）やウイルス対策に弾かれている可能性があります。
別の場所を試すなら: irm .../install.ps1 | iex; install -Dir 'D:\tools'
"@
}
Write-Host "動きました: $ver"

if (-not $NoRegister) {
    & $exe --install
}

Write-Host ''
Write-Host '入りました。新しいシェルで `tsg` と打つか、スタートメニューから開いてください。'
Write-Host 'シェル統合（プロンプトの位置を伝える）も入れるなら: tsg --install-shell-integration'
Write-Host 'AI エージェントの状態通知を配線するなら:            tsg --install-agent-hooks'
