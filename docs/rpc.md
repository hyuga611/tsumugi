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
| `ext_hello` | `name` | 名乗る（記録を読める形にするためだけ） |
| `ext_log` | `limit?` | 拡張が何をしたかの記録（§5.4） |
| `subscribe` | `events` | 出来事を購読する（§5.1） |
| `unsubscribe` | `events` | やめる |
| `register_command` | `command`(ExtCommand) | 語彙を足す（§5.2） |
| `unregister_command` | `id` | 外す |
| `get_buffer` | `pane`, `start?`, `end?` | 中身を取り出す。範囲は**ドキュメント絶対行** |
| `ext_pane_open` | `id`, `near?`, `dir?`, `title`, `text` | 拡張が自分のペインを開く（§5.3） |
| `ext_pane_write` | `id`, `text` | その中身を差し替える |
| `ext_pane_close` | `id` | 閉じる |
| `notify` | `text`, `level?` | 走っている窓へ知らせる（§5.6） |
| `wait` | `pane?`, `matcher`, `timeout_ms?` | 条件が満たされるまで待つ（§6） |
| `layout_export` | `tab?` | いまの並べ方を形だけ書き出す（§7） |
| `layout_apply` | `spec` | その形で**新しいタブ**を開く |
| `worktree_list` | `pane?` | git の作業ツリーを並べる（§8） |
| `worktree_add` | `pane?`, `path`, `branch?` | 足して、そこで開く |
| `worktree_open` | `path` | そこで新しいタブを開く |
| `worktree_remove` | `pane?`, `path`, `force?` | 消す（既定では押し切らない） |
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
| `event` | `event`(PluginEvent) | 購読した出来事（§5.1） |
| `ext_commands` | `commands` | 外から足された語彙の**全体**（総取り替え） |
| `buffer` | `pane`, `kind`, `start`, `lines` | `get_buffer` の返事 |
| `ext_pane` | `id`, `pane` | `ext_pane_open` の返事（ペインの番号） |
| `ext_log` | `entries` | `ext_log` の返事。**古いものが先** |
| `notify` | `text`, `level` | 画面へ出す知らせ |
| `waited` | `matched`, `pane?` | `wait` の返事。**1 通だけ** |
| `layout_spec` | `spec` | `layout_export` の返事 |
| `worktrees` | `items` | `worktree_list` の返事 |
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

## 5. 拡張を書く

ここまでが「外から動かす口」。ここから先は **外から足す口**。

`concept.md` の「捨てるもの 5」で、v1 に組み込みスクリプト言語を入れない
代わりに約束したのがこれで、拡張は**別のプロセス**として走る。言語は何でも
よく、落ちても本体は落ちない。

### 5.1 出来事を受け取る

```sh
tsg --subscribe                    # 全部
tsg --subscribe command_end,agent  # 選んで
```

出るのは JSON Lines。`--tap` の生バイトと違い、**シェル統合（OSC 133）が
言ってきたこと**をそのまま意味の粒で配る。画面から当てないので、
出力の形が変わった日に黙って壊れることがない。

| 名前 | `e` | 何が来るか |
|---|---|---|
| `command_end` | `command_end` | `pane`, `exit_code?`, `command`, `output_start?`, `output_end?` |
| `pane` | `pane_opened` / `pane_closed` | `pane`, `cwd?` |
| `agent` | `agent_state` | `pane`, `state`, `agent?`（hooks が名乗ったもの） |
| `cwd` | `cwd` | `pane`, `cwd` |
| `command` | `command` | `id`, `pane?`, `arg?`（登録した語彙が押された） |

**名乗ったものだけ**が届く。知らない名前を挙げたら `error` が返る
（打ち間違えたまま黙って待たされるのが一番困る）。

`command_end` が返す `output_start` / `output_end` は、そのまま
`get_buffer` に渡せる。

```jsonc
{"t":"subscribe","events":["command_end"]}
{"t":"event","event":{"e":"command_end","pane":1,"exit_code":1,"command":"cargo test","output_start":42,"output_end":98}}
{"t":"get_buffer","pane":1,"start":42,"end":98}
{"t":"buffer","pane":1,"kind":"term","start":42,"lines":["...", "..."]}
```

### 5.2 語彙を足す

```jsonc
{"t":"register_command","command":{
  "id":"ext.blame","title":"行の来歴","title_en":"Blame this line",
  "keys":["ctrl+b"],"menu":"編集"}}
```

