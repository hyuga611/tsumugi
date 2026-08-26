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
use tsg_mux::protocol::AgentState;
use tsg_mux::{Client, ClientMsg, Match, PROTOCOL_VERSION, ServerMsg, SessionInfo};

/// 窓を持たないクライアントが名乗る大きさ。
///
/// **0 は「大きさを持ち込まない」。** 台本から覗くだけの相手が 80x24 を
/// 名乗ると、その後に開くペインまでその大きさになる。
const NO_SIZE: (u16, u16) = (0, 0);

/// 応答を待つ上限。返らないときに黙って固まらせない。
const TIMEOUT: Duration = Duration::from_secs(5);

/// サーバ子プロセスを親から切り離す。
///
/// これを忘れると、GUI が強制終了されたときにサーバも道連れになり、
/// 「ウィンドウを閉じてもシェルは死なない」という約束が破れる（実際に踏んだ）。
#[cfg(windows)]
pub(crate) fn detach(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(unix)]
pub(crate) fn detach(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // 新しいプロセスグループへ。端末からのシグナルを一緒に受けないようにする。
    cmd.process_group(0);
}

#[cfg(not(any(windows, unix)))]
pub(crate) fn detach(_cmd: &mut std::process::Command) {}

/// mux サーバを起こして、繋がるまで待つ。
///
/// **起こすのはここだけ。** 手元から開くときも、遠隔から繋がれたときも
/// 同じ道を通る（別の起こし方を 2 つ持つと、片方だけ直る）。
pub fn spawn_server(session: &str) -> Result<Client> {
    spawn_server_with(session, true)
}

pub fn spawn_server_with(session: &str, restore: bool) -> Result<Client> {
    let exe = std::env::current_exe().context("自分の実行ファイルの場所が分かりません")?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("--server").arg(session);
    if !restore {
        cmd.arg("--no-restore");
    }
    detach(&mut cmd);
    cmd.spawn()
        .with_context(|| format!("mux サーバを起こせません: {}", exe.display()))?;

    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(50));
        if let Ok(c) = Client::connect(session) {
            return Ok(c);
        }
    }
    bail!("mux サーバが 5 秒以内に応答しませんでした")
}

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
        // 台本から覗くだけの口。**組み直しは窓を持つ側の仕事**で、
        // ここから前回の形を出してしまうと、覗いただけで画面が変わる。
        restore: false,
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
    let Some(ServerMsg::Snapshot { lines, alt, .. }) = found else {
        bail!("ペイン {want} の中身が返りません");
    };

    // 全画面アプリの最中なら、**見えているのは alt screen のほう**。
    // 履歴と混ぜて出すと、窓と食い違う。
    let lines = if alt.is_empty() { lines } else { alt };
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

/// `--until` の書き方を `Match` にする。
///
/// **組み合わせ（`all` / `any` / `not`）はコマンドラインに載せない。**
/// 括弧を打ちながら組む形にすると、打ち間違いを画面で直す羽目になる。
/// 組み合わせたいものは生の口（`docs/rpc.md`）から JSON で渡す。
fn parse_matcher(until: &str) -> Result<Match> {
    let t = until.trim();
    if let Some(text) = t.strip_prefix("text:") {
        if text.is_empty() {
            bail!("`text:` の後ろが空です");
        }
        return Ok(Match::Substring {
            text: text.to_string(),
        });
    }
    if let Some(pattern) = t.strip_prefix("re:") {
        let m = Match::Regex {
            pattern: pattern.to_string(),
        };
        m.check().map_err(anyhow::Error::msg)?;
        return Ok(m);
    }
    if let Some(name) = t.strip_prefix("event:") {
        let m = Match::Event {
            name: name.to_string(),
        };
        m.check().map_err(anyhow::Error::msg)?;
        return Ok(m);
    }
    if t == "exit" {
        return Ok(Match::CommandEnd { code: None });
    }
    if let Some(code) = t.strip_prefix("exit:") {
        let code: i32 = code
            .parse()
            .map_err(|_| anyhow::anyhow!("終了コードを読めません: {code}"))?;
        return Ok(Match::CommandEnd { code: Some(code) });
    }
    bail!(
        "'{until}' を知りません（working / blocked / done / failed / idle / \
         text:<字> / re:<正規表現> / exit / exit:<番号> / event:<名前>）"
    )
}

