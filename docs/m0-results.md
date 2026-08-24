# M0 スパイク結果

`arch.md` §9 の判定ゲート。ここで崩れた前提は設計のやり直しを要求する。

---

## M0-a（PTY / 解析層） — 🟢 通過

実測日: 2026-08-23 / Windows 11 Pro 26200 / x86_64 / Rust 1.97.1
再現: `cargo run -p tsg-probe`

| 検査 | PowerShell 5.1 | Git Bash 5.x | 結論 |
|---|:---:|:---:|---|
| **OSC 133 が PTY を通る** | PASS (9/9) | PASS (9/9) | **ConPTY は OSC 133 を握り潰さない。** 最大リスクが消えた |
| コマンドブロックに畳める | PASS | PASS | `ic` `ac` `io` `ao` の前提が成立 |
| 終了コードを取れる | PASS (127) | PASS (127) | `]e`（次のエラーへ）の前提が成立 |
| 実プロンプト統合から終了コード | PASS (3) | PASS (3) | シェル統合を配ればそのまま動く |
| UTF-8 / CJK / 異体字 / 絵文字 | PASS | PASS | `日本語` `※` `葛+IVS(U+E0100)` `🐕` が往復して壊れない |
| alt screen が履歴を汚さない | PASS | PASS | TUI の描画がスクロールバックに混ざらない |
| alt + マウスレポートで所有権が子へ | PASS | PASS | `concept.md` の所有権モデルが実データで裁定できる |

**結論: `concept.md` / `modal-spec.md` / `mouse-parity.md` の前提を変更する必要はない。**
Windows を劣化モードにする回避策（ヒューリスティックなプロンプト検出）は不要。

### 副産物として潰したもの

1. **alt screen 上の OSC 133 を記録してしまう実バグ**（プローブが検出）
   alt screen では履歴が伸びず絶対行番号が意味を持たないため、記録すると `]e` が
   実在しない行へ飛ぶ。`TermState::mark()` で alt 中のマークを捨てるよう修正し、
   回帰テスト `marks_emitted_on_alt_screen_are_discarded` を追加。

2. **`bash` が WSL のランチャに解決される事故**
   Windows の PATH では `System32\bash.exe`（WSL ランチャ）が先に当たることがあり、
   PTY 上で `execvpe(/bin/bash) failed` を出して即死する。
   これを放置すると「ConPTY が OSC を落としている」という**誤った結論**になる。
   `resolve_program()` で実行ファイルを明示解決し、WSL シムを除外するようにした。
   → 本体でもシェル起動は名前解決を OS 任せにしないこと。

### 環境上の罠

このマシンでは共有 target ディレクトリ（`CARGO_TARGET_DIR` が `%LOCALAPPDATA%` 配下）に
出力した実行ファイルが Windows のアプリケーション制御ポリシーにブロックされる
（`os error 4551`）。`.cargo/config.toml` でリポジトリ内の `target/` を使うようにしてあるが、
**環境変数 `CARGO_TARGET_DIR` はこの設定より優先される**ため、シェルに設定がある場合は
シェル側で明示する必要がある。
PowerShell は `$env:CARGO_TARGET_DIR = "./target"` を先に実行する
（bash の `VAR=x cmd` 形式は PowerShell では動かない）。

---

## M0-b（描画 / 入力 / IME） — 🟢 通過

実測日: 2026-08-23 / 同環境 / wgpu 30.0.1 + winit 0.30.13 + swash 0.2 + fontdb 0.24
再現: `cargo run -p tsg -- --diagnose`（数値のみ）/ `cargo run -p tsg`（GUI）