足した語彙は**本体のコマンドと同じ道**に載る — コマンドパレットに並び、
`menu` を書けば右クリックメニューのその節に出て、`tsg --run ext.blame` でも
呼べる。押されると `command` の出来事として登録した拡張へ返る。

約束が 3 つある。

- **`id` は `ext.` で始まる。** 名前空間を分けないと、次の版で本体が同じ id を
  使った日に黙って取り合いになる。
- **キーは Ctrl 付きと F キーだけ。** `d` や `w` はモーダルの文法そのもので、
  1 つ持っていかれると `[count] operator motion` の掛け算がその字のところだけ
  欠ける。既定がもう名乗っているキーも渡さない（受け付けなかったキーは
  理由と一緒に画面の下へ出る）。
- **登録した接続が切れたら消える。** 落ちた拡張の項目がメニューに残り続けると、
  押しても何も起きない行が増えていく。

`menu` を書かなければパレット止まりになる。これは `mouse-parity.md` の
「マウス経路を宣言する」がそのまま拡張にも効いている形で、
**宣言しなかったものは黙ってメニューに現れない**。

### 5.3 拡張が自分のペインを持つ

語彙（`register_command`）は「押せる場所」を足すもの。眺めを出したいなら、
拡張はペインを持てる。

```jsonc
{"t":"ext_pane_open","id":"ext.blame","title":"来歴","text":"…","dir":"vertical"}
{"t":"ext_pane","id":"ext.blame","pane":4}
{"t":"ext_pane_write","id":"ext.blame","text":"…新しい中身…"}
```

**同じ `id` で開き直すと同じペインに書く。** そうしないと、コマンドを押す
たびにペインが増えて、片付けるのが人の仕事になる。

中身はテキストで、プロセスは持たない。だからスクロールも `af` も検索も
`:w` も、開いたファイルと同じように効く。

語彙とペインで**落ちたときの扱いが違う**ので、そこだけ注意してほしい。

| | 拡張が落ちたら |
|---|---|
| 語彙（`register_command`） | **消える。** 押しても何も起きない項目が残るのは嘘になる |
| ペイン（`ext_pane_open`） | **残る。** 中身は読めるもので、読んでいる途中に消えるほうが困る（手放すのは `id` だけ） |

### 5.4 拡張が何をしたかを見る

```sh
tsg --ext-log        # 既定は 50 本
tsg --ext-log 200
```

```
  12秒前  blame        ext.blame を足しました
   9秒前  blame        ext.blame のペインを開きました
   3秒前  blame      ✗ ext.other は開いていません
```

**断った理由が必ず出る。** サーバは「断る」と「記録する」を 1 か所に
まとめてあるので、断り方が増えても記録の書き忘れが起きない。
「繋がっているのに何も起きない」を調べるとき、人が最初に見る場所がこれ。

`ext_hello` で名乗ると名前で出る。名乗らなければ `#3` のような接続の番号。

### 5.5 画面へ知らせる

```sh
tsg --notify "ビルドが終わりました"
tsg --notify "テストが落ちました" --error
```

ステータス行に出る。`--warn` / `--error` は色が変わるだけで、段は 3 つしか
無い——読む人が「これは 3 段目だから…」と考え始めた時点で、知らせとしては
失敗している。

**溜めない。** 窓が 1 枚も開いていなければ、どこにも出ない。後から出てくる
知らせは、たいてい手遅れで、しかも文脈を失っている。

### 5.6 いちばん短い拡張

失敗したコマンドだけを拾って書き出す。

```sh
tsg --subscribe command_end | while read -r line; do
  echo "$line" | grep -q '"exit_code":0' || echo "$line" >> failures.jsonl
done
```

### 5.7 ひととおり使った実例

`examples/herdr-agents.py` が、ここまでの口を全部通る形で使っている——
名乗り・語彙・購読・自分のペイン・知らせ。**別のターミナル多重化ランタイム
（herdr）が抱えているエージェントを、tsumugi の中で見る**という中身で、
`tsg --rpc` を子プロセスとして起こして標準入出力で話す。

```sh
py examples/herdr-agents.py -s work
```

拡張が別プロセスなのはこのためで、この台本が落ちても端末は落ちない。

---

## 6. 条件を決めて待つ

```sh
tsg --wait --until done --timeout 600   # エージェントが終わるまで
tsg --wait --until exit:1               # 失敗したコマンドが出るまで
tsg --wait --until text:PASS            # その字が画面に出るまで
tsg --wait --until 're:FAILED \d+'      # 正規表現で
```

