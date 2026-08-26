//! Language Server Protocol のクライアント。
//!
//! # 何をして、何をしないか
//!
//! **する**: 診断（誤りの位置）・定義へ移動・補完。この 3 つは
//! 「読む・直す」を止めずに続けるために要る。
//!
//! **しない**: 名前の変更・整形・コードアクション。これらは「編集の道具」の
//! 側の機能で、端末の中で少し直すのに要るものではない。入れると LSP の
//! 仕様のかなりの部分を持つことになり、道具の性格が変わる。
//!
//! # どこで走るか
//!
//! **mux サーバの中**。開いているファイルを持っているのがそこなので、
//! 診断もそこに置けば、窓を閉じて開き直しても消えない。
//!
//! # 落ちたらどうなるか
//!
//! **黙って何も出さないだけ。** 言語サーバが入っていない・落ちた・遅いは
//! どれも普通に起きる。そのたびにファイルが開けなくなるなら、
//! 入れないほうがましになる。

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub mod servers;

/// 誤りの重さ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

impl Severity {
    fn from_lsp(n: u64) -> Self {
        match n {
            1 => Self::Error,
            2 => Self::Warning,
            3 => Self::Info,
            _ => Self::Hint,
        }
    }
}

/// 1 つの誤り。**行も桁も 0 起点**（LSP と同じ）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub line: usize,
    pub col: usize,
    pub end_line: usize,
    pub end_col: usize,
    pub severity: Severity,
    pub message: String,
}

/// 補完の候補 1 つ。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Completion {
    /// 入れる文字列。
    pub insert: String,
    /// 一覧に出す字（`insert` と違うことがある）。
    pub label: String,
    /// 種類や型（`fn(&mut self, T)` など）。無いこともある。
    pub detail: Option<String>,
}

/// 行き先（定義へ移動の答え）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub path: String,
    pub line: usize,
    pub col: usize,
}

/// 1 か所の書き換え。**行も桁も 0 起点**、終わりは含まない（LSP と同じ）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextEdit {
    pub line: usize,
    pub col: usize,
    pub end_line: usize,
    pub end_col: usize,
    pub text: String,
}

/// 言語サーバから届いたもの。
#[derive(Debug, Clone)]
pub enum Incoming {
    /// 診断。ファイルごとに**総取り替え**（LSP がそういう形）。
    Diagnostics {
        path: String,
        items: Vec<Diagnostic>,
    },
    /// 問い合わせの答え。`id` は投げたときの番号。
    Answer { id: u64, result: Value },
}

/// 走っている言語サーバ 1 つ。
pub struct Server {
    child: Child,
    stdin: ChildStdin,
    pub rx: Receiver<Incoming>,
    next_id: u64,
    /// 送った版。**LSP は版が戻ると壊れる**ので、ファイルごとに数える。
    versions: BTreeMap<String, i64>,
}

impl Drop for Server {
    fn drop(&mut self) {
        // 行儀よく終わらせる余裕は無い（落とすときはたいてい急いでいる）。
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    /// 起こす。**入っていなければエラー**（呼ぶ側が黙って諦める）。
    pub fn start(program: &str, args: &[String], root: &str) -> Result<Self> {
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd.current_dir(root);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        // 言語サーバは進み具合をよく吐く。読まないので捨てる。
        cmd.stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("{program} を起こせません"))?;
        let stdin = child.stdin.take().context("入力を掴めません")?;
        let stdout = child.stdout.take().context("出力を掴めません")?;

        let (tx, rx) = channel();
        thread::spawn(move || read_loop(stdout, &tx));

        let mut server = Self {
            child,
            stdin,
            rx,
            next_id: 1,
            versions: BTreeMap::new(),
        };
        server.initialize(root)?;
        Ok(server)
    }