/// サーバに待ってもらう。答えは `Waited` が 1 通。
fn wait_for(session: &str, matcher: Match, timeout: u64, pane: Option<u32>) -> Result<bool> {
    let (mut client, _) = attach(session)?;
    client.send(&ClientMsg::Wait {
        pane,
        matcher,
        timeout_ms: (timeout > 0).then(|| timeout * 1000),
    })?;
    loop {
        match client.recv_timeout(Duration::from_millis(500)) {
            Some(ServerMsg::Waited { matched, .. }) => return Ok(matched),
            // 待ち始める前に断られた（読めない式など）。
            Some(ServerMsg::Error { message }) => bail!("{message}"),
            Some(_) | None => {}
        }
    }
}

/// 走っている窓へ知らせる。**返事は待たない。**
///
/// 窓が 1 枚も開いていなければ、どこにも出ない（溜めない）。後から
/// 出てくる知らせは、たいてい手遅れで、しかも文脈を失っている。
pub fn notify(session: &str, text: &str, level: tsg_mux::Level) -> Result<()> {
    let (mut client, _) = attach(session)?;
    client.send(&ClientMsg::Notify {
        text: text.to_string(),
        level,
    })?;
    std::thread::sleep(Duration::from_millis(150));
    let _ = client.send(&ClientMsg::Detach);
    Ok(())
}

/// 打った側の居場所。ペインの居場所が分からないときの受け皿。
fn here() -> Option<String> {
    std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// git の作業ツリーを並べる。
///
/// **エージェントを並べるなら、置き場所も要る。** 3 本走らせるのに
/// 1 つの作業ツリーで 3 つの枝は回せない。
pub fn worktrees(session: &str) -> Result<()> {
    let (mut client, _) = attach(session)?;
    client.send(&ClientMsg::WorktreeList {
        pane: None,
        cwd: here(),
    })?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        match client.recv_timeout(Duration::from_millis(200)) {
            Some(ServerMsg::Worktrees { items }) => {
                let mut out = std::io::stdout().lock();
                for w in items {
                    writeln!(
                        out,
                        "{}\t{}\t{}",
                        if w.main { "*" } else { " " },
                        if w.branch.is_empty() {
                            "(detached)"
                        } else {
                            &w.branch
                        },
                        w.path
                    )?;
                }
                out.flush()?;
                let _ = client.send(&ClientMsg::Detach);
                return Ok(());
            }
            Some(ServerMsg::Error { message }) => bail!("{message}"),
            Some(_) | None => {}
        }
    }
    bail!("答えが返ってきません")
}

/// その作業ツリーで新しいタブを開く。
pub fn worktree_open(session: &str, path: &str) -> Result<()> {
    let full = std::fs::canonicalize(path)
        .with_context(|| format!("{path} が見つかりません"))?
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .to_string();
    let (mut client, _) = attach(session)?;
    client.send(&ClientMsg::WorktreeOpen { path: full })?;
    std::thread::sleep(Duration::from_millis(300));
    let _ = client.send(&ClientMsg::Detach);
    Ok(())
}

/// 作業ツリーを足して、そこで開く。
pub fn worktree_add(session: &str, path: &str, branch: Option<&str>) -> Result<()> {
    let (mut client, _) = attach(session)?;
    client.send(&ClientMsg::WorktreeAdd {
        pane: None,
        cwd: here(),
        path: path.to_string(),
        branch: branch.map(str::to_string),
    })?;
    // 足せたら形が変わる。断られたら理由が返る。
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        match client.recv_timeout(Duration::from_millis(200)) {
            Some(ServerMsg::Layout(_)) => {
                let _ = client.send(&ClientMsg::Detach);
                return Ok(());
            }
            Some(ServerMsg::Error { message }) => bail!("{message}"),
            Some(_) | None => {}
        }
    }
    bail!("できたかどうか分かりません")
}

/// 作業ツリーを消す。**押し切らない**（`--force` は渡さない）。
pub fn worktree_remove(session: &str, path: &str) -> Result<()> {
    let (mut client, _) = attach(session)?;
    client.send(&ClientMsg::WorktreeRemove {
        pane: None,
        cwd: here(),
        path: path.to_string(),
        force: false,
    })?;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if let Some(ServerMsg::Error { message }) = client.recv_timeout(Duration::from_millis(200))
        {
            bail!("{message}")
        }
    }
    // 断られなければ消えている（消えたことは `--worktrees` で確かめられる）。
    let _ = client.send(&ClientMsg::Detach);
    Ok(())
}