**待つのはサーバ。** 画面に出たものを全部見ているのはこちらなので、台本が
生バイトを追いかけて自分で判定しなくていい。待ち始めた時点より前に出ていた
字は見ない（前からあった字ですぐ当たるなら、待った意味が無い）。

条件は組み合わせられる。コマンドラインには載せていない——括弧を打ちながら
組む形にすると、打ち間違いを画面で直す羽目になる——ので、生の口から渡す。

```jsonc
// 「テストが終わって、しかも終了コードが 0 でない」
{"t":"wait","matcher":{"m":"all","of":[
  {"m":"command_end","code":1},
  {"m":"substring","text":"test result"}
]},"timeout_ms":600000}
{"t":"waited","matched":true,"pane":1}
```

`substring` / `regex` / `command_end` / `agent` / `event` / `all` / `any` /
`not`。読めない正規表現も、知らない出来事の名前も、**待ち始める前に**断る
（待たせておいて「実は読めませんでした」は、待った時間が丸ごと無駄になる）。

`event` は購読（§5.1）と**同じ名前**を使う。

```sh
tsg --wait --until event:pane      # ペインが開くか閉じるまで
tsg --wait --until event:command   # 拡張の語彙が押されるまで
```

「起きるまで待つ」と「起きたら知らせて」は同じことを別の向きから見ている
だけなので、名前の付け方まで分けない。分けると片方だけ増える日が来る。

⚠️ `not` は「**この一回の見比べで当たらなかった**」であって「二度と当たら
ない」ではない。出力が来るたびに見比べるので、`not` 単体で待つとたいてい
最初の出力で当たって返る。

---

## 7. 並べ方を持ち運ぶ

```sh
tsg --layout-export > dev.json   # いまの形を書き出す
tsg --layout-apply dev.json      # その形で開く
```

書き出すのは**形だけ**——割る向き・取り分と、葉で開く場所・起動するもの。
ペインの番号は落とす（次に当てるときには、その番号のペインはもう無い）。

**当てるのは新しいタブ。** いまのタブを組み替えると、そこに居るペインを
閉じることになる。走っているものを黙って殺すのは、頼まれていない。

```jsonc
{"k":"split","dir":"horizontal","weights":[60,40],"children":[
  {"k":"leaf","cwd":"/w/app"},
  {"k":"split","dir":"vertical","weights":[50,50],"children":[
    {"k":"leaf","cwd":"/w/app","command":["cargo","watch","-x","test"]},
    {"k":"leaf","cwd":"/w/app"}
  ]}
]}
```

---

## 8. 作業ツリー

```sh
tsg --worktrees                              # 並べる
tsg --worktree-add ../app-fix --branch fix   # 足して、そこで開く
tsg --worktree-open ../app-fix               # 開くだけ
tsg --worktree-remove ../app-fix             # 消す
```

**エージェントを並べるなら、置き場所も要る。** 3 本走らせるのに、1 つの
作業ツリーで 3 つの枝は回せない。`--worktree-add` は足したあと**そのまま
新しいタブで開く**（足しただけで場所を打ち直すのでは、頼んだことの続きが
人の仕事に戻っているだけ）。

消すときは `--force` を渡さない。直しかけが残っていれば git が断る。
**押し切らない**——消えたものは戻らない。

---

## 9. 版

`attach` の `version` がサーバと違えば `error` が返り、そこで終わる。
**当てずっぽうに続けない。** 形が変わったのに古い解釈で読み続けるほうが、
繋がらないより危ない。

| 版 | 変えたこと |
|---|---|
| 20 | 画面への知らせ（`notify`）と、出来事で待てる `matcher`（`event`） |
| 19 | 作業ツリー（`worktree_*`） |
| 18 | 並べ方の書き出しと適用（`layout_export` / `layout_apply`） |
| 17 | 条件を決めて待つ（`wait` と組み合わせられる `matcher`） |
| 16 | 拡張の記録（`ext_hello` / `ext_log`） |
| 15 | 拡張が自分のペインを持てる（`ext_pane_*`） |
| 14 | 言語サーバの「直す側」（`hover` / `references` / `rename`） |
| 13 | 拡張の口（`subscribe` / `register_command` / `get_buffer` と `event` / `ext_commands` / `buffer`） |
| 12 | `snapshot` に `alt` / `alt_cursor` を足した（alt screen を履歴と混ぜない） |
| 8 | `attach` の `cols` / `rows` に 0（＝大きさを持ち込まない）を足した |
| 7 | ファイル編集の差分化（`edit_file`） |
| 2 | 再アタッチの復元を生バイトの再送から `snapshot` へ |