    fn initialize(&mut self, root: &str) -> Result<()> {
        let uri = path_to_uri(root);
        let id = self.send_request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": uri,
                "capabilities": {
                    "textDocument": {
                        "synchronization": { "didSave": true },
                        "publishDiagnostics": {},
                        "definition": { "linkSupport": true },
                        "completion": {
                            "completionItem": { "snippetSupport": false }
                        },
                        // ここから「直す側」。**読む側だけでは足りない**
                        // ——名前を変える・使われている場所を並べる・
                        // その場で意味を訊く、が編集の掛け算になる。
                        "hover": { "contentFormat": ["plaintext", "markdown"] },
                        "references": {},
                        "rename": { "prepareSupport": false }
                    },
                    "workspace": { "configuration": false }
                }
            }),
        )?;

        // **答えを待ってから `initialized` を出す。** 仕様がそう決めていて、
        // 先に出すと相手が黙り込む（rust-analyzer で実際に踏んだ。
        // 走ってはいるのに診断が 1 つも来ない）。
        //
        // 待つのは 30 秒まで。大きなリポジトリだと `initialize` の答えが
        // 返るまで時間がかかる。それでも来なければ諦めて先へ進む
        // （何も出ないだけで、開くのは止めない）。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self
                .rx
                .recv_timeout(left.min(std::time::Duration::from_millis(200)))
            {
                Ok(Incoming::Answer { id: got, .. }) if got == id => break,
                // 起きる前に届いたものは捨てる（この時点では誰も見ていない）。
                Ok(_) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                // 相手が落ちた。**待ち続けない。**
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        self.send_notification("initialized", json!({}))
    }

    fn write(&mut self, body: &Value) -> Result<()> {
        let text = serde_json::to_string(body)?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n{text}", text.len())?;
        self.stdin.flush()?;
        Ok(())
    }

    fn send_request(&mut self, method: &str, params: Value) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        Ok(id)
    }

    fn send_notification(&mut self, method: &str, params: Value) -> Result<()> {
        self.write(&json!({"jsonrpc":"2.0","method":method,"params":params}))
    }

    /// ファイルを開いたと伝える。
    pub fn did_open(&mut self, path: &str, language: &str, text: &str) -> Result<()> {
        self.versions.insert(path.to_string(), 1);
        self.send_notification(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri": path_to_uri(path),
                "languageId": language,
                "version": 1,
                "text": text,
            }}),
        )
    }

    /// 中身が変わったと伝える。**全文で送る。**
    ///
    /// 差分で送るほうが速いが、こちらの差分と LSP の差分は形が違うので、
    /// 変換のところに間違いが入る。**間違えると診断の位置が全部ずれる**ので、
    /// 速さより正しさを取る。
    pub fn did_change(&mut self, path: &str, text: &str) -> Result<()> {
        let v = self.versions.entry(path.to_string()).or_insert(1);
        *v += 1;
        let version = *v;
        self.send_notification(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": path_to_uri(path), "version": version },
                "contentChanges": [ { "text": text } ]
            }),
        )
    }

    /// 保存したと伝える。
    ///
    /// **これが無いと rust-analyzer は `cargo check` を走らせない。**
    /// 型の誤りは `cargo` にしか分からないので、保存を伝えないと
    /// 構文の誤りしか出てこない。
    pub fn did_save(&mut self, path: &str) -> Result<()> {
        self.send_notification(
            "textDocument/didSave",
            json!({"textDocument":{"uri": path_to_uri(path)}}),
        )
    }

    pub fn did_close(&mut self, path: &str) -> Result<()> {
        self.versions.remove(path);
        self.send_notification(
            "textDocument/didClose",
            json!({"textDocument":{"uri": path_to_uri(path)}}),
        )
    }

    /// 定義へ。答えは `Incoming::Answer` で返る。
    pub fn definition(&mut self, path: &str, line: usize, col: usize) -> Result<u64> {
        self.send_request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": path_to_uri(path) },
                "position": { "line": line, "character": col }
            }),
        )
    }

    /// 補完。答えは `Incoming::Answer` で返る。
    pub fn completion(&mut self, path: &str, line: usize, col: usize) -> Result<u64> {
        self.send_request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": path_to_uri(path) },
                "position": { "line": line, "character": col }
            }),
        )
    }

    /// その場で意味を訊く（`K`）。
    pub fn hover(&mut self, path: &str, line: usize, col: usize) -> Result<u64> {
        self.send_request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": path_to_uri(path) },
                "position": { "line": line, "character": col }
            }),
        )
    }

    /// 使われている場所（`gr`）。**宣言も入れる** — 探しているのは
    /// 「この名前がどこに出るか」で、宣言だけ抜けていると数が合わない。
    pub fn references(&mut self, path: &str, line: usize, col: usize) -> Result<u64> {
        self.send_request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": path_to_uri(path) },
                "position": { "line": line, "character": col },
                "context": { "includeDeclaration": true }
            }),
        )
    }

    /// 名前を変える（`gn`）。答えは WorkspaceEdit。
    pub fn rename(&mut self, path: &str, line: usize, col: usize, new_name: &str) -> Result<u64> {
        self.send_request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": path_to_uri(path) },
                "position": { "line": line, "character": col },
                "newName": new_name
            }),
        )
    }
}

