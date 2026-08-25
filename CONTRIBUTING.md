# 手を入れる / Contributing

日本語でも英語でもかまいません。/ Japanese or English, either is fine.

## 先に読むもの

`docs/` の 4 本が正本で、コードより先にあります。迷ったら
[`docs/concept.md`](docs/concept.md) の中心命題へ戻ってください。

- **グリッドはドキュメントである。** プロセスはその末尾に追記しているだけ。
- **すべてのコマンドにマウスの道がある。** 無いとテストが落ちます。
- **`tsg-modal` は純粋。** I/O を持ち込まないでください。
- **多重化は必ず別プロセス。**

## 動かす

```
cargo build
cargo test --all
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

CI は Windows / macOS / Linux で上と同じことをします。

## 書き方

- **コメントは「なぜ」を書く。** 何をしているかはコードが言います。
  やめた選択肢と、その理由が残っていると後から助かります。
- **テストの名前は主張にする。** `test_foo` ではなく
  `the_terminal_answers_when_it_is_asked` のように、落ちたときに
  何が壊れたか分かる名前を。
- **実バグは推測で直さない。** 再現させてから直してください。
  マイルストーンの記録（`docs/m*-results.md`）は、うまくいかなかったことも
  書く場所です。

## 提案

機能名ではなく「いま何ができなくて困っているか」から書いてください。
解き方はこちらで一緒に考えます。