| 検査 | 結果 | 備考 |
|---|:---:|---|
| wgpu でセル描画が立ち上がる | PASS | DX12。ウィンドウ起動〜描画〜シェル起動まで例外なし |
| フォントチェーンが組める | PASS | `Consolas -> MS Gothic(x1.111) -> Segoe UI Emoji(x0.809)` |
| **CJK が 2 セル幅に収まる** | PASS | `日` `あ` `※` すべて 2.00 セル（下記の修正後） |
| PTY を繋いで実シェルが動く | PASS | ウィンドウ内でコマンドが打てる |
| **IME の preedit が出る** | PASS | Windows TSF。イベント 89 件受信。変換中文字列がカーソル位置にインライン描画される |
| **IME 候補ウィンドウの位置** | PASS | `set_ime_cursor_area()` により preedit の直下に出る |
| **モード切替で IME が自動 ON/OFF** | PASS | `[mode] Normal -> IME 無効` / `[mode] Insert -> IME 有効` の往復を実機で確認 |

### 修正した不整合: 欧文フォントと CJK フォントの寸法が合わない

M0-b の最初の実測は **🟡 全角が 1.80 セル**だった。

- Consolas の半角送り幅 = 9.9px（0.55em）→ セル幅は 10px
- MS Gothic の全角送り幅 = 18.0px（1.0em）→ 20px 必要なのに 18px しかない

欧文等幅フォントと CJK フォントは em に対する送り幅の設計が違うため、
**同じ px で描くと全角がセル格子に収まらない**。日本語環境で既存ターミナルの
表がズレる主因のひとつがこれである。

採った解: **セル格子を正とし、フォールバックフォント側を格子へ伸縮させる。**
フォント発見時に代表グリフ（`日` `一` `あ` `漢` `🐕` `☺`）を1つ測り、
`2 * cell_w` になる倍率を各フォントに持たせて、その倍率でラスタライズする。
結果 `MS Gothic ×1.111` / `Segoe UI Emoji ×0.809` が自動で決まり、全角は 2.00 セルになった。

回帰テスト `fallback_fonts_fit_the_cell_grid`（`tsg-render`）で固定した。

### IME の実測（2026-08-23・Windows 11 / TSF）

```
[mode] Insert -> IME 有効
[IME] 変換中 "はやさか" cursor=Some((12, 12))
[IME] 変換中 "早坂"     cursor=Some((0, 6))
[IME] 確定 "早坂"
[mode] Normal -> IME 無効
[mode] Insert -> IME 有効
...
IME イベント総数: 89
変換中文字列(preedit)の受信: あり（PASS）
```

未確定文字列がカーソル位置に描かれ、IME の候補ウィンドウがその直下に出ることを目視で確認した。
`Esc` で通常モードへ入ると日本語入力が効かなくなり、`i` で戻ると再び効く。

**これで `arch.md` §6.2 の「モードを本体が知っているから IME を正しく切れる」という主張が
実機で立証された。** vim の `im_control` 系プラグインが解いてきた問題を、本体側で構造的に解ける。

macOS / Linux（ibus・fcitx）での確認は、その環境に触れるときに同じ手順で行う。
winit に寄せている以上、実装の追加は不要な見込みだが、未実測であることは記録しておく。

### 参考: 幅の目盛りを見るには

起動直後に流し込んでいる幅の目盛りは、シェルの出力ですぐ画面外へ出る。
`Esc` で通常モードに入り `g` を押すとドキュメント先頭に戻って確認できる。
なお送り幅は数値で 2.00 セルと確定しており、描画はセル番号で位置決めしているため、
目盛りは念のための二重確認である。

---

## M0 総括 — 🟢 通過。設計の変更は不要

3つの危険な仮定はすべて実機で成立した。

| 仮定 | 結果 |
|---|---|
| ConPTY を OSC 133 が通る | 🟢 通る。ターミナル固有の語彙は Windows でも成立 |
| 3 OS で IME の preedit が出る | 🟢 Windows で立証。macOS / Linux は未実測 |
| CJK 幅が崩れない | 🟢 解析層・描画層とも 2 セルで整合（フォント伸縮の修正後） |

`concept.md` / `modal-spec.md` / `mouse-parity.md` / `arch.md` は**変更なしで M1 へ進める**。

スパイクが潰した実バグ3件（alt screen 上のセマンティックマーク、欧文と CJK の寸法不一致、
WSL シムへの誤解決）は、いずれも紙の上では見つからない種類のものだった。