/// 答えを読み続ける。**壊れた頭は捨てて次を待つ**（落とさない）。
fn read_loop(stdout: impl Read, tx: &Sender<Incoming>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let Some(len) = read_content_length(&mut reader) else {
            return;
        };
        let mut buf = vec![0u8; len];
        if reader.read_exact(&mut buf).is_err() {
            return;
        }
        let Ok(msg) = serde_json::from_slice::<Value>(&buf) else {
            continue;
        };
        let out = match msg.get("method").and_then(Value::as_str) {
            Some("textDocument/publishDiagnostics") => parse_diagnostics(&msg),
            // それ以外の知らせ（進み具合など）は読まない。
            Some(_) => None,
            None => msg.get("id").and_then(Value::as_u64).map(|id| {
                let result = msg.get("result").cloned().unwrap_or(Value::Null);
                Incoming::Answer { id, result }
            }),
        };
        if let Some(out) = out
            && tx.send(out).is_err()
        {
            return;
        }
    }
}

/// 頭を読んで本文の長さを返す。
fn read_content_length(reader: &mut impl BufRead) -> Option<usize> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None; // 相手が終わった
        }
        let line = line.trim_end();
        if line.is_empty() {
            return len; // 空行で頭が終わる
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            len = v.trim().parse().ok();
        }
    }
}

fn parse_diagnostics(msg: &Value) -> Option<Incoming> {
    let params = msg.get("params")?;
    let path = uri_to_path(params.get("uri")?.as_str()?)?;
    let items = params
        .get("diagnostics")?
        .as_array()?
        .iter()
        .filter_map(parse_one_diagnostic)
        .collect();
    Some(Incoming::Diagnostics { path, items })
}

fn parse_one_diagnostic(d: &Value) -> Option<Diagnostic> {
    let range = d.get("range")?;
    let start = range.get("start")?;
    let end = range.get("end")?;
    Some(Diagnostic {
        line: start.get("line")?.as_u64()? as usize,
        col: start.get("character")?.as_u64()? as usize,
        end_line: end.get("line")?.as_u64()? as usize,
        end_col: end.get("character")?.as_u64()? as usize,
        severity: Severity::from_lsp(d.get("severity").and_then(Value::as_u64).unwrap_or(1)),
        // 下の行は 1 行しかない。**長い説明の 1 行目だけ**を持つ。
        message: d
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .lines()
            .next()
            .unwrap_or_default()
            .to_string(),
    })
}

/// 定義の答えを読む。**形が 3 通りある**（1 つ / 並び / リンク）。
pub fn parse_definition(result: &Value) -> Option<Location> {
    let one = match result {
        Value::Array(a) => a.first()?,
        Value::Object(_) => result,
        _ => return None,
    };
    // `LocationLink` なら行き先が別の名前で入っている。
    let (uri, range) = match one.get("targetUri") {
        Some(u) => (
            u,
            one.get("targetSelectionRange")
                .or_else(|| one.get("targetRange"))?,
        ),
        None => (one.get("uri")?, one.get("range")?),
    };
    let start = range.get("start")?;
    Some(Location {
        path: uri_to_path(uri.as_str()?)?,
        line: start.get("line")?.as_u64()? as usize,
        col: start.get("character")?.as_u64()? as usize,
    })
}