/// いまの並べ方を形だけ書き出す。
///
/// **番号は落とす。** 次に当てるときには、その番号のペインはもう無い。
pub fn layout_export(session: &str) -> Result<()> {
    let (mut client, _) = attach(session)?;
    client.send(&ClientMsg::LayoutExport { tab: None })?;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        match client.recv_timeout(Duration::from_millis(200)) {
            Some(ServerMsg::LayoutSpec { spec }) => {
                let mut out = std::io::stdout().lock();
                writeln!(out, "{}", serde_json::to_string_pretty(&spec)?)?;
                out.flush()?;
                let _ = client.send(&ClientMsg::Detach);
                return Ok(());
            }
            Some(ServerMsg::Error { message }) => bail!("{message}"),
            Some(_) | None => {}
        }
    }
    bail!("形が返ってきません")
}

/// 書き出した形で開く。**新しいタブに開く**（いまのペインは触らない）。
pub fn layout_apply(session: &str, path: &str) -> Result<()> {
    let text = if path == "-" {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut buf)?;
        buf
    } else {
        std::fs::read_to_string(path).with_context(|| format!("{path} を読めません"))?
    };
    let spec: tsg_mux::LayoutSpec =
        serde_json::from_str(&text).context("形として読めません（--layout-export の出力です）")?;
    let (mut client, _) = attach(session)?;
    client.send(&ClientMsg::LayoutApply { spec })?;
    // 形が変われば `layout` が返る。**返るまで切らない**（切ると届く前に終わる）。
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        match client.recv_timeout(Duration::from_millis(200)) {
            Some(ServerMsg::Layout(_)) => {
                let _ = client.send(&ClientMsg::Detach);
                return Ok(());
            }
            Some(ServerMsg::Error { message }) => bail!("{message}"),
            Some(_) | None => {}
        }
    }
    bail!("開けたかどうか分かりません")
}

/// 拡張が何をしたかの記録を出す。
///
/// **断った理由がここに出る。** 「繋がっているのに何も起きない」を
/// 調べるとき、人が最初に見る場所がこれ。
pub fn ext_log(session: &str, limit: Option<usize>) -> Result<()> {
    let (mut client, _) = attach(session)?;
    client.send(&ClientMsg::ExtLog { limit })?;
    let mut out = std::io::stdout().lock();
    // 答えは 1 通だけ。**待ち続けない**（拡張が 1 つも繋がっていなければ空で返る）。
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        if let Some(ServerMsg::ExtLog { entries }) = client.recv_timeout(Duration::from_millis(200))
        {
            if entries.is_empty() {
                eprintln!("記録はまだありません");
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            for e in entries {
                writeln!(
                    out,
                    "{:>8}  {:<12} {} {}",
                    ago(now.saturating_sub(e.at)),
                    e.who,
                    if e.refused { "✗" } else { " " },
                    e.what
                )?;
            }
            out.flush()?;
            let _ = client.send(&ClientMsg::Detach);
            return Ok(());
        }
    }
    bail!("記録が返ってきません")
}

/// 「いつ」を人の言葉で。**時計の形にしない** — 見たいのはたいてい
/// 「さっき何が起きたか」で、そこに要るのは絶対時刻ではなく隔たり。
fn ago(secs: u64) -> String {
    match secs {
        0..=1 => "たった今".into(),
        2..=59 => format!("{secs}秒前"),
        60..=3599 => format!("{}分前", secs / 60),
        3600..=86399 => format!("{}時間前", secs / 3600),
        _ => format!("{}日前", secs / 86400),
    }
}

