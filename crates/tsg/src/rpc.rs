//! 外から動かすための口。
//!
//! mux は最初から**プロセス間のプロトコル**（JSON Lines）で動いているので、
//! 外部から叩ける口はもともと在る。ここでやるのは、それを
//! **人と台本が使える形にして、約束として公開する**こと。
//!
//! - `--list`      走っているセッション
//! - `--send`      いまのペインへ文字を送る
//! - `--capture`   ペインに見えているものをテキストで取る
//! - `--tap`       出てくるバイト列を覗く
//! - `--rpc`       生のプロトコルを標準入出力で話す（**最後の逃げ道**）
//!
//! 便利な口をいくつ足しても、必ず「それでは足りない」が来る。そのときに
//! こちらの実装を待たずに済むよう、生の口を最初から開けておく。
//! 形は `docs/rpc.md`。
//!
//! # 誰が話せるか
//!
//! ソケットは**そのユーザだけに閉じている**（`tsg_mux::endpoint`）。
//! ここに繋げるということは、すでにそのユーザとしてプロセスを起こせる
//! ということなので、これ以上の認証は置いていない。逆に言えば
//! **ソケットの権限がこの口の唯一の錠**で、そこを緩めてはいけない。

use std::io::{BufRead, Write};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tsg_mux::{Client, ClientMsg, PROTOCOL_VERSION, ServerMsg, SessionInfo};

/// 窓を持たないクライアントが名乗る大きさ。
///
/// **0 は「大きさを持ち込まない」。** 台本から覗くだけの相手が 80x24 を
/// 名乗ると、その後に開くペインまでその大きさになる。
const NO_SIZE: (u16, u16) = (0, 0);

/// 応答を待つ上限。返らないときに黙って固まらせない。
const TIMEOUT: Duration = Duration::from_secs(5);

/// 繋いで Attach まで済ませ、セッションの形を返す。
fn attach(session: &str) -> Result<(Client, SessionInfo)> {
    let mut client = Client::connect(session)
        .with_context(|| format!("セッション '{session}' が見つかりません"))?;
    client.send(&ClientMsg::Attach {
        version: PROTOCOL_VERSION,
        cols: NO_SIZE.0,
        rows: NO_SIZE.1,
        cwd: None,
        command: None,
    })?;
    let reply = client
        .wait_for(TIMEOUT, |m| {
            matches!(m, ServerMsg::Attached { .. } | ServerMsg::Error { .. })
        })
        .context("Attach に応答がありません")?;
    match reply {
        ServerMsg::Attached { session: info, .. } => Ok((client, info)),
        ServerMsg::Error { message } => bail!("{message}"),
        _ => bail!("想定外の応答"),
    }
}

/// いま見えているペイン。
fn active_pane(info: &SessionInfo) -> u32 {
    info.tabs
        .iter()
        .find(|t| t.id == info.active_tab)
        .map_or(0, |t| t.active_pane)
}

/// 走っているセッション名を 1 行ずつ。
///
/// **繋がるものだけを出す。** 落ちたサーバの控えを混ぜると、台本が
/// 「在る」と思って繋ぎに行って失敗する。
pub fn list() -> Result<()> {
    let names = tsg_mux::sessions::live();
    if names.is_empty() {
        eprintln!("走っているセッションはありません");
        return Ok(());
    }
    let mut out = std::io::stdout().lock();
    for name in names {
        writeln!(out, "{name}")?;
    }
    Ok(())
}

/// いまのペインへ文字を送る。`\n` は改行、`\e` は Esc。
pub fn send(session: &str, text: &str) -> Result<()> {
    let (mut client, info) = attach(session)?;
    let pane = active_pane(&info);

    let bytes = text.replace(r"\n", "\r").replace(r"\e", "\x1b");
    client.send(&ClientMsg::Input {
        pane,
        data: tsg_mux::encode_bytes(bytes.as_bytes()),
    })?;
    // 送り終える前に切ると取りこぼす
    std::thread::sleep(Duration::from_millis(300));
    client.send(&ClientMsg::Detach)?;
    eprintln!("ペイン {pane} へ {} バイト送りました", bytes.len());
    Ok(())
}

/// ペインに見えているものをテキストで取る。
///
/// サーバが再アタッチ用に持っている画面（`Snapshot`）をそのまま使う。
/// 見えているものと同じであることが構造的に保証されるので、
/// 取り出し専用の別経路を作らない。
pub fn capture(session: &str, pane: Option<u32>) -> Result<()> {
    let (client, info) = attach(session)?;
    let want = pane.unwrap_or_else(|| active_pane(&info));
    if !info.panes.iter().any(|p| p.id == want) {
        let ids: Vec<String> = info.panes.iter().map(|p| p.id.to_string()).collect();
        bail!("ペイン {want} はありません（在るのは {}）", ids.join(" "));
    }

    let found = client.wait_for(
        TIMEOUT,
        |m| matches!(m, ServerMsg::Snapshot { pane, .. } if *pane == want),
    );
    let Some(ServerMsg::Snapshot { lines, .. }) = found else {
        bail!("ペイン {want} の中身が返りません");
    };

    let mut out = std::io::stdout().lock();
    for line in lines {
        writeln!(out, "{}", strip_ansi(&line))?;
    }
    Ok(())
}