/// `textDocument/hover` の答え。人が読む 1 かたまりにする。
///
/// **形が 3 通りある**（文字列 / `{value}` / その並び）。どれも
/// 「中身は `contents`」という一点だけは同じなので、そこから畳む。
pub fn parse_hover(result: &Value) -> Option<String> {
    fn one(v: &Value) -> Option<String> {
        match v {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => o.get("value")?.as_str().map(str::to_string),
            _ => None,
        }
    }
    let contents = result.get("contents")?;
    let text = match contents {
        Value::Array(a) => a.iter().filter_map(one).collect::<Vec<_>>().join("\n"),
        other => one(other)?,
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// 場所の並び（参照）。**`Location` と `LocationLink` の両方を受ける。**
pub fn parse_locations(result: &Value) -> Vec<Location> {
    let items = match result {
        Value::Array(a) => a.as_slice(),
        Value::Object(_) => std::slice::from_ref(result),
        _ => return Vec::new(),
    };
    items
        .iter()
        .filter_map(|one| {
            let (uri, range) = match one.get("targetUri") {
                Some(u) => (
                    u,
                    one.get("targetSelectionRange")
                        .or_else(|| one.get("targetRange"))?,
                ),
                None => (one.get("uri")?, one.get("range")?),
            };
            let start = range.get("start")?;
            Some(Location {
                path: uri_to_path(uri.as_str()?)?,
                line: start.get("line")?.as_u64()? as usize,
                col: start.get("character")?.as_u64()? as usize,
            })
        })
        .collect()
}

/// 名前を変える答え（WorkspaceEdit）を、ファイルごとに割る。
///
/// **形が 2 通りある**（`changes` / `documentChanges`）。返すのは
/// パスごとの書き換えで、どのファイルが何か所かを呼ぶ側が数えられるようにする
/// （「このファイルだけ当てて、他は当てない」と正直に言うために要る）。
pub fn parse_rename(result: &Value) -> Vec<(String, Vec<TextEdit>)> {
    fn edits_of(v: &Value) -> Vec<TextEdit> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|e| {
                        let range = e.get("range")?;
                        let start = range.get("start")?;
                        let end = range.get("end")?;
                        Some(TextEdit {
                            line: start.get("line")?.as_u64()? as usize,
                            col: start.get("character")?.as_u64()? as usize,
                            end_line: end.get("line")?.as_u64()? as usize,
                            end_col: end.get("character")?.as_u64()? as usize,
                            text: e.get("newText")?.as_str()?.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    let mut out: Vec<(String, Vec<TextEdit>)> = Vec::new();
    if let Some(changes) = result.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            if let Some(path) = uri_to_path(uri) {
                out.push((path, edits_of(edits)));
            }
        }
    }
    if let Some(docs) = result.get("documentChanges").and_then(Value::as_array) {
        for d in docs {
            // `create` / `rename` / `delete` は当てない（ファイルを作る・
            // 消すのは、名前を変えるつもりの人が頼んだことではない）。
            let Some(uri) = d.pointer("/textDocument/uri").and_then(Value::as_str) else {
                continue;
            };
            if let Some(path) = uri_to_path(uri)
                && let Some(edits) = d.get("edits")
            {
                out.push((path, edits_of(edits)));
            }
        }
    }
    out.retain(|(_, e)| !e.is_empty());
    out
}

/// 補完の答えを読む。**形が 2 通りある**（並び / `items` を持つ物）。
pub fn parse_completions(result: &Value, limit: usize) -> Vec<Completion> {
    let items = match result {
        Value::Array(a) => a.as_slice(),
        Value::Object(o) => match o.get("items").and_then(Value::as_array) {
            Some(a) => a.as_slice(),
            None => return Vec::new(),
        },
        _ => return Vec::new(),
    };
    items
        .iter()
        .filter_map(|i| {
            let label = i.get("label")?.as_str()?.trim().to_string();
            if label.is_empty() {
                return None;
            }
            // 入れる字は `insertText` があればそちら。無ければ見出し。
            let insert = i
                .get("insertText")
                .and_then(Value::as_str)
                .unwrap_or(&label)
                .to_string();
            Some(Completion {
                insert,
                label,
                detail: i
                    .get("detail")
                    .and_then(Value::as_str)
                    .map(|s| s.lines().next().unwrap_or(s).to_string()),
            })
        })
        .take(limit)
        .collect()
}

/// パス -> `file://` の URI。
///
/// **Windows の `\` とドライブ名を通す。** `C:\a\b` は `file:///C:/a/b`。
/// ここを間違えると、診断がどのファイルのものか分からなくなる。
pub fn path_to_uri(path: &str) -> String {
    let p = path.replace('\\', "/");
    let p = if p.starts_with('/') {
        p
    } else {
        format!("/{p}")
    };
    // 空白などは百分率で書く。**すべてを書き換えない**（`/` や `:` は残す）。
    let escaped: String = p
        .chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            '#' => "%23".to_string(),
            '?' => "%3F".to_string(),
            c => c.to_string(),
        })
        .collect();
    format!("file://{escaped}")
}

/// `file://` の URI -> パス。読めなければ `None`。
pub fn uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///C:/a` の形。ホスト部が空なので `/` が 1 つ残る。
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    let decoded = percent_decode(rest);
    // ドライブ名なら `\` へ戻す。
    let looks_windows = decoded.as_bytes().get(1) == Some(&b':');
    Some(if looks_windows {
        decoded.replace('/', "\\")
    } else {
        format!("/{decoded}")
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Windows のパスが往復する。** ここがずれると、診断がどのファイルの
    /// ものか分からなくなる。
    #[test]
    fn a_windows_path_survives_the_round_trip() {
        let p = r"C:\Users\me\dev\a.rs";
        let uri = path_to_uri(p);
        assert_eq!(uri, "file:///C:/Users/me/dev/a.rs");
        assert_eq!(uri_to_path(&uri).as_deref(), Some(p));
    }

    #[test]
    fn a_unix_path_survives_the_round_trip() {
        let p = "/home/me/dev/a.rs";
        let uri = path_to_uri(p);
        assert_eq!(uri, "file:///home/me/dev/a.rs");
        assert_eq!(uri_to_path(&uri).as_deref(), Some(p));
    }

    /// 空白の入ったパスも通る。
    #[test]
    fn a_space_in_the_path_is_escaped_and_comes_back() {
        let p = r"C:\My Documents\a.rs";
        let uri = path_to_uri(p);
        assert!(uri.contains("%20"), "空白が書き換えられていない: {uri}");
        assert_eq!(uri_to_path(&uri).as_deref(), Some(p));
    }

    /// 頭を読んで本文の長さが分かる。
    #[test]
    fn the_header_gives_the_body_length() {
        let mut r =
            std::io::BufReader::new(&b"Content-Length: 42\r\nContent-Type: x\r\n\r\nbody"[..]);
        assert_eq!(read_content_length(&mut r), Some(42));
    }

    /// **相手が終わったら終わる。** 空の入力で回り続けない。
    #[test]
    fn an_ended_stream_stops_the_reader() {
        let mut r = std::io::BufReader::new(&b""[..]);
        assert_eq!(read_content_length(&mut r), None);
    }

    /// 定義の答えは 3 通りの形で来る。どれも読める。
    #[test]
    fn a_definition_is_read_in_any_of_its_three_shapes() {
        let want = Location {
            path: if cfg!(windows) {
                r"C:\a\b.rs".into()
            } else {
                "/a/b.rs".into()
            },
            line: 3,
            col: 7,
        };
        let uri = path_to_uri(&want.path);
        let range = json!({"start":{"line":3,"character":7},"end":{"line":3,"character":9}});

        let single = json!({"uri": uri, "range": range});
        assert_eq!(parse_definition(&single).as_ref(), Some(&want));

        let many = json!([{"uri": uri, "range": range}]);
        assert_eq!(parse_definition(&many).as_ref(), Some(&want));

        let link = json!([{"targetUri": uri, "targetSelectionRange": range}]);
        assert_eq!(parse_definition(&link).as_ref(), Some(&want));

        assert!(
            parse_definition(&json!(null)).is_none(),
            "無い答えを読んでいる"
        );
    }

    /// 補完は 2 通りの形で来る。どちらも読める。
    #[test]
    fn completions_are_read_in_either_shape() {
        let items = json!([
            {"label":"push","insertText":"push","detail":"fn(&mut self, T)"},
            {"label":"pop"}
        ]);
        let flat = parse_completions(&items, 10);
        assert_eq!(flat.len(), 2);
        assert_eq!(flat[0].insert, "push");
        // `insertText` が無ければ見出しを入れる
        assert_eq!(flat[1].insert, "pop");

        let wrapped = json!({"isIncomplete": false, "items": items});
        assert_eq!(parse_completions(&wrapped, 10).len(), 2);

        // **数は絞る。** 何百も返ってくることがある
        assert_eq!(parse_completions(&wrapped, 1).len(), 1);
    }

    /// 診断を読む。**位置も重さも落とさない。**
    #[test]
    fn a_diagnostic_keeps_its_place_and_weight() {
        let path = if cfg!(windows) {
            r"C:\a\b.rs"
        } else {
            "/a/b.rs"
        };
        let msg = json!({
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": path_to_uri(path),
                "diagnostics": [{
                    "range": {"start":{"line":1,"character":2},"end":{"line":1,"character":5}},
                    "severity": 2,
                    "message": "unused variable\nnote: more"
                }]
            }
        });
        let Some(Incoming::Diagnostics { path: got, items }) = parse_diagnostics(&msg) else {
            panic!("読めない");
        };
        assert_eq!(got, path);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].severity, Severity::Warning);
        assert_eq!(items[0].line, 1);
        assert_eq!(items[0].col, 2);
        assert_eq!(items[0].end_col, 5);
        // 下の行は 1 行しかない。**長い説明の 1 行目だけ**を持つ
        assert_eq!(items[0].message, "unused variable");
    }

    // ---- 直す側 -----------------------------------------------------------

    /// `contents` の形は 3 通りある。**どれで来ても読めること。**
    #[test]
    fn hover_reads_all_three_shapes() {
        let plain = json!({ "contents": "fn main()" });
        assert_eq!(parse_hover(&plain).as_deref(), Some("fn main()"));

        let marked = json!({ "contents": { "kind": "markdown", "value": "fn main()" } });
        assert_eq!(parse_hover(&marked).as_deref(), Some("fn main()"));

        let many = json!({ "contents": ["fn main()", { "value": "in crate x" }] });
        assert_eq!(parse_hover(&many).as_deref(), Some("fn main()\nin crate x"));
    }

    /// 「そこには何も無い」を、空文字ではなく `None` で返す。
    #[test]
    fn an_empty_hover_is_none_not_a_blank_line() {
        assert!(parse_hover(&json!({ "contents": "   " })).is_none());
        assert!(parse_hover(&json!({ "contents": [] })).is_none());
        assert!(parse_hover(&json!({})).is_none());
    }

    #[test]
    fn references_read_both_location_shapes() {
        let plain = json!([
            { "uri": "file:///c:/x/a.rs", "range": { "start": { "line": 3, "character": 4 } } },
            { "targetUri": "file:///c:/x/b.rs",
              "targetSelectionRange": { "start": { "line": 9, "character": 1 } } },
        ]);
        let got = parse_locations(&plain);
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].line, got[0].col), (3, 4));
        assert_eq!((got[1].line, got[1].col), (9, 1));
    }

    /// 名前を変える答えも形が 2 通り。**どちらもファイルごとに割れること。**
    #[test]
    fn rename_reads_both_workspace_edit_shapes() {
        let changes = json!({
            "changes": {
                "file:///c:/x/a.rs": [{
                    "range": { "start": { "line": 1, "character": 4 },
                               "end": { "line": 1, "character": 7 } },
                    "newText": "bar"
                }]
            }
        });
        let got = parse_rename(&changes);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1[0].text, "bar");
        assert_eq!((got[0].1[0].col, got[0].1[0].end_col), (4, 7));

        let docs = json!({
            "documentChanges": [{
                "textDocument": { "uri": "file:///c:/x/a.rs", "version": 1 },
                "edits": [{
                    "range": { "start": { "line": 0, "character": 0 },
                               "end": { "line": 0, "character": 3 } },
                    "newText": "baz"
                }]
            }]
        });
        let got = parse_rename(&docs);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1[0].text, "baz");
    }

    /// ファイルを作る・消す指示は当てない。
    /// **名前を変えるつもりの人が頼んだことではない。**
    #[test]
    fn rename_ignores_file_creation_and_deletion() {
        let docs = json!({
            "documentChanges": [
                { "kind": "create", "uri": "file:///c:/x/new.rs" },
                { "kind": "delete", "uri": "file:///c:/x/old.rs" },
            ]
        });
        assert!(parse_rename(&docs).is_empty());
    }
}
