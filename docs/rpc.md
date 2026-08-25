# rpc.md — 外から tsumugi を動かす

tsumugi の mux は最初から**別プロセス**で、クライアント（ウィンドウ）とは
JSON Lines で話している。つまり「外から動かす口」は後付けの機能ではなく、
最初から製品の中を通っている道そのものだ。ここではその形を約束として書く。

窓が持っているものは全部この口の向こうにある。窓を開かずにセッションを作り、
コマンドを流し、画面を読み、ペインを割ることができる。

---

## 1. 安全のこと（先に読む）

**この口に繋げる者は、そのユーザとして何でもできる。** シェルへ任意のキーを
送れる（＝任意のコマンドを実行できる）し、画面に出た全部（鍵・パスワード・
トークン）を読める。

だから口は**そのユーザだけに閉じてある**。

| | 置き場所 | 誰が触れるか |
|---|---|---|
| Unix | `$XDG_RUNTIME_DIR/tsumugi/<名前>.sock`（無ければ `/tmp/tsumugi-<uid>/`） | 0700 のディレクトリ配下、ソケットは 0600 |
| Windows | `\\.\pipe\tsumugi-<印>-<名前>.sock` | 現在のユーザ SID だけを許す DACL |

Windows では加えて、繋いだ先のプロセスが自分のものかを**クライアント側でも
確かめる**。`\\.\pipe\` は誰でも名前を作れる名前空間なので、先回りして同じ
名前のパイプを立てておく手（squatting）があるため。

**この権限がこの口の唯一の錠**で、それ以上の認証は置いていない。
ここに繋げる時点で、その相手はすでに同じユーザとしてプロセスを起こせる。

守れない環境では**開かない**。ソケットを自分だけに閉じられなければ、
サーバは起動に失敗する。

---

## 2. すぐ使う口

たいていのことは 4 つで足りる。

```sh
tsg --list                      # 走っているセッションを並べる
tsg -s work --send 'ls -la\n'   # いまのペインへ入力を流す（\n で改行、\e で Esc）
tsg -s work --capture           # いまのペインに見えているものをテキストで
tsg -s work --capture 3         # ペイン 3 を指定して
tsg -s work --tap               # 出てくる生バイトを覗く（Ctrl-C で終了）
```

`--capture` が返すのは、サーバが再アタッチ用に持っている画面そのもの。
**窓に見えているものと同じであることが構造的に保証される**（取り出し専用の
別経路を作っていないので、片方だけ古くなることが起きない）。

### 例: ビルドを流して結果だけ取る

```sh
tsg -s work --send 'cargo test 2>&1 | tail -20\n'
sleep 30
tsg -s work --capture | tail -20
```

---

## 3. 生の口 — `tsg --rpc`

便利な口をいくつ足しても「それでは足りない」は必ず来る。そのときに
こちらの実装を待たずに済むよう、生の口を開けてある。

```sh
tsg -s work --rpc
```

- **標準入力の 1 行 = `ClientMsg` 1 通**
- **標準出力の 1 行 = `ServerMsg` 1 通**
- `Attach` は済ませてから渡す。最初の 1 行に `attached` が出るので、
  台本はそこからペインの id を読める
- 読めない行はその行だけ捨てて `{"t":"error",...}` を返す。落とさないのは、
  手で打って形を覚える使い方を潰さないため
- 標準入力が閉じたら `Detach` して終わる（**プロセスは殺さない**）

```console
$ printf '' | tsg -s work --rpc
{"t":"attached","version":8,"session":{"name":"work","tabs":[{"id":1,"layout":{"k":"leaf","pane":1},"active_pane":1,"zoom":null}],"active_tab":1,"panes":[{"id":1,"title":"cmd.exe","cols":80,"rows":24,"alive":true}]}}
```

### 例: ペインを割って、片方でコマンドを走らせる

```sh
{
  echo '{"t":"split","pane":1,"dir":"horizontal"}'
  sleep 1
  printf '{"t":"input","pane":2,"data":"%s"}\n' "$(printf 'top\r' | base64 -w0)"
  sleep 5
} | tsg -s work --rpc
```

---

## 4. プロトコル

JSON Lines（1 行 1 メッセージ、UTF-8）。判別は `"t"` フィールド。
いまの版は **12**。

> **生バイトを送る。** グリッドの差分ではなく PTY の生バイトを流し、
> クライアントは自前の `tsg-term` で解析して自分のグリッドを作る。
> これで差分の直列化を実装せずに済み、プロトコルが端末仕様に引きずられない。
> 再アタッチのときだけ、サーバが持つ画面をスナップショットとして送る。

### 4.1 クライアント → サーバ（`ClientMsg`）

| `t` | 引数 | 何をするか |
|---|---|---|
| `attach` | `version`, `cols`, `rows`, `cwd?`, `command?` | 最初の 1 通。版が違えば `error` が返る |
| `input` | `pane`, `data`(base64) | キー入力を流す |
| `resize` | `pane`, `cols`, `rows` | ペインの大きさ |
| `split` | `pane`, `dir`(`horizontal`/`vertical`) | 割る |
| `close_pane` | `pane` | 閉じる |
| `resize_split` | `pane`, `delta` | 分割比を動かす |
| `swap_panes` | `a`, `b` | 入れ替える |
| `equalize` | `tab` | 分割比をそろえる |
| `set_zoom` | `tab`, `pane?` | 1 枚を画面いっぱいに（`null` で戻す） |
| `move_tab` | `tab`, `to` | タブの並べ替え |
| `open_file` | `pane`, `path` | そのペインでファイルを開く |
| `set_file` | `pane`, `text` | 全文を渡し直す |
| `edit_file` | `pane`, `base_len`, `edits` | 差分を当てる |
| `save_file` | `pane`, `path?` | 保存する |
| `close_file` | `pane` | 端末へ戻す |
| `pipe_result` | `pane`, `dir`, `title`, `text` | 結果を新しいペインで開く |
| `new_tab` | — | タブを足す |
| `select_tab` | `tab` | タブを選ぶ |
| `detach` | — | **切るがプロセスは生かす**（永続性の本体） |
| `shutdown` | — | サーバごと落とす |
| `ping` | — | 生きているか |

**`cols` / `rows` に 0 を送ると「大きさを持ち込まない」。** 窓を持たない
台本が適当な 80x24 を名乗ると、その後に開くペインまでその大きさになる。
`--send` / `--capture` / `--tap` / `--rpc` はすべて 0 を送っている。

### 4.2 サーバ → クライアント（`ServerMsg`）

| `t` | 引数 | いつ来るか |
|---|---|---|
| `attached` | `version`, `session` | `attach` の返事 |
| `snapshot` | `pane`, `lines`, `cursor_line`, `cursor_col`, `alt`, `alt_cursor` | アタッチ時。各行は **SGR 付きの ANSI**。`lines` は primary だけ、`alt` は全画面アプリの最中ならその画面 |
| `output` | `pane`, `data`(base64) | PTY が何か出したとき |
| `resized` | `pane`, `cols`, `rows` | ペインの実サイズが変わったとき |
| `layout` | `SessionInfo` | 木が変わったとき |
| `file_state` | `pane`, `path?`, `title`, `text`, `dirty` | ファイルを開いた / 再アタッチ |
| `file_saved` | `pane`, `path` | 保存できた |
| `file_closed` | `pane` | 閉じた |
| `need_full_file` | `pane` | 差分を当てられなかった。全文を送り直す |
| `pane_exited` | `pane` | 子プロセスが終わった |
| `pong` | — | `ping` の返事 |
| `error` | `message` | 断った理由 |

`resized` を受けるまで**自分の鏡を勝手に広げてはいけない**。ConPTY が実際に
サイズを変えるのはサーバが `pty.resize()` を呼んだ瞬間で、それ以前のバイトは
古い桁数で組まれている。先に広げると、古い桁で折り返された行を新しい桁で
読み直して表示が崩れる。この通知はバイト列と同じ順序で届く。

### 4.3 型

```jsonc
// SessionInfo
{ "name": "work", "tabs": [TabInfo], "active_tab": 1, "panes": [PaneInfo] }
// TabInfo
{ "id": 1, "layout": Layout, "active_pane": 1, "zoom": null }
// PaneInfo
{ "id": 1, "title": "cmd.exe", "cols": 80, "rows": 24, "alive": true }
// Layout（木）
{ "k": "leaf", "pane": 1 }
{ "k": "split", "dir": "horizontal", "children": [Layout], "weights": [100, 100] }
// Edit（edit_file の中身。位置はバイト）
{ "start": 12, "remove": 3, "insert": "abc" }
```

---

## 5. 版

`attach` の `version` がサーバと違えば `error` が返り、そこで終わる。
**当てずっぽうに続けない。** 形が変わったのに古い解釈で読み続けるほうが、
繋がらないより危ない。

| 版 | 変えたこと |
|---|---|
| 12 | `snapshot` に `alt` / `alt_cursor` を足した（alt screen を履歴と混ぜない） |
| 8 | `attach` の `cols` / `rows` に 0（＝大きさを持ち込まない）を足した |
| 7 | ファイル編集の差分化（`edit_file`） |
| 2 | 再アタッチの復元を生バイトの再送から `snapshot` へ |