/// SGR などのエスケープを落として素のテキストにする。
///
/// スナップショットは色付きの ANSI で来る（画面の復元にそのまま食わせるため）。
/// 台本が欲しいのはたいてい字だけなので、ここで落とす。
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // `ESC [ ... 英字` と `ESC ] ... BEL|ST` を読み飛ばす。
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            Some(']') => {
                let mut prev = '\0';
                for c in chars.by_ref() {
                    if c == '\u{7}' || (prev == '\u{1b}' && c == '\\') {
                        break;
                    }
                    prev = c;
                }
            }
            // それ以外の 2 バイト列は、後ろの 1 文字ごと捨てる。
            _ => {}
        }
    }
    out.trim_end().to_string()
}

/// 生のバイト列を覗く。
pub fn tap(session: &str) -> Result<()> {
    let (client, _) = attach(session)?;
    eprintln!("--tap: セッション '{session}' を覗いています（Ctrl-C で終了）");
    let mut out = std::io::stdout().lock();
    loop {
        match client.recv_timeout(Duration::from_millis(500)) {
            Some(ServerMsg::Output { pane, data }) => {
                if let Some(bytes) = tsg_mux::decode_bytes(&data) {
                    writeln!(out, "[pane {pane}] {}", escape_bytes(&bytes))?;
                    out.flush()?;
                }
            }
            Some(ServerMsg::Resized { pane, cols, rows }) => {
                writeln!(out, "[pane {pane}] === RESIZED {cols}x{rows} ===")?;
                out.flush()?;
            }
            Some(_) | None => {}
        }
    }
}

/// 生のプロトコルを標準入出力で話す。
///
/// 標準入力の 1 行 = `ClientMsg` 1 通、標準出力の 1 行 = `ServerMsg` 1 通。
/// **Attach は済ませてから渡す**（最初の 1 行に `Attached` が出る）ので、
/// 台本はペインの id をそこから読める。
///
/// 送る側が壊れた行を出したら、その行だけ捨てて `{"t":"error"}` を返す。
/// 落とさないのは、対話的に手で打って形を覚える使い方を潰さないため。
pub fn raw(session: &str) -> Result<()> {
    let (mut client, info) = attach(session)?;
    emit(&ServerMsg::Attached {
        version: PROTOCOL_VERSION,
        session: info,
    })?;

    // **接続は 1 本**にする。2 本張ると Attach が 2 回走って
    // スナップショットが二重に出る。読む係へ客を預け、送る側は
    // チャネル越しに頼む。
    let (tx, outgoing) = std::sync::mpsc::channel::<ClientMsg>();
    let pump = std::thread::spawn(move || {
        loop {
            while let Ok(msg) = outgoing.try_recv() {
                let stop = matches!(msg, ClientMsg::Detach);
                if client.send(&msg).is_err() || stop {
                    return;
                }
            }
            if let Some(msg) = client.recv_timeout(Duration::from_millis(50))
                && emit(&msg).is_err()
            {
                return;
            }
        }
    });

    // 送る側は、標準入力が閉じたら終わる。
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<ClientMsg>(&line) {
            // 壊れた行はその行だけ捨てて知らせる。落とさないのは、
            // 手で打って形を覚える使い方を潰さないため。
            Err(e) => emit(&ServerMsg::Error {
                message: format!("読めない行: {e}"),
            })?,
            Ok(msg) => {
                if tx.send(msg).is_err() {
                    break;
                }
            }
        }
    }
    let _ = tx.send(ClientMsg::Detach);
    drop(tx);
    let _ = pump.join();
    Ok(())
}

/// 1 通を標準出力へ。**1 行 1 通**を崩さない。
fn emit(msg: &ServerMsg) -> Result<()> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{}", serde_json::to_string(msg)?)?;
    out.flush()?;
    Ok(())
}

/// 制御文字を目に見える形にする。
pub fn escape_bytes(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &b in bytes {
        match b {
            0x1b => out.push_str("\\e"),
            b'\r' => out.push_str("\\r"),
            b'\n' => out.push_str("\\n"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_is_stripped_down_to_the_characters() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m"), "red");
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("a\u{1b}[38;5;208mb"), "ab");
        // OSC（タイトルなど）は BEL でも ST でも閉じる
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}x"), "x");
        assert_eq!(strip_ansi("\u{1b}]7;file:///tmp\u{1b}\\y"), "y");
        // 行末の空白は落とす（画面は桁いっぱいまで空白で埋まっている）
        assert_eq!(strip_ansi("text     "), "text");
    }

    /// 閉じていないエスケープで固まらない・落ちない。
    /// **中身は子プロセスが自由に出せる**ので、壊れた入力は必ず来る。
    #[test]
    fn an_unterminated_escape_does_not_hang_or_panic() {
        assert_eq!(strip_ansi("\u{1b}["), "");
        assert_eq!(strip_ansi("\u{1b}]0;no end"), "");
        assert_eq!(strip_ansi("\u{1b}"), "");
    }

    #[test]
    fn bytes_are_shown_in_a_form_that_can_be_read_back() {
        assert_eq!(escape_bytes(b"ok\r\n"), "ok\\r\\n");
        assert_eq!(escape_bytes(&[0x1b, b'[', b'A']), "\\e[A");
        assert_eq!(escape_bytes(&[0x00, 0xff]), "\\x00\\xff");
    }

    /// 窓を持たない口が**大きさを持ち込まない**こと。
    /// ここが 80x24 だと、台本を 1 回叩いただけで次に開くペインが縮む。
    #[test]
    fn a_headless_client_does_not_bring_a_size() {
        assert_eq!(NO_SIZE, (0, 0));
    }
}