/// 意味の粒の出来事を覗く。
///
/// `--tap` が生バイトなのに対し、こちらは**シェル統合が言ってきたこと**を
/// そのまま JSON Lines で出す。拡張を書く前に、何が届くのかを目で見るための口。
pub fn subscribe(session: &str, events: &[String]) -> Result<()> {
    let (mut client, _) = attach(session)?;
    client.send(&ClientMsg::Subscribe {
        events: events.to_vec(),
    })?;
    eprintln!(
        "--subscribe: '{session}' の {} を覗いています（Ctrl-C で終了）",
        events.join(", ")
    );
    let mut out = std::io::stdout().lock();
    loop {
        match client.recv_timeout(Duration::from_millis(500)) {
            Some(ServerMsg::Event { event }) => {
                writeln!(out, "{}", serde_json::to_string(&event)?)?;
                out.flush()?;
            }
            // 知らない名前を挙げたときは、黙って待たせない。
            Some(ServerMsg::Error { message }) => eprintln!("{message}"),
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
pub fn raw(session: &str, spawn: bool) -> Result<()> {
    // 遠隔から繋がれたときは、向こうにまだ誰も居ない。起こしてから話す。
    if spawn && Client::connect(session).is_err() {
        spawn_server(session)?;
    }
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

// ---------------------------------------------------------------------------
// エージェント
// ---------------------------------------------------------------------------
//
// **推測しない。** 画面を読んでエージェントの状態を当てにいくと、
// 相手が出力の形を変えた日に黙って壊れる。名乗ってもらう口を開けて、
// hooks から呼ばせる（`--install-agent-hooks`）。

/// 自分の状態を名乗る。hooks から 1 行で呼ばれる。
///
/// **失敗しても 0 で返す。** これはエージェントのフックの中で走るので、
/// tsumugi が居ないところで動かしたときに相手のセッションを壊してはいけない。
pub fn set_agent_state(
    session: &str,
    state: &str,
    pane: Option<u32>,
    cost: Option<String>,
    agent: Option<String>,
) -> Result<()> {
    let Some(state) = AgentState::parse(state) else {
        bail!("状態 '{state}' を知りません（working / blocked / done / failed / idle）");
    };
    let Ok((mut client, _)) = attach(session) else {
        // 走っていないセッションへの報告は、黙って捨てる。
        return Ok(());
    };
    client.send(&ClientMsg::SetAgentState {
        pane,
        state,
        cost,
        agent,
    })?;
    std::thread::sleep(Duration::from_millis(120));
    let _ = client.send(&ClientMsg::Detach);
    Ok(())
}

/// どのペインがどうなっているか。`session<TAB>pane<TAB>state` を 1 行ずつ。
///
/// `--list` の「1 行 1 セッション」を壊さないよう、別の口にしてある。
pub fn agents(session: &str) -> Result<()> {
    let (_client, info) = attach(session)?;
    let mut out = std::io::stdout().lock();
    for p in &info.panes {
        let Some(state) = p.agent else {
            continue;
        };
        writeln!(out, "{}\t{}\t{}", info.name, p.id, state.name())?;
    }
    Ok(())
}

/// その状態になるまで待つ。
///
/// 返り値は終了コードで返す（0 = なった / 2 = 時間切れ）。
/// 台本が `if tsg --wait --until blocked; then ...` と書けることが目的。
pub fn wait(session: &str, until: &str, timeout: u64, pane: Option<u32>) -> Result<bool> {
    // 状態以外の待ち方は**サーバに待たせる**（画面を全部見ているのは向こう）。
    let Some(want) = AgentState::parse(until) else {
        let matcher = parse_matcher(until)?;
        return wait_for(session, matcher, timeout, pane);
    };
    let (client, info) = attach(session)?;
    let matches = |i: &SessionInfo| {
        i.panes
            .iter()
            .filter(|p| pane.is_none_or(|w| w == p.id))
            .any(|p| p.agent == Some(want))
    };
    // 待ち始めた時点でもう条件を満たしているなら、待たない。
    if matches(&info) {
        return Ok(true);
    }
    let deadline = (timeout > 0).then(|| std::time::Instant::now() + Duration::from_secs(timeout));
    loop {
        if let Some(d) = deadline
            && std::time::Instant::now() >= d
        {
            return Ok(false);
        }
        match client.recv_timeout(Duration::from_millis(500)) {
            Some(ServerMsg::Layout(i)) if matches(&i) => return Ok(true),
            Some(_) | None => {}
        }
    }
}

/// エージェントへ文を投げる。`--wait` を付けると、返事待ちに戻るまで待つ。
///
/// **投げた直後は `working` として扱う。** そうしないと、投げる前から
/// `blocked` だったペインを見て「もう返ってきた」と即座に判断してしまう。
pub fn prompt(session: &str, text: &str, pane: Option<u32>, and_wait: bool) -> Result<bool> {
    let (mut client, info) = attach(session)?;
    let target = pane.unwrap_or_else(|| active_pane(&info));
    let bytes = format!("{}\r", text.replace(r"\n", "\r").replace(r"\e", "\x1b"));
    client.send(&ClientMsg::SetAgentState {
        pane: Some(target),
        state: AgentState::Working,
        cost: None,
        agent: None,
    })?;
    client.send(&ClientMsg::Input {
        pane: target,
        data: tsg_mux::encode_bytes(bytes.as_bytes()),
    })?;
    if !and_wait {
        std::thread::sleep(Duration::from_millis(300));
        let _ = client.send(&ClientMsg::Detach);
        eprintln!("ペイン {target} へ投げました");
        return Ok(true);
    }
    eprintln!("ペイン {target} へ投げました。返事を待っています…");
    loop {
        if let Some(ServerMsg::Layout(i)) = client.recv_timeout(Duration::from_millis(500)) {
            let done = i
                .panes
                .iter()
                .find(|p| p.id == target)
                .and_then(|p| p.agent)
                .is_some_and(AgentState::wants_you);
            if done {
                return Ok(true);
            }
        }
    }
}

/// tsumugi の中で `tsg` と打たれたときに、窓ではなくタブを開く。
///
/// **端末エミュレータを端末の中から起動するのは日常**なので、そのたびに
/// 窓が増えるのは邪魔でしかない。中に居ると分かるなら、いまの窓に
/// タブを足して切り替える。`--new-window` を書けば今までどおり窓が開く。
///
/// 繋がらなければ `false` を返す。呼ぶ側はそのまま窓を開けばいい
/// （**開かないより、窓が開くほうがまし**）。
pub fn open_tab_here(session: &str, cwd: Option<String>, command: Option<Vec<String>>) -> bool {
    let Ok((mut client, _)) = attach(session) else {
        return false;
    };
    if client.send(&ClientMsg::NewTab { cwd, command }).is_err() {
        return false;
    }
    // 送り終える前に切ると取りこぼす
    std::thread::sleep(Duration::from_millis(200));
    let _ = client.send(&ClientMsg::Detach);
    true
}

/// 見えているペイン全部へ同じ文を投げる。
///
/// **同じ問いを別のエージェントへ同時に投げる**ための口。返事の速さも
/// 中身も違うので、揃うのを待ってから見比べる（`--wait` ＋ `--compare`）。
pub fn broadcast(session: &str, text: &str, and_wait: bool) -> Result<bool> {
    let (mut client, info) = attach(session)?;
    let targets: Vec<u32> = visible_panes(&info);
    if targets.is_empty() {
        bail!("送り先のペインがありません");
    }
    let bytes = format!("{}\r", text.replace(r"\n", "\r").replace(r"\e", "\x1b"));
    // 投げた先は全部「動いている」にしておく。**投げる前から
    // blocked だったペインを見て「もう返ってきた」と誤らないため。**
    for id in &targets {
        client.send(&ClientMsg::SetAgentState {
            pane: Some(*id),
            state: AgentState::Working,
            cost: None,
            agent: None,
        })?;
    }
    client.send(&ClientMsg::Broadcast {
        panes: targets.clone(),
        data: tsg_mux::encode_bytes(bytes.as_bytes()),
    })?;
    if !and_wait {
        std::thread::sleep(Duration::from_millis(300));
        let _ = client.send(&ClientMsg::Detach);
        eprintln!("{} 個のペインへ投げました", targets.len());
        return Ok(true);
    }
    eprintln!(
        "{} 個のペインへ投げました。返事を待っています…",
        targets.len()
    );
    loop {
        if let Some(ServerMsg::Layout(i)) = client.recv_timeout(Duration::from_millis(500)) {
            let done = targets.iter().all(|id| {
                i.panes
                    .iter()
                    .find(|p| p.id == *id)
                    .and_then(|p| p.agent)
                    .is_some_and(AgentState::wants_you)
            });
            if done {
                eprintln!("全部そろいました");
                return Ok(true);
            }
        }
    }
}

/// 各ペインに見えているものを 1 枚に並べる。
///
/// **同じ問いを投げたあと、返事を見比べるためのもの。** ペインを行き来して
/// 目で突き合わせる代わりに、1 本のテキストにして端から読む。
/// 出す先は標準出力なので、`tsg --compare > out.md` でも `| less` でも通る。
pub fn compare(session: &str) -> Result<()> {
    let (client, info) = attach(session)?;
    let targets = visible_panes(&info);
    if targets.is_empty() {
        bail!("見比べるペインがありません");
    }
    let mut out = std::io::stdout().lock();
    for id in targets {
        let title = info
            .panes
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.title.clone())
            .unwrap_or_default();
        let state = info
            .panes
            .iter()
            .find(|p| p.id == id)
            .and_then(|p| p.agent)
            .map_or("-".to_string(), |a| a.name().to_string());
        writeln!(out, "## ペイン {id}  {title}  [{state}]")?;
        writeln!(out)?;
        let found = client.wait_for(
            TIMEOUT,
            |m| matches!(m, ServerMsg::Snapshot { pane, .. } if *pane == id),
        );
        match found {
            Some(ServerMsg::Snapshot { lines, alt, .. }) => {
                let lines = if alt.is_empty() { lines } else { alt };
                for line in lines {
                    writeln!(out, "{}", strip_ansi(&line))?;
                }
            }
            _ => writeln!(out, "（中身が返りません）")?,
        }
        writeln!(out)?;
    }
    Ok(())
}

/// 画面の側のコマンドを外から実行する。
///
/// **窓の中でしか起きないこと**（検索・ラベル・畳み・配色）を台本から
/// 動かす。知らない id は先に弾く — 送っても何も起きないより、
/// その場で「そんな id は無い」と言うほうがいい。
pub fn run_command(session: &str, id: &str, arg: Option<String>) -> Result<()> {
    // `ext.` は外から登録された語彙。**こちらは中身を知らない**ので、
    // 実在するかどうかはサーバに判断させる（知らない id なら error が返る）。
    if !id.starts_with("ext.") && !tsg_modal::REGISTRY.iter().any(|s| s.id == id) {
        bail!("コマンド '{id}' を知りません（一覧は tsg --commands）");
    }
    let (mut client, _) = attach(session)?;
    client.send(&ClientMsg::RunCommand {
        id: id.to_string(),
        arg,
    })?;
    std::thread::sleep(Duration::from_millis(200));
    let _ = client.send(&ClientMsg::Detach);
    Ok(())
}

/// コマンドの id と題名。`--run` に渡せるものの一覧。
pub fn commands() -> Result<()> {
    let mut out = std::io::stdout().lock();
    for spec in tsg_modal::REGISTRY {
        writeln!(out, "{}	{}	{}", spec.id, spec.keys.join(" "), spec.title)?;
    }
    Ok(())
}

/// いまのタブで見えているペイン。
fn visible_panes(info: &SessionInfo) -> Vec<u32> {
    info.tabs
        .iter()
        .find(|t| t.id == info.active_tab)
        .map(|t| t.layout.panes())
        .unwrap_or_default()
}

/// 走っている窓でファイルを開く。`render` を付けると読む形で。
///
/// **端末から `tsg --open README.md` と打てる**ことに意味がある。
/// エディタを別に開かずに、いま見ている窓の中で読める。
pub fn open(session: &str, path: &str, render: bool) -> Result<()> {
    let full = std::fs::canonicalize(path)
        .unwrap_or_else(|_| std::path::PathBuf::from(path))
        .display()
        .to_string();
    // Windows の `\?\` 前置は、そのまま渡すと相手側で扱いに困る。
    let full = full.strip_prefix(r"\\?\").unwrap_or(&full).to_string();
    let (mut client, info) = attach(session)?;
    let pane = active_pane(&info);
    client.send(&ClientMsg::OpenFile { pane, path: full })?;
    if render {
        client.send(&ClientMsg::SetPreview {
            pane: Some(pane),
            on: Some(true),
        })?;
    }
    std::thread::sleep(Duration::from_millis(250));
    let _ = client.send(&ClientMsg::Detach);
    Ok(())
}

/// 読む形を切り替える。
pub fn render(session: &str, pane: Option<u32>) -> Result<()> {
    let (mut client, _) = attach(session)?;
    client.send(&ClientMsg::SetPreview { pane, on: None })?;
    std::thread::sleep(Duration::from_millis(200));
    let _ = client.send(&ClientMsg::Detach);
    Ok(())
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
