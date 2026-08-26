//! mux サーバ。セッション木の唯一の所有者。
//!
//! `arch.md` の不変条件 4「mux は常に別プロセス」。ローカル 1 ウィンドウでも
//! GUI はクライアントであり、ここが状態を持つ。永続性を後付けにしないための構造。
//!
//! スレッド構成（`arch.md` §5）:
//!   - listener      : 接続を受ける
//!   - client reader : 接続ごと。JSON Lines を解いて `Event` にする
//!   - pty reader    : ペインごと。生バイトを `Event` にする
//!   - state         : **1本だけ**。全状態の唯一の所有者
//!
//! 状態の所有者を 1 スレッドに固定するのは、モーダル操作と PTY 追記の競合を
//! ロックではなく順序で解くため。

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::mpsc::{self, Sender};
use std::thread;

use anyhow::{Context, Result};
use interprocess::TryClone;

use crate::endpoint::Endpoint;
use tsg_pty::{CommandBuilder, PtySession};
use tsg_term::Terminal;

use crate::protocol::*;

/// 再アタッチ時に送り返す最大行数。
const SNAPSHOT_MAX_LINES: usize = 5000;

/// 待っている相手が控える字の長さ。**頭から捨てる。**
///
/// 出っぱなしのコマンドを待つと、控えが際限なく伸びる。探している字は
/// たいてい直前に出るので、この長さで足りる。
const WAIT_TAIL_MAX: usize = 8 * 1024;

/// 生バイトから、人が読む字だけを取り出す。
///
/// **飾りを落とす。** `\x1b[32mPASS\x1b[0m` の中の `PASS` を探せないと、
/// 色を付けて出すテストランナーでは何も当たらない。
fn plain_text(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI / OSC などの並び。**終わりの字まで読み飛ばす。**
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if c.is_ascii_alphabetic() || c == '~' {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    // BEL か ST（`ESC \`）まで。
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            }
            continue;
        }
        if c == '\n' || c == '\t' || !c.is_control() {
            out.push(c);
        }
    }
    out
}

/// 拡張の記録に残す本数。**輪にして古いものから捨てる。**
///
/// 走らせっぱなしのサーバで静かに太らないように上限を持つ。調べたいのは
/// たいてい「さっき何が起きたか」なので、この長さで足りる。
const EXT_LOG_MAX: usize = 200;

/// いまの unix 秒。**形にするのは出す側**（サーバは時計を持つだけ）。
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

enum Event {
    ClientConnected {
        id: u64,
        writer: Box<dyn Write + Send>,
    },
    ClientMsg {
        id: u64,
        msg: ClientMsg,
    },
    ClientGone {
        id: u64,
    },
    PtyOutput {
        pane: u32,
        data: Vec<u8>,
    },
    PtyExit {
        pane: u32,
    },
    /// 時計。**外で書き換えられたファイルに気づく**ためだけにある。
    ///
    /// 監視の仕掛けを入れる手もあるが、OS ごとに癖があり、
    /// 編集中のファイル 1 つ 2 つを見るためには重い。1 秒に 1 回
    /// 更新時刻を見れば足りる。
    Tick,
    Stop,
}

struct Pane {
    id: u32,
    pty: PtySession,
    writer: Box<dyn Write + Send>,
    term: Terminal,
    cols: u16,
    rows: u16,
    title: String,
    alive: bool,
    /// エディタとして開いているファイル。**ここに置くから閉じても消えない。**
    file: Option<ServerFile>,
    /// エージェントが名乗った状態。**サーバが持つ**ので、窓を閉じても
    /// 開き直せば「どれが返事待ちか」が残っている。
    agent: Option<AgentState>,
    /// Markdown を読む形で見せているか。表示の状態だが、**開き直しても
    /// 戻らない**ようにサーバが預かる（開いているファイルと同じ扱い）。
    preview: bool,
    cost: Option<String>,
    /// 起こしたときの引数。**再起動をまたいで組み直す**ときに使う
    /// （`restore.rs`）。画面の中身は控えないが、これは控える。
    command: Option<Vec<String>>,
    /// 名乗ったエージェントの名前（`claude` / `codex`）。
    ///
    /// シェルの中で `claude` と打たれた場合、ペインのプログラムはシェルなので、
    /// **フックが名乗ってくれた名前だけが手がかり**になる。
    agent_kind: Option<String>,
    /// 最初のプロンプトが出たら打ち込む文字列（改行は付けない）。
    ///
    /// 組み直したペインに「続きから」を**置くだけ**。走らせるのは人が決める。
    /// 頼まれていないコマンドを勝手に実行しない。
    pending_type: Option<String>,
    /// 起こしたときの場所。OSC 7 が来ていないときの控え。
    ///
    /// **シェル統合を入れていない人でも、開いた場所に戻ってきてほしい。**
    /// OSC 7 だけを頼りにすると、入れていない環境では毎回ホームで開く。
    spawn_cwd: Option<String>,
}

/// サーバが預かるファイル。
///
/// 編集そのものはクライアント（モーダルエンジンを持つ側）が行い、
/// ここは**結果を預かるだけ**。保存はサーバが書く。
struct ServerFile {
    /// 保存先。`>` の結果はまだ決まっていない。
    path: Option<std::path::PathBuf>,
    /// 表示用の名前。
    title: String,
    text: String,
    dirty: bool,
    /// 最後に読んだ / 書いたときのディスク側の状態（更新時刻と大きさ）。
    ///
    /// **エージェントは開いている裏でファイルを書き換える。** 気づかないと、
    /// 古い中身を見ながら直して、保存した瞬間に相手の仕事を消す。
    stamp: Option<(std::time::SystemTime, u64)>,
}

/// 誰が、何を、いつまで待っているか。
struct Waiting {
    client: u64,
    pane: Option<u32>,
    matcher: Match,
    /// 待ち始めてから出てきた字（末尾だけ）。
    ///
    /// **行番号では追えない。** 端末の「行」は増えるとは限らず、シェルは
    /// もう在る行の上に書く。ドキュメントの長さを見張ると、画面の中で
    /// 書き換わっただけの出力を丸ごと取りこぼす（実際に踏んだ）。
    ///
    /// **待ち始めた時点より前は入れない。** 前から出ていた字ですぐ当たる
    /// なら、待った意味が無い。長さは `WAIT_TAIL_MAX` で頭から捨てる
    /// （出っぱなしのコマンドを待つと際限なく伸びるので）。
    tail: String,
    deadline: Option<std::time::Instant>,
}

struct State {
    session: String,
    /// 前回の形から組み直すか。`--no-restore` で切る。
    restore: bool,
    /// 言語サーバの世話。**開いているファイルと同じところに置く**
    /// ので、窓を閉じて開き直しても診断が消えない。
    lsp: crate::lsp::Lsp,
    panes: BTreeMap<u32, Pane>,
    tabs: Vec<TabInfo>,
    active_tab: u32,
    clients: BTreeMap<u64, Box<dyn Write + Send>>,
    /// 接続ごとの購読。**名乗ったものだけ**を配る（`ClientMsg::Subscribe`）。
    subs: BTreeMap<u64, BTreeSet<String>>,
    /// 待っている相手。**当たったら 1 通返して外す。**
    waits: Vec<Waiting>,
    /// 拡張が名乗った名前。記録を読める形にするためだけに持つ。
    ext_names: BTreeMap<u64, String>,
    /// 拡張が何をしたかの記録。**輪にして古いものから捨てる。**
    ///
    /// 際限なく溜めると、走らせっぱなしのサーバで静かに太る。
    ext_log: std::collections::VecDeque<ExtLogEntry>,
    /// 拡張が開いたペイン。`id` -> (誰のものか, ペイン番号)。
    ///
    /// **接続が切れても、ペインは閉じない。** 語彙（`ext`）は「押せるのに
    /// 何も起きない」が最悪なので消すが、ペインの中身は**読めるもの**で、
    /// 読んでいる途中に消えるほうが困る。切れたら id だけ手放す。
    ext_panes: BTreeMap<String, (u64, u32)>,
    /// 外から足された語彙。値は「誰が登録したか」と中身。
    ///
    /// 登録者を覚えるのは、その接続が切れたときに一緒に消すため。
    /// 押しても何も起きない項目がメニューに残り続けるのが一番悪い。
    ext: BTreeMap<String, (u64, ExtCommand)>,
    next_pane: u32,
    next_tab: u32,
    tx: Sender<Event>,
    cols: u16,
    rows: u16,
    /// 次に起こすペインの作業ディレクトリ・起動コマンド。
    spawn_cwd: Option<String>,
    spawn_command: Option<Vec<String>>,
}

impl State {
    fn new(session: String, tx: Sender<Event>) -> Self {
        Self {
            session,
            panes: BTreeMap::new(),
            tabs: Vec::new(),
            active_tab: 0,
            clients: BTreeMap::new(),
            subs: BTreeMap::new(),
            ext: BTreeMap::new(),
            waits: Vec::new(),
            ext_names: BTreeMap::new(),
            ext_log: std::collections::VecDeque::new(),
            ext_panes: BTreeMap::new(),
            next_pane: 1,
            next_tab: 1,
            tx,
            cols: 80,
            rows: 24,
            spawn_cwd: None,
            spawn_command: None,
            restore: true,
            lsp: crate::lsp::Lsp::default(),
        }
    }

    fn info(&self) -> SessionInfo {
        SessionInfo {
            name: self.session.clone(),
            tabs: self.tabs.clone(),
            active_tab: self.active_tab,
            panes: self
                .panes
                .values()
                .map(|p| PaneInfo {
                    id: p.id,
                    title: p.title.clone(),
                    cols: p.cols,
                    rows: p.rows,
                    alive: p.alive,
                    agent: p.agent,
                    preview: p.preview,
                    cost: p.cost.clone(),
                })
                .collect(),
        }
    }

    fn spawn_pane(&mut self, cols: u16, rows: u16) -> Result<u32> {
        let (cwd, command) = (self.spawn_cwd.clone(), self.spawn_command.clone());
        self.spawn_pane_in(cols, rows, cwd, command)
    }

    /// ペインを1つ起こす。
    ///
    /// `cwd` は「どこで開くか」。指定が無ければホーム。ファイラの
    /// 「ここでターミナルを開く」やペイン分割が効くかどうかがここで決まる。
    fn spawn_pane_in(
        &mut self,
        cols: u16,
        rows: u16,
        cwd: Option<String>,
        command: Option<Vec<String>>,
    ) -> Result<u32> {
        let id = self.next_pane;
        self.next_pane += 1;

        let program = command
            .as_ref()
            .and_then(|c| c.first().cloned())
            .unwrap_or_else(default_shell);
        let mut cmd = CommandBuilder::new(&program);
        if let Some(args) = command.as_ref() {
            for a in &args[1..] {
                cmd.arg(a);
            }
        }
        cmd.env("TERM", "xterm-256color");
        // **これが無いと多くのアプリが 256 色へ落ちる。** 解析器は真色に
        // 対応しているので、名乗るだけで見た目が変わる。
        cmd.env("COLORTERM", "truecolor");
        // 中で走るものに「ここは tsumugi の中だ」と教える。
        //
        // **これが無いと、中で `tsg` と打ったときに窓がもう 1 枚開く。**
        // 中に居ると分かれば、新しい窓ではなくこのセッションのタブを開ける。
        cmd.env("TSUMUGI_SESSION", &self.session);
        cmd.env("TSUMUGI_PANE", id.to_string());
        let spawn_cwd = cwd.filter(|c| std::path::Path::new(c).is_dir());
        match &spawn_cwd {
            Some(dir) => cmd.cwd(dir),
            None => {
                if let Some(home) = home_dir() {
                    cmd.cwd(home);
                }
            }
        }

        let pty = PtySession::spawn(cmd, tsg_pty::size(cols, rows))
            .with_context(|| format!("ペイン {id} のシェル起動に失敗: {program}"))?;
        let mut reader = pty.reader()?;
        let writer = pty.writer()?;

        let tx = self.tx.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 16384];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx
                            .send(Event::PtyOutput {
                                pane: id,
                                data: buf[..n].to_vec(),
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            }
            let _ = tx.send(Event::PtyExit { pane: id });
        });

        let opened_cwd = spawn_cwd.clone();
        self.panes.insert(
            id,
            Pane {
                id,
                pty,
                writer,
                term: Terminal::new(cols as usize, rows as usize, tsg_term::ambiguous()),
                cols,
                rows,
                title: program,
                alive: true,
                file: None,
                agent: None,
                preview: false,
                cost: None,
                command: command.clone(),
                agent_kind: None,
                pending_type: None,
                spawn_cwd,
            },
        );
        self.emit(PluginEvent::PaneOpened {
            pane: id,
            cwd: opened_cwd,
        });
        Ok(id)
    }

    /// 拡張のペインへ中身を置いて、窓へ配る。
    ///
    /// `dirty` は偽。**生成された眺めであって、直しかけの原稿ではない**ので、
    /// `*` を付けて閉じるときに引き止めるのは違う。
    fn write_ext_pane(&mut self, pane: u32, title: &str, text: &str) {
        if let Some(p) = self.panes.get_mut(&pane) {
            p.file = Some(ServerFile {
                path: None,
                title: title.to_string(),
                text: text.to_string(),
                dirty: false,
                stamp: None,
            });
        }
        self.broadcast(&ServerMsg::FileState {
            pane,
            path: None,
            title: title.to_string(),
            text: text.to_string(),
            dirty: false,
        });
    }

    /// 数えた時点より後に終わったコマンドを、出来事の形にして返す。
    ///
    /// 借用を分けるために一度 `Vec` に落としてから配る（`emit` は `&mut self`）。
    fn finished_since(&self, pane: u32, before: usize) -> Vec<PluginEvent> {
        let Some(p) = self.panes.get(&pane) else {
            return Vec::new();
        };
        let blocks = p.term.state.marks.blocks();
        let done: Vec<&tsg_term::CommandBlock> =
            blocks.iter().filter(|b| b.exit_code.is_some()).collect();
        if done.len() <= before {
            return Vec::new();
        }
        done[before..]
            .iter()
            .map(|b| {
                let command = b
                    .command_line
                    .and_then(|line| p.term.state.grid.document_line(line))
                    .map(tsg_term::Line::text)
                    .map(|text| {
                        // プロンプト記号を落として、打たれた部分だけにする。
                        let col = b.command_col;
                        text.chars().skip(col).collect::<String>().trim().to_string()
                    })
                    .unwrap_or_default();
                PluginEvent::CommandEnd {
                    pane,
                    exit_code: b.exit_code,
                    command,
                    output_start: b.output_start,
                    output_end: b.output_end,
                }
            })
            .collect()
    }

    /// 形が変わった。控えを置き直す。
    ///
    /// **エージェントの状態が変わっただけでは書かない。** それは形ではないし、
    /// 1 分に何度も来る。
    fn shape_changed(&mut self) {
        self.save_for_restore();
    }

    /// 組み直したペインへ「続きから」を置く。**押すのは人。**
    ///
    /// プロンプトが出てから置く。出る前に書くと、シェルが起き上がる途中の
    /// 入力として捨てられる。プロンプトの位置は OSC 133 で分かるので、
    /// **勘で待たずに済む**（シェル統合が入っていなければ置かない。
    /// 見えない相手へ当て推量で打ち込むより、何もしないほうがいい）。
    fn type_pending(&mut self, pane: u32) {
        let Some(p) = self.panes.get_mut(&pane) else {
            return;
        };
        if p.pending_type.is_none() || p.term.state.marks.blocks().is_empty() {
            return;
        }
        let Some(line) = p.pending_type.take() else {
            return;
        };
        let _ = p.writer.write_all(line.as_bytes());
        let _ = p.writer.flush();
    }

    /// 言語サーバから届いたものを配る。
    ///
    /// **待たない。** 溜まっているぶんだけ拾って、無ければ何もしない。
    fn poll_lsp(&mut self) {
        for (pane, msg, what) in self.lsp.poll() {
            let Some(pane) = pane else {
                continue; // どのペインのものか分からないものは捨てる
            };
            match msg {
                tsg_lsp::Incoming::Diagnostics { items, .. } => {
                    self.broadcast(&ServerMsg::Diagnostics { pane, items });
                }
                tsg_lsp::Incoming::Answer { result, .. } => match what {
                    Some(crate::lsp::Pending::Definition { .. }) => {
                        match tsg_lsp::parse_definition(&result) {
                            Some(at) => self.broadcast(&ServerMsg::Jump {
                                pane,
                                path: at.path,
                                line: at.line,
                                col: at.col,
                            }),
                            None => self.broadcast(&ServerMsg::Error {
                                message: "定義が見つかりません".into(),
                            }),
                        }
                    }
                    Some(crate::lsp::Pending::Completion { .. }) => {
                        // **数は絞る。** 何百も返ってくることがある。
                        let items = tsg_lsp::parse_completions(&result, 50);
                        self.broadcast(&ServerMsg::Completions { pane, items });
                    }
                    Some(crate::lsp::Pending::Hover { .. }) => {
                        match tsg_lsp::parse_hover(&result) {
                            Some(text) => self.broadcast(&ServerMsg::Hover { pane, text }),
                            // 言語サーバは「そこには何も無い」を空で返す。
                            // 黙って終わると、押した人には壊れたように見える。
                            None => self.broadcast(&ServerMsg::Error {
                                message: "ここには説明がありません".into(),
                            }),
                        }
                    }
                    Some(crate::lsp::Pending::References { .. }) => {
                        let items = tsg_lsp::parse_locations(&result);
                        if items.is_empty() {
                            self.broadcast(&ServerMsg::Error {
                                message: "使われている場所が見つかりません".into(),
                            });
                        } else {
                            self.broadcast(&ServerMsg::Locations { pane, items });
                        }
                    }
                    Some(crate::lsp::Pending::Rename { from, .. }) => {
                        let all = tsg_lsp::parse_rename(&result);
                        let here = all
                            .iter()
                            .find(|(path, _)| crate::lsp::same_path(path, &from))
                            .map(|(_, e)| e.clone())
                            .unwrap_or_default();
                        let others = all.len() - usize::from(!here.is_empty());
                        if here.is_empty() && others == 0 {
                            self.broadcast(&ServerMsg::Error {
                                message: "この名前は変えられません".into(),
                            });
                        } else {
                            self.broadcast(&ServerMsg::Edits {
                                pane,
                                edits: here,
                                others,
                            });
                        }
                    }
                    None => {}
                },
            }
        }
    }

    /// 外で書き換えられたファイルを読み直す。
    ///
    /// **エージェントは開いている裏でファイルを書き換える。** 気づかないと、
    /// 古い中身を見ながら直して、保存した瞬間に相手の仕事を消す。
    ///
    /// 直しかけ（`dirty`）のものは読み直さない。**打ったものを黙って
    /// 捨てない。** 代わりに「外で変わった」と知らせて、どうするかは人が決める。
    fn reload_changed_files(&mut self) {
        let mut reloaded: Vec<(u32, String, String, String)> = Vec::new();
        let mut clashed: Vec<u32> = Vec::new();
        for p in self.panes.values_mut() {
            let Some(f) = p.file.as_mut() else {
                continue;
            };
            let Some(path) = f.path.clone() else {
                continue;
            };
            let Some(now) = disk_stamp(&path) else {
                continue; // 消された。開いている中身はそのまま残す
            };
            if f.stamp == Some(now) {
                continue;
            }
            f.stamp = Some(now);
            if f.dirty {
                clashed.push(p.id);
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if text == f.text {
                continue; // 触られただけで中身は同じ
            }
            f.text = text.clone();
            reloaded.push((p.id, path.display().to_string(), f.title.clone(), text));
        }
        for (pane, path, title, text) in reloaded {
            self.broadcast(&ServerMsg::FileState {
                pane,
                path: Some(path),
                title,
                text,
                dirty: false,
            });
        }
        for pane in clashed {
            self.broadcast(&ServerMsg::Error {
                message: format!(
                    "ペイン {pane}: 開いているファイルが外で書き換えられました（直しかけなので読み直していません）"
                ),
            });
        }
    }

    /// 組み直すための控えを置く。**画面の中身は書かない**（`restore.rs`）。
    ///
    /// 形が変わるたびに置き直す。落ちるときに書こうとしても、電源が
    /// 落ちた場合は間に合わない。
    fn save_for_restore(&self) {
        let panes = self
            .panes
            .values()
            .filter(|p| p.alive)
            .map(|p| crate::restore::SavedPane {
                id: p.id,
                cwd: self.pane_cwd(p.id),
                command: p.command.clone(),
                agent: p.agent_kind.clone(),
            })
            .collect();
        crate::restore::save(
            &self.session,
            &crate::restore::Saved {
                version: crate::restore::VERSION,
                tabs: self.tabs.clone(),
                active_tab: self.active_tab,
                panes,
            },
        );
    }

    /// 控えから組み直す。**戻せなかったペインは飛ばす**（1 つも戻せなければ
    /// 素の 1 ペインで開く）。
    fn restore_from(&mut self, saved: crate::restore::Saved, cols: u16, rows: u16) -> bool {
        let mut made: std::collections::BTreeMap<u32, u32> = std::collections::BTreeMap::new();
        for p in &saved.panes {
            // 「続きから」で起こす。会話の記録を持っているのは相手なので、
            // こちらは開き方だけを知っていればいい。
            let command = p
                .command
                .as_ref()
                .map(|c| crate::restore::resume_command(c));
            let Ok(id) = self.spawn_pane_in(cols, rows, p.cwd.clone(), command) else {
                continue;
            };
            // シェルの中で起こされていた相手には、続きから開く 1 行を
            // **プロンプトに置くだけ**にする（押すのは人）。
            if p.command.is_none()
                && let Some(line) = p.agent.as_deref().and_then(crate::restore::resume_line)
                && let Some(pane) = self.panes.get_mut(&id)
            {
                pane.agent_kind = p.agent.clone();
                pane.pending_type = Some(line);
            }
            made.insert(p.id, id);
        }
        if made.is_empty() {
            return false;
        }
        // 木の中の古い id を新しい id へ置き換える。戻せなかったものは畳む。
        self.tabs = saved
            .tabs
            .into_iter()
            .filter_map(|t| {
                let layout = remap_layout(&t.layout, &made)?;
                Some(TabInfo {
                    id: t.id,
                    active_pane: made
                        .get(&t.active_pane)
                        .copied()
                        .unwrap_or_else(|| layout.panes().first().copied().unwrap_or_default()),
                    layout,
                    zoom: None,
                    name: t.name.clone(),
                })
            })
            .collect();
        if self.tabs.is_empty() {
            return false;
        }
        self.active_tab = if self.tabs.iter().any(|t| t.id == saved.active_tab) {
            saved.active_tab
        } else {
            self.tabs[0].id
        };
        self.next_tab = self.tabs.iter().map(|t| t.id).max().unwrap_or(0) + 1;
        true
    }

    /// 今のタブでアクティブなペイン。
    fn active_pane(&self) -> Option<u32> {
        self.tabs
            .iter()
            .find(|t| t.id == self.active_tab)
            .map(|t| t.active_pane)
    }

    /// そのペインが今いるディレクトリ（OSC 7 で受け取ったもの）。
    ///
    /// 分割や新しいタブは**今いる場所**で開くのが端末の慣例。
    /// 追加の仕掛けは要らず、すでに解析済みの OSC 7 をそのまま使う。
    fn pane_cwd(&self, pane: u32) -> Option<String> {
        let p = self.panes.get(&pane)?;
        let Some(url) = p.term.state.cwd.as_deref() else {
            // OSC 7 が来ていない（シェル統合を入れていない）。
            // **開いた場所を覚えているので、そこを答える。**
            return p.spawn_cwd.clone();
        };
        parse_file_url(url)
    }

    fn new_tab(&mut self, cols: u16, rows: u16) -> Result<u32> {
        let pane = self.spawn_pane(cols, rows)?;
        let id = self.next_tab;
        self.next_tab += 1;
        self.tabs.push(TabInfo {
            id,
            layout: Layout::leaf(pane),
            active_pane: pane,
            zoom: None,
            name: None,
        });
        self.active_tab = id;
        Ok(id)
    }

    fn tab_of(&mut self, pane: u32) -> Option<&mut TabInfo> {
        self.tabs
            .iter_mut()
            .find(|t| t.layout.panes().contains(&pane))
    }

    fn send_to(&mut self, id: u64, msg: &ServerMsg) {
        let Ok(line) = serde_json::to_string(msg) else {
            return;
        };
        if let Some(w) = self.clients.get_mut(&id)
            && (writeln!(w, "{line}").is_err() || w.flush().is_err())
        {
            self.clients.remove(&id);
        }
    }

    /// 待っている相手に見比べさせる。**当たったら 1 通返して外す。**
    ///
    /// `text` はその一回で新しく出た字。`ended` / `agent` はその一回で
    /// 分かったこと（`protocol::MatchInput`）。
    fn feed_waits(&mut self, pane: u32, ended: Option<Option<i32>>) {
        self.feed_waits_with(pane, ended, None, None);
    }

    fn feed_waits_with(
        &mut self,
        pane: u32,
        ended: Option<Option<i32>>,
        agent: Option<AgentState>,
        event: Option<&str>,
    ) {
        if self.waits.is_empty() {
            return;
        }
        let mut done: Vec<(u64, u32)> = Vec::new();
        self.waits.retain(|w| {
            if w.pane.is_some_and(|p| p != pane) {
                return true;
            }
            let input = MatchInput {
                text: &w.tail,
                ended,
                agent,
                event,
            };
            if w.matcher.hit(&input) {
                done.push((w.client, pane));
                return false;
            }
            true
        });
        for (client, pane) in done {
            self.send_to(
                client,
                &ServerMsg::Waited {
                    matched: true,
                    pane: Some(pane),
                },
            );
        }
    }

    /// 時間切れを片付ける。**黙って待たせ続けない。**
    fn expire_waits(&mut self) {
        if self.waits.is_empty() {
            return;
        }
        let now = std::time::Instant::now();
        let mut over: Vec<u64> = Vec::new();
        self.waits.retain(|w| match w.deadline {
            Some(d) if d <= now => {
                over.push(w.client);
                false
            }
            _ => true,
        });
        for client in over {
            self.send_to(
                client,
                &ServerMsg::Waited {
                    matched: false,
                    pane: None,
                },
            );
        }
    }

    /// 出てきたぶんを、待っている相手それぞれの控えへ足す。
    ///
    /// 控えを 1 本にまとめないのは、待ち始めた時刻が相手ごとに違うため。
    fn push_tails(&mut self, pane: u32, chunk: &str) {
        for w in &mut self.waits {
            if w.pane.is_some_and(|x| x != pane) {
                continue;
            }
            w.tail.push_str(chunk);
            // 頭から捨てる。**字の境目で切る**（マルチバイトの途中で
            // 切ると落ちる）。
            if w.tail.len() > WAIT_TAIL_MAX {
                let cut = w.tail.len() - WAIT_TAIL_MAX;
                let at = (cut..w.tail.len())
                    .find(|i| w.tail.is_char_boundary(*i))
                    .unwrap_or(w.tail.len());
                w.tail.drain(..at);
            }
        }
    }

    /// git に訊く場所。**ペインが居る場所**で訊く（作業ツリーごとに答えが違う）。
    ///
    /// ペインの居場所が分からないとき（シェル統合が入っていない＝OSC 7 が
    /// 来ない、かつ開いた場所も覚えていない）は、打った側が持たせてきた場所へ
    /// 倒す。**出せないことを事故にしない。**
    fn git_dir(&self, pane: Option<u32>, fallback: Option<String>) -> Option<String> {
        pane.or_else(|| self.active_pane())
            .and_then(|p| self.pane_cwd(p))
            .or(fallback)
    }

    /// 実際の木を、番号を落とした形にする。
    ///
    /// 葉には**そのペインの場所と起動したもの**を書く。番号を書き出しても、
    /// 次に当てるときにはもう無い。
    fn spec_of(&self, layout: &Layout) -> LayoutSpec {
        match layout {
            Layout::Leaf { pane } => LayoutSpec::Leaf {
                cwd: self.pane_cwd(*pane),
                command: self.panes.get(pane).and_then(|p| p.command.clone()),
            },
            Layout::Split {
                dir,
                children,
                weights,
            } => LayoutSpec::Split {
                dir: *dir,
                children: children.iter().map(|c| self.spec_of(c)).collect(),
                weights: weights_for(children, weights),
            },
        }
    }

    /// 拡張がしたことを 1 行残す。
    fn note(&mut self, id: u64, what: impl Into<String>, refused: bool) {
        let who = self
            .ext_names
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("#{id}"));
        self.ext_log.push_back(ExtLogEntry {
            at: now_secs(),
            who,
            what: what.into(),
            refused,
        });
        while self.ext_log.len() > EXT_LOG_MAX {
            self.ext_log.pop_front();
        }
    }

    /// 断る。**送るのと残すのを 1 か所にする。**
    ///
    /// 別々にすると、断り方を増やすたびにどちらかを書き忘れる。書き忘れた
    /// ぶんは「繋がっているのに何も起きない」として人の前に出る。
    fn refuse(&mut self, id: u64, message: String) {
        self.note(id, message.clone(), true);
        self.send_to(id, &ServerMsg::Error { message });
    }

    /// 出来事を、それを名乗った接続だけへ配る。
    ///
    /// **窓は購読しない。** 窓が要るのは形と生バイトで、意味の粒は
    /// 外の拡張のためのもの。両方へ配ると、窓の側にも「無視する通知」が増える。
    fn emit(&mut self, event: PluginEvent) {
        let name = event.name();
        // **待ちと購読は同じ出来事を見る。** 片方だけ増える形にしない。
        if !self.waits.is_empty() {
            let pane = match &event {
                PluginEvent::CommandEnd { pane, .. }
                | PluginEvent::PaneOpened { pane, .. }
                | PluginEvent::PaneClosed { pane }
                | PluginEvent::AgentState { pane, .. }
                | PluginEvent::Cwd { pane, .. } => Some(*pane),
                PluginEvent::Command { pane, .. } => *pane,
            };
            if let Some(pane) = pane {
                self.feed_waits_with(pane, None, None, Some(name));
            }
        }
        let want: Vec<u64> = self
            .subs
            .iter()
            .filter(|(_, names)| names.iter().any(|n| n == name))
            .map(|(id, _)| *id)
            .collect();
        if want.is_empty() {
            return;
        }
        let msg = ServerMsg::Event { event };
        for id in want {
            self.send_to(id, &msg);
        }
    }

    /// 外から足された語彙の全体を配り直す。
    fn broadcast_ext(&mut self) {
        let commands: Vec<ExtCommand> = self.ext.values().map(|(_, c)| c.clone()).collect();
        self.broadcast(&ServerMsg::ExtCommands { commands });
    }

    fn broadcast(&mut self, msg: &ServerMsg) {
        let Ok(line) = serde_json::to_string(msg) else {
            return;
        };
        let mut dead = Vec::new();
        for (id, w) in &mut self.clients {
            if writeln!(w, "{line}").is_err() || w.flush().is_err() {
                dead.push(*id);
            }
        }
        for id in dead {
            self.clients.remove(&id);
        }
    }

    fn file_state(&self, pane: u32) -> Option<ServerMsg> {
        let f = self.panes.get(&pane)?.file.as_ref()?;
        Some(ServerMsg::FileState {
            pane,
            path: f.path.as_ref().map(|p| p.display().to_string()),
            title: f.title.clone(),
            text: f.text.clone(),
            dirty: f.dirty,
        })
    }

    fn snapshot(&self, pane: u32) -> Option<ServerMsg> {
        let p = self.panes.get(&pane)?;
        let grid = &p.term.state.grid;

        // **primary だけを文書として送る。** alt screen の行を混ぜると、
        // 受けた側は alt に居ることを知らないまま 1 本の文書として復元し、
        // 全画面アプリの絵が履歴に焼き付く。
        //
        // 末尾の空行は送らない。送ると再アタッチのたびに画面が下へ押し出される。
        let mut end = grid.primary_len();
        while end > 0
            && grid
                .primary_line_ansi(end - 1)
                .is_some_and(|l| l.is_empty())
        {
            end -= 1;
        }
        let start = end.saturating_sub(SNAPSHOT_MAX_LINES);

        // alt に居るなら、primary のカーソルは「戻ったときに書き始める場所」。
        let (cursor_line, cursor_col) = grid.primary_cursor();

        Some(ServerMsg::Snapshot {
            pane,
            lines: (start..end)
                .filter_map(|i| grid.primary_line_ansi(i))
                .collect(),
            cursor_line: cursor_line.saturating_sub(start),
            cursor_col,
            alt: grid.alt_lines_ansi(),
            alt_cursor: (grid.cursor.row, grid.cursor.col),
        })
    }

    fn on_client(&mut self, id: u64, msg: ClientMsg) -> Result<bool> {
        match msg {
            ClientMsg::Attach {
                version,
                cols,
                rows,
                cwd,
                command,
                restore,
            } => {
                if version != PROTOCOL_VERSION {
                    self.send_to(
                        id,
                        &ServerMsg::Error {
                            message: format!(
                                "プロトコルの版が違います（サーバ {PROTOCOL_VERSION} / クライアント {version}）"
                            ),
                        },
                    );
                    return Ok(true);
                }
                // 0 は「大きさを持ち込まない」。窓を持たないクライアントが
                // 適当な既定値を名乗ると、その後に開くペインまでそれになる。
                if cols > 0 && rows > 0 {
                    self.cols = cols;
                    self.rows = rows;
                }
                if self.tabs.is_empty() {
                    let (c, r) = (self.cols, self.rows);
                    // 前回の形が残っていれば、そこから組み直す。
                    //
                    // **`-e` や `--cwd` を明示して開いたときは組み直さない。**
                    // 「これを開いてくれ」と言われているのに前回の形が出るのは、
                    // 頼んだことと違う。その判断は打った側にしかできないので、
                    // `restore` として渡ってくる（`command` はここでも見る。
                    // これは既定で入ることが無い）。
                    let restored = (restore && command.is_none() && self.restore)
                        .then(|| crate::restore::load(&self.session))
                        .flatten()
                        .is_some_and(|saved| self.restore_from(saved, c, r));
                    if !restored {
                        // 起動時の指定は最初のペインにだけ効く。
                        // 再アタッチで既にペインがあるなら、そこは触らない。
                        self.spawn_cwd = cwd;
                        self.spawn_command = command;
                        self.new_tab(c, r)?;
                        self.spawn_command = None;
                    }
                    self.save_for_restore();
                }
                let info = self.info();
                self.send_to(
                    id,
                    &ServerMsg::Attached {
                        version: PROTOCOL_VERSION,
                        session: info,
                    },
                );
                // 既存ペインの画面を復元して渡す。これが再アタッチの本体。
                let ids: Vec<u32> = self.panes.keys().copied().collect();
                for pane in ids {
                    if let Some(snap) = self.snapshot(pane) {
                        self.send_to(id, &snap);
                    }
                    // エディタとして開いていたペインは、そのまま開き直す
                    if let Some(msg) = self.file_state(pane) {
                        self.send_to(id, &msg);
                    }
                }
                // 先に繋がっていた拡張の語彙を、いま来た窓へ渡す。
                // **繋いだ順で語彙が変わってはいけない。**
                if !self.ext.is_empty() {
                    let commands: Vec<ExtCommand> =
                        self.ext.values().map(|(_, c)| c.clone()).collect();
                    self.send_to(id, &ServerMsg::ExtCommands { commands });
                }
            }

            ClientMsg::SetAgentState {
                pane,
                state,
                cost,
                agent,
            } => {
                // ペインの指定が無ければ、いま選ばれているところ。
                // hooks は自分がどのペインに居るか知らないので、これが既定。
                let target = pane.or_else(|| self.active_pane());
                let mut named = false;
                if let Some(p) = target.and_then(|id| self.panes.get_mut(&id)) {
                    let changed = p.agent != Some(state) || (cost.is_some() && p.cost != cost);
                    p.agent = Some(state);
                    if cost.is_some() {
                        p.cost = cost;
                    }
                    // 名乗った相手を覚える。**これが再起動のあとの
                    // 「続きから」になる。**
                    if agent.is_some() && p.agent_kind != agent {
                        p.agent_kind = agent;
                        named = true;
                    }
                    if changed {
                        let info = self.info();
                        self.broadcast(&ServerMsg::Layout(info));
                    }
                }
                if named {
                    self.save_for_restore();
                }
                if let Some(pane) = target {
                    self.feed_waits_with(pane, None, Some(state), Some("agent"));
                    self.emit(PluginEvent::AgentState {
                        pane,
                        state,
                        agent: self.panes.get(&pane).and_then(|p| p.agent_kind.clone()),
                    });
                }
            }

            ClientMsg::Broadcast { panes, data } => {
                let Some(bytes) = decode_bytes(&data) else {
                    return Ok(true);
                };
                // 宛先を書かなければ、いまのタブで見えているペイン全部。
                let targets: Vec<u32> = if panes.is_empty() {
                    self.tabs
                        .iter()
                        .find(|t| t.id == self.active_tab)
                        .map(|t| t.layout.panes())
                        .unwrap_or_default()
                } else {
                    panes
                };
                for id in targets {
                    if let Some(p) = self.panes.get_mut(&id) {
                        let _ = p.writer.write_all(&bytes);
                        let _ = p.writer.flush();
                    }
                }
            }

            ClientMsg::RunCommand { id: cmd, arg } => {
                // 外から足された語彙は、窓ではなく**登録した拡張**へ返す。
                // 窓へ配っても、窓はその id を知らないので何も起きない。
                if cmd.starts_with("ext.") {
                    if !self.ext.contains_key(&cmd) {
                        self.refuse(id, format!("{cmd} は登録されていません"));
                        return Ok(true);
                    }
                    let pane = self.active_pane();
                    self.emit(PluginEvent::Command { id: cmd, pane, arg });
                    return Ok(true);
                }
                // サーバは中身を知らない。**そのまま配るだけ。**
                self.broadcast(&ServerMsg::RunCommand { id: cmd, arg });
            }

            ClientMsg::ExtHello { name } => {
                let name = name.trim().to_string();
                if name.is_empty() {
                    self.refuse(id, "名前が空です".into());
                    return Ok(true);
                }
                self.ext_names.insert(id, name.clone());
                self.note(id, format!("{name} が繋がりました"), false);
            }

            ClientMsg::Notify { text, level } => {
                let text = text.trim().to_string();
                if text.is_empty() {
                    self.refuse(id, "知らせが空です".into());
                    return Ok(true);
                }
                self.note(id, format!("知らせ: {text}"), false);
                self.broadcast(&ServerMsg::Notify { text, level });
            }

            ClientMsg::WorktreeList { pane, cwd } => {
                let Some(dir) = self.git_dir(pane, cwd) else {
                    self.refuse(id, "git の中ではありません".into());
                    return Ok(true);
                };
                match git(&dir, &["worktree", "list", "--porcelain"]) {
                    Ok(out) => self.send_to(
                        id,
                        &ServerMsg::Worktrees {
                            items: parse_worktrees(&out),
                        },
                    ),
                    Err(e) => self.refuse(id, e),
                }
            }

            ClientMsg::WorktreeAdd {
                pane,
                cwd,
                path,
                branch,
            } => {
                let Some(dir) = self.git_dir(pane, cwd) else {
                    self.refuse(id, "git の中ではありません".into());
                    return Ok(true);
                };
                let mut args: Vec<&str> = vec!["worktree", "add"];
                if let Some(b) = branch.as_deref() {
                    args.push("-b");
                    args.push(b);
                }
                args.push(&path);
                match git(&dir, &args) {
                    // **作ったら開く。** 作っただけで場所を自分で打ち直すのは、
                    // 頼んだことの続きが人の仕事に戻っているだけ。
                    Ok(_) => return self.on_client(id, ClientMsg::WorktreeOpen { path }),
                    Err(e) => self.refuse(id, e),
                }
            }

            ClientMsg::WorktreeRemove {
                pane,
                cwd,
                path,
                force,
            } => {
                let Some(dir) = self.git_dir(pane, cwd) else {
                    self.refuse(id, "git の中ではありません".into());
                    return Ok(true);
                };
                let mut args: Vec<&str> = vec!["worktree", "remove"];
                if force {
                    args.push("--force");
                }
                args.push(&path);
                match git(&dir, &args) {
                    Ok(_) => self.note(id, format!("{path} を消しました"), false),
                    // 直しかけが残っていれば git が断る。**押し切らない。**
                    Err(e) => self.refuse(id, e),
                }
            }

            ClientMsg::WorktreeOpen { path } => {
                let (cols, rows) = (self.cols, self.rows);
                let saved = self.spawn_cwd.replace(path);
                let made = self.new_tab(cols, rows);
                self.spawn_cwd = saved;
                made?;
                let info = self.info();
                self.broadcast(&ServerMsg::Layout(info));
                self.shape_changed();
            }

            ClientMsg::LayoutExport { tab } => {
                let tab = tab.unwrap_or(self.active_tab);
                let Some(t) = self.tabs.iter().find(|t| t.id == tab) else {
                    self.refuse(id, format!("タブ {tab} はありません"));
                    return Ok(true);
                };
                let layout = t.layout.clone();
                let spec = self.spec_of(&layout);
                self.send_to(id, &ServerMsg::LayoutSpec { spec });
            }

            ClientMsg::LayoutApply { spec } => {
                let leaves = spec.leaf_list();
                if leaves.is_empty() {
                    self.refuse(id, "葉が 1 つもありません".into());
                    return Ok(true);
                }
                // 当てるときに居た場所を、場所を書いていない葉の既定にする。
                let here = self.active_pane().and_then(|p| self.pane_cwd(p));
                let (cols, rows) = (self.cols, self.rows);
                let mut made: Vec<u32> = Vec::with_capacity(leaves.len());
                for (cwd, command) in leaves {
                    match self.spawn_pane_in(cols, rows, cwd.or_else(|| here.clone()), command) {
                        Ok(p) => made.push(p),
                        Err(e) => {
                            // 途中で失敗したら、開いたぶんを片付けてから断る。
                            // **半分だけ開いた形を残さない。**
                            for p in made {
                                if let Some(mut v) = self.panes.remove(&p) {
                                    let _ = v.pty.kill();
                                }
                            }
                            self.refuse(id, format!("開けませんでした: {e:#}"));
                            return Ok(true);
                        }
                    }
                }
                let mut iter = made.iter().copied();
                let Some(layout) = spec.to_layout(&mut iter) else {
                    for p in made {
                        if let Some(mut v) = self.panes.remove(&p) {
                            let _ = v.pty.kill();
                        }
                    }
                    self.refuse(id, "形を組めません".into());
                    return Ok(true);
                };
                let active_pane = made[0];
                let tab_id = self.next_tab;
                self.next_tab += 1;
                self.tabs.push(TabInfo {
                    id: tab_id,
                    layout,
                    active_pane,
                    zoom: None,
                    name: None,
                });
                self.active_tab = tab_id;
                let info = self.info();
                self.broadcast(&ServerMsg::Layout(info));
                self.shape_changed();
            }

            ClientMsg::Wait {
                pane,
                matcher,
                timeout_ms,
            } => {
                // **待たせてから「読めませんでした」は最悪。** 先に組んでみる。
                if let Err(e) = matcher.check() {
                    self.refuse(id, e);
                    return Ok(true);
                }
                // いま見えているところから先だけを見る。
                let deadline = timeout_ms
                    .filter(|ms| *ms > 0)
                    .map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms));
                self.waits.push(Waiting {
                    client: id,
                    pane,
                    matcher,
                    tail: String::new(),
                    deadline,
                });
            }

            ClientMsg::ExtLog { limit } => {
                let n = limit.unwrap_or(50).min(EXT_LOG_MAX);
                let entries: Vec<ExtLogEntry> = self
                    .ext_log
                    .iter()
                    .rev()
                    .take(n)
                    .rev()
                    .cloned()
                    .collect();
                self.send_to(id, &ServerMsg::ExtLog { entries });
            }

            ClientMsg::Subscribe { events } => {
                let unknown: Vec<String> = events
                    .iter()
                    .filter(|e| !PluginEvent::NAMES.contains(&e.as_str()))
                    .cloned()
                    .collect();
                if !unknown.is_empty() {
                    // **黙って無視しない。** 名前を打ち間違えた拡張は、
                    // 何も届かないまま正しく繋がったつもりで待ち続ける。
                    self.refuse(
                        id,
                        format!(
                            "知らない出来事です: {}（あるのは {}）",
                            unknown.join(", "),
                            PluginEvent::NAMES.join(", ")
                        ),
                    );
                }
                let mut took: Vec<String> = Vec::new();
                let set = self.subs.entry(id).or_default();
                for e in events {
                    if PluginEvent::NAMES.contains(&e.as_str()) {
                        set.insert(e.clone());
                        took.push(e);
                    }
                }
                if !took.is_empty() {
                    self.note(id, format!("{} を購読", took.join(", ")), false);
                }
            }

            ClientMsg::Unsubscribe { events } => {
                if let Some(set) = self.subs.get_mut(&id) {
                    for e in &events {
                        set.remove(e);
                    }
                    if set.is_empty() {
                        self.subs.remove(&id);
                    }
                }
            }

            ClientMsg::RegisterCommand { command } => {
                if !ExtCommand::id_is_valid(&command.id) {
                    self.refuse(
                        id,
                        format!(
                            "id は `ext.` で始まる英数字にしてください: {}",
                            command.id
                        ),
                    );
                    return Ok(true);
                }
                if command.title.is_empty() || command.title_en.is_empty() {
                    self.refuse(id, "title と title_en の両方が要ります".into());
                    return Ok(true);
                }
                // 同じ id を別の接続が持っているなら奪わせない。
                if let Some((owner, _)) = self.ext.get(&command.id)
                    && *owner != id
                {
                    self.refuse(id, format!("{} は他の拡張が使っています", command.id));
                    return Ok(true);
                }
                self.note(id, format!("{} を足しました", command.id), false);
                self.ext.insert(command.id.clone(), (id, command));
                self.broadcast_ext();
            }

            ClientMsg::UnregisterCommand { id: cmd } => {
                if self.ext.get(&cmd).is_some_and(|(owner, _)| *owner == id) {
                    self.ext.remove(&cmd);
                    self.note(id, format!("{cmd} を外しました"), false);
                    self.broadcast_ext();
                }
            }

            ClientMsg::Hover { pane, line, col } => {
                self.lsp.hover(pane, line, col);
            }

            ClientMsg::References { pane, line, col } => {
                self.lsp.references(pane, line, col);
            }

            ClientMsg::Rename {
                pane,
                line,
                col,
                new_name,
            } => {
                if new_name.trim().is_empty() {
                    self.send_to(
                        id,
                        &ServerMsg::Error {
                            message: "新しい名前が空です".into(),
                        },
                    );
                    return Ok(true);
                }
                self.lsp.rename(pane, line, col, new_name.trim());
            }

            ClientMsg::ExtPaneOpen {
                id: name,
                near,
                dir,
                title,
                text,
            } => {
                if !ExtCommand::id_is_valid(&name) {
                    self.refuse(
                        id,
                        format!("id は `ext.` で始まる英数字にしてください: {name}"),
                    );
                    return Ok(true);
                }
                // すでに開いているなら、そこへ書く。**増やさない。**
                if let Some(pane) = live_ext_pane(&self.ext_panes, &self.panes, &name, id) {
                    self.write_ext_pane(pane, &title, &text);
                    self.send_to(id, &ServerMsg::ExtPane { id: name, pane });
                    return Ok(true);
                }
                self.ext_panes.remove(&name);
                let near = near.or_else(|| self.active_pane());
                let Some(near) = near else {
                    self.refuse(id, "開く場所がありません（ペインが 1 つもない）".into());
                    return Ok(true);
                };
                let (cols, rows) = (self.cols, self.rows);
                let here = self.pane_cwd(near);
                let new_pane = self.spawn_pane_in(cols, rows, here, None)?;
                self.write_ext_pane(new_pane, &title, &text);
                if let Some(tab) = self.tab_of(near) {
                    tab.layout.split(near, new_pane, dir.unwrap_or(Dir::Horizontal));
                    tab.active_pane = new_pane;
                }
                self.note(id, format!("{name} のペインを開きました"), false);
                self.ext_panes.insert(name.clone(), (id, new_pane));
                let info = self.info();
                self.broadcast(&ServerMsg::Layout(info));
                self.shape_changed();
                self.send_to(
                    id,
                    &ServerMsg::ExtPane {
                        id: name,
                        pane: new_pane,
                    },
                );
            }

            ClientMsg::ExtPaneWrite { id: name, text } => {
                let Some(pane) = live_ext_pane(&self.ext_panes, &self.panes, &name, id) else {
                    // **黙って捨てない。** 書いたつもりで何も出ない、が一番困る。
                    self.refuse(id, format!("{name} は開いていません"));
                    return Ok(true);
                };
                let title = self
                    .panes
                    .get(&pane)
                    .and_then(|p| p.file.as_ref())
                    .map_or_else(String::new, |f| f.title.clone());
                self.write_ext_pane(pane, &title, &text);
            }

            ClientMsg::ExtPaneClose { id: name } => {
                if let Some(pane) = live_ext_pane(&self.ext_panes, &self.panes, &name, id) {
                    self.ext_panes.remove(&name);
                    self.note(id, format!("{name} のペインを閉じました"), false);
                    // 閉じ方は 1 か所しかない。**同じ道を通す** — タブの片付けと
                    // 割り付けの畳み方をここへ書き写すと、必ず片方だけ直る日が来る。
                    return self.on_client(id, ClientMsg::ClosePane { pane });
                }
            }

            ClientMsg::GetBuffer { pane, start, end } => {
                let Some(p) = self.panes.get(&pane) else {
                    self.refuse(id, format!("ペイン {pane} はありません"));
                    return Ok(true);
                };
                // ファイルを開いていればその中身。**画面から読み直さない**
                // （開いている中身を持っているのはサーバ側だから）。
                let msg = if let Some(f) = p.file.as_ref() {
                    let all: Vec<String> = f.text.lines().map(str::to_string).collect();
                    let (from, to) = clip_range(all.len(), start, end);
                    ServerMsg::Buffer {
                        pane,
                        kind: "file".into(),
                        start: from,
                        lines: all[from..to].to_vec(),
                    }
                } else {
                    let total = p.term.state.grid.document_len();
                    let (from, to) = clip_range(total, start, end);
                    let lines = (from..to)
                        .map(|i| {
                            p.term
                                .state
                                .grid
                                .document_line(i)
                                .map(tsg_term::Line::text)
                                .unwrap_or_default()
                        })
                        .collect();
                    ServerMsg::Buffer {
                        pane,
                        kind: "term".into(),
                        start: from,
                        lines,
                    }
                };
                self.send_to(id, &msg);
            }

            ClientMsg::SetPreview { pane, on } => {
                let target = pane.or_else(|| self.active_pane());
                if let Some(p) = target.and_then(|id| self.panes.get_mut(&id)) {
                    let next = on.unwrap_or(!p.preview);
                    if p.preview != next {
                        p.preview = next;
                        let info = self.info();
                        self.broadcast(&ServerMsg::Layout(info));
                    }
                }
            }

            ClientMsg::Input { pane, data } => {
                if let (Some(bytes), Some(p)) = (decode_bytes(&data), self.panes.get_mut(&pane)) {
                    let _ = p.writer.write_all(&bytes);
                    let _ = p.writer.flush();
                }
            }

            ClientMsg::Resize { pane, cols, rows } => {
                let changed = self
                    .panes
                    .get(&pane)
                    .is_some_and(|p| p.cols != cols || p.rows != rows);
                if let Some(p) = self.panes.get_mut(&pane) {
                    p.cols = cols;
                    p.rows = rows;
                    let _ = p.pty.resize(tsg_pty::size(cols, rows));
                    p.term.resize(cols as usize, rows as usize);
                }
                // ここが ConPTY の桁数が変わった瞬間。同じ順路で全クライアントへ伝える。
                if changed {
                    self.broadcast(&ServerMsg::Resized { pane, cols, rows });
                }
            }

            ClientMsg::Split { pane, dir } => {
                let (cols, rows) = (self.cols, self.rows);
                let here = self.pane_cwd(pane);
                let new_pane = self.spawn_pane_in(cols, rows, here, None)?;
                if let Some(tab) = self.tab_of(pane) {
                    tab.layout.split(pane, new_pane, dir);
                    tab.active_pane = new_pane;
                }
                let info = self.info();
                self.broadcast(&ServerMsg::Layout(info));
                self.shape_changed();
            }

            ClientMsg::Definition { pane, line, col } => {
                if !self.lsp.definition(pane, line, col) {
                    self.send_to(
                        id,
                        &ServerMsg::Error {
                            message: "定義を引ける言語サーバがありません".into(),
                        },
                    );
                }
            }

            ClientMsg::Complete { pane, line, col } => {
                // 補完は**空振りしても黙っている**。打つたびに投げるので、
                // 出ないたびに知らせが出ると読めなくなる。
                let _ = self.lsp.complete(pane, line, col);
            }

            ClientMsg::RenameTab { tab, name } => {
                // 長すぎる名前はタブの並びを潰す。切り詰めて受ける
                // （断るより、入るところまで入れたほうが使える）。
                let name: String = name.trim().chars().take(32).collect();
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.name = (!name.is_empty()).then_some(name);
                    let info = self.info();
                    self.broadcast(&ServerMsg::Layout(info));
                    self.shape_changed();
                }
            }

            ClientMsg::ClosePane { pane } => {
                if let Some(mut p) = self.panes.remove(&pane) {
                    let _ = p.pty.kill();
                }
                if let Some(tab) = self.tab_of(pane) {
                    tab.layout.remove(pane);
                    if tab.active_pane == pane {
                        tab.active_pane = tab.layout.panes().first().copied().unwrap_or(0);
                    }
                }
                // **在るペインで数える。** `Layout::remove` は葉（1 枚だけの
                // タブ）を消せないので、`panes()` は閉じたはずの id を返し続ける。
                // それを信じると、中身の無いタブが残って閉じられなくなる。
                let alive: Vec<u32> = self.panes.keys().copied().collect();
                let was = self
                    .tabs
                    .iter()
                    .position(|t| t.id == self.active_tab)
                    .unwrap_or(0);
                self.tabs
                    .retain(|t| t.layout.panes().iter().any(|p| alive.contains(p)));
                // 見ていたタブが消えたら、その場所の隣へ移る。**居場所を残さない。**
                // 消えた id を指したままだと、タブ帯でどれも選ばれていない状態になり、
                // `Space n` / `Space p` の起点も無くなる。
                if !self.tabs.iter().any(|t| t.id == self.active_tab) {
                    let next = was.min(self.tabs.len().saturating_sub(1));
                    self.active_tab = self.tabs.get(next).map_or(0, |t| t.id);
                }
                let info = self.info();
                self.broadcast(&ServerMsg::Layout(info));
                self.shape_changed();
            }

            ClientMsg::ResizeSplit { pane, delta } => {
                // 分割比はサーバの木が持つ。クライアント側だけで持つと
                // 再アタッチで戻り、複数クライアントで食い違う。
                if let Some(tab) = self.tab_of(pane)
                    && tab.layout.resize(pane, delta)
                {
                    let info = self.info();
                    self.broadcast(&ServerMsg::Layout(info));
                    self.shape_changed();
                }
            }

            ClientMsg::OpenFile { pane, path } => {
                // 無いファイルは新規として開く。開けないほうが不便。
                let text = match std::fs::read_to_string(&path) {
                    Ok(t) => t,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(e) => {
                        self.send_to(
                            id,
                            &ServerMsg::Error {
                                message: format!("{path} を開けません: {e}"),
                            },
                        );
                        return Ok(true);
                    }
                };
                let title = std::path::Path::new(&path)
                    .file_name()
                    .map_or_else(|| path.clone(), |n| n.to_string_lossy().into_owned());
                if let Some(p) = self.panes.get_mut(&pane) {
                    p.file = Some(ServerFile {
                        path: Some(std::path::PathBuf::from(&path)),
                        title: title.clone(),
                        text: text.clone(),
                        dirty: false,
                        stamp: disk_stamp(std::path::Path::new(&path)),
                    });
                }
                self.lsp.opened(pane, &path, &text);
                self.broadcast(&ServerMsg::FileState {
                    pane,
                    path: Some(path),
                    title,
                    text,
                    dirty: false,
                });
            }

            ClientMsg::EditFile {
                pane,
                base_len,
                edits,
            } => {
                let Some(f) = self.panes.get_mut(&pane).and_then(|p| p.file.as_mut()) else {
                    return Ok(true);
                };
                // 長さが合わないなら取りこぼしている。当てずに立て直す。
                let mut ok = f.text.len() == base_len;
                if ok {
                    for e in &edits {
                        if !e.apply(&mut f.text) {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    f.dirty = true;
                    let text = f.text.clone();
                    self.lsp.changed(pane, &text);
                } else {
                    self.send_to(id, &ServerMsg::NeedFullFile { pane });
                }
            }

            ClientMsg::SetFile { pane, text } => {
                if let Some(f) = self.panes.get_mut(&pane).and_then(|p| p.file.as_mut()) {
                    f.dirty = f.text != text;
                    f.text = text.clone();
                }
                self.lsp.changed(pane, &text);
            }

            ClientMsg::SaveFile { pane, path: given } => {
                let Some(f) = self.panes.get_mut(&pane).and_then(|p| p.file.as_mut()) else {
                    self.send_to(
                        id,
                        &ServerMsg::Error {
                            message: "このペインはファイルではありません".into(),
                        },
                    );
                    return Ok(true);
                };
                if let Some(g) = given {
                    let as_path = std::path::PathBuf::from(&g);
                    f.title = as_path
                        .file_name()
                        .map_or_else(|| g.clone(), |n| n.to_string_lossy().into_owned());
                    f.path = Some(as_path);
                }
                let Some(path) = f.path.clone() else {
                    self.send_to(
                        id,
                        &ServerMsg::Error {
                            message: "保存先がありません（`:w <パス>` で決めてください）".into(),
                        },
                    );
                    return Ok(true);
                };
                let text = f.text.clone();
                match write_atomically(&path, &text) {
                    Ok(()) => {
                        f.dirty = false;
                        // 自分で書いたぶんを「外で変わった」と誤らない。
                        f.stamp = disk_stamp(&path);
                        self.lsp.saved(pane);
                        let path = path.display().to_string();
                        self.broadcast(&ServerMsg::FileSaved { pane, path });
                    }
                    Err(e) => self.send_to(
                        id,
                        &ServerMsg::Error {
                            message: format!("保存できません: {e}"),
                        },
                    ),
                }
            }

            ClientMsg::PipeResult {
                pane,
                dir,
                title,
                text,
            } => {
                let (cols, rows) = (self.cols, self.rows);
                let here = self.pane_cwd(pane);
                let new_pane = self.spawn_pane_in(cols, rows, here, None)?;
                if let Some(p) = self.panes.get_mut(&new_pane) {
                    p.file = Some(ServerFile {
                        path: None,
                        title: title.clone(),
                        text: text.clone(),
                        dirty: true,
                        stamp: None,
                    });
                }
                if let Some(tab) = self.tab_of(pane) {
                    tab.layout.split(pane, new_pane, dir);
                    tab.active_pane = new_pane;
                }
                let info = self.info();
                self.broadcast(&ServerMsg::Layout(info));
                self.shape_changed();
                self.broadcast(&ServerMsg::FileState {
                    pane: new_pane,
                    path: None,
                    title,
                    text,
                    dirty: true,
                });
            }

            ClientMsg::CloseFile { pane } => {
                if let Some(p) = self.panes.get_mut(&pane) {
                    p.file = None;
                    p.preview = false;
                }
                self.lsp.closed(pane);
                self.broadcast(&ServerMsg::FileClosed { pane });
            }

            ClientMsg::SwapPanes { a, b } => {
                if let Some(tab) = self.tab_of(a)
                    && tab.layout.swap(a, b)
                {
                    // 見ている場所は動かさない。入れ替えたのは中身なので、
                    // カーソルまで飛ぶと「どっちが動いたのか」が分からなくなる。
                    tab.active_pane = b;
                    let info = self.info();
                    self.broadcast(&ServerMsg::Layout(info));
                    self.shape_changed();
                }
            }

            ClientMsg::Equalize { tab } => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.layout.equalize();
                    let info = self.info();
                    self.broadcast(&ServerMsg::Layout(info));
                    self.shape_changed();
                }
            }

            ClientMsg::SetZoom { tab, pane } => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    // 同じペインをもう一度指したら戻す（トグル）
                    t.zoom = if t.zoom == pane { None } else { pane };
                    if let Some(p) = t.zoom {
                        t.active_pane = p;
                    }
                    let info = self.info();
                    self.broadcast(&ServerMsg::Layout(info));
                }
            }

            ClientMsg::MoveTab { tab, to } => {
                if let Some(from) = self.tabs.iter().position(|t| t.id == tab) {
                    let to = to.min(self.tabs.len().saturating_sub(1));
                    if from != to {
                        let t = self.tabs.remove(from);
                        self.tabs.insert(to, t);
                        let info = self.info();
                        self.broadcast(&ServerMsg::Layout(info));
                    }
                }
            }

            ClientMsg::NewTab { cwd, command } => {
                let (cols, rows) = (self.cols, self.rows);
                // 頼まれた場所が無ければ、いま居るペインと同じ場所。
                let here = cwd.or_else(|| self.active_pane().and_then(|p| self.pane_cwd(p)));
                let saved_cwd = std::mem::replace(&mut self.spawn_cwd, here);
                let saved_cmd = std::mem::replace(&mut self.spawn_command, command);
                let made = self.new_tab(cols, rows);
                self.spawn_cwd = saved_cwd;
                self.spawn_command = saved_cmd;
                made?;
                let info = self.info();
                self.broadcast(&ServerMsg::Layout(info));
                self.shape_changed();
            }

            ClientMsg::SelectTab { tab } => {
                if self.tabs.iter().any(|t| t.id == tab) {
                    self.active_tab = tab;
                    let info = self.info();
                    self.broadcast(&ServerMsg::Layout(info));
                }
            }

            // 🔴 これが永続性の本体。クライアントを外すだけで、ペインは殺さない。
            ClientMsg::Detach => {
                self.clients.remove(&id);
            }

            ClientMsg::Shutdown => return Ok(false),
            ClientMsg::Ping => self.send_to(id, &ServerMsg::Pong),
        }
        Ok(true)
    }

    fn handle(&mut self, event: Event) -> bool {
        match event {
            Event::ClientConnected { id, writer } => {
                self.clients.insert(id, writer);
            }
            Event::ClientGone { id } => {
                // 接続が切れてもペインは生かす（デタッチと同じ扱い）。
                self.clients.remove(&id);
                self.subs.remove(&id);
                self.waits.retain(|w| w.client != id);
                // 落ちた拡張の語彙は消す。押しても何も起きない項目が
                // メニューに残り続けるのが一番悪い。
                let before = self.ext.len();
                self.ext.retain(|_, (owner, _)| *owner != id);
                if self.ext.len() != before {
                    self.broadcast_ext();
                }
                // ペインは閉じない。**読んでいる途中に消えるほうが困る。**
                // 手放すのは名前だけで、中身はそのまま残る。
                let had_panes = self.ext_panes.len();
                self.ext_panes.retain(|_, (owner, _)| *owner != id);
                let left = had_panes - self.ext_panes.len();
                if self.ext_names.contains_key(&id) || before != self.ext.len() || left > 0 {
                    self.note(id, "切れました", false);
                }
                self.ext_names.remove(&id);
            }
            Event::ClientMsg { id, msg } => match self.on_client(id, msg) {
                Ok(true) => {}
                Ok(false) => return false,
                Err(e) => {
                    let msg = ServerMsg::Error {
                        message: format!("{e:#}"),
                    };
                    self.send_to(id, &msg);
                }
            },
            Event::PtyOutput { pane, data } => {
                // 場所が変わったら控えも置き直す（`cd` して落ちたとき、
                // 前の場所で開き直すのは頼んだことと違う）。**変わった
                // ときだけ**書く — 出力のたびに書いたら意味が無い。
                let before = self.panes.get(&pane).and_then(|p| p.term.state.cwd.clone());
                // 「終わったコマンド」の数を食べる前に数えておく。
                // **画面から当てない。** 出力の形が変わった日に黙って壊れる。
                let done_before = self
                    .panes
                    .get(&pane)
                    .map_or(0, |p| finished_blocks(&p.term.state.marks));
                if let Some(p) = self.panes.get_mut(&pane) {
                    p.term.feed(&data);
                }
                let after = self.panes.get(&pane).and_then(|p| p.term.state.cwd.clone());
                if before != after {
                    self.save_for_restore();
                    if let Some(cwd) = after.clone() {
                        self.emit(PluginEvent::Cwd { pane, cwd });
                    }
                }
                let events = self.finished_since(pane, done_before);
                // 待っている相手へ、その一回で新しく出た字と
                // 「終わったかどうか」をまとめて渡す。
                let ended = events.iter().find_map(|e| match e {
                    PluginEvent::CommandEnd { exit_code, .. } => Some(*exit_code),
                    _ => None,
                });
                if !self.waits.is_empty() {
                    let chunk = plain_text(&data);
                    self.push_tails(pane, &chunk);
                    self.feed_waits(pane, ended);
                }
                for event in events {
                    self.emit(event);
                }
                self.type_pending(pane);
                let msg = ServerMsg::Output {
                    pane,
                    data: encode_bytes(&data),
                };
                self.broadcast(&msg);
            }
            Event::PtyExit { pane } => {
                if let Some(p) = self.panes.get_mut(&pane) {
                    p.alive = false;
                }
                self.broadcast(&ServerMsg::PaneExited { pane });
                self.emit(PluginEvent::PaneClosed { pane });
                // 終わったペインは控えから外す。**自分で `exit` したものを
                // 次の起動で開き直さない。**
                self.shape_changed();
            }
            Event::Tick => {
                self.reload_changed_files();
                self.poll_lsp();
                self.expire_waits();
            }
            Event::Stop => return false,
        }
        true
    }
}

/// その `id` で開いてあり、**まだ生きていて**、その接続のものであるペイン。
///
/// 人が閉じたペインの番号を握ったままにすると、次に書いたものが
/// 「もう無いペイン」へ行って黙って消える。引くたびに生死を見る。
fn live_ext_pane<T>(
    ext_panes: &BTreeMap<String, (u64, u32)>,
    panes: &BTreeMap<u32, T>,
    id: &str,
    owner: u64,
) -> Option<u32> {
    let (who, pane) = ext_panes.get(id)?;
    (*who == owner && panes.contains_key(pane)).then_some(*pane)
}

/// git を 1 回起こす。**窓は出さない。**
///
/// 断られた理由はそのまま返す。git の言葉のほうが、こちらで言い換えるより
/// 正確で、検索もできる。
fn git(dir: &str, args: &[&str]) -> std::result::Result<String, String> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(args).current_dir(dir);
    cmd.stdin(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd
        .output()
        .map_err(|e| format!("git を起こせません: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let msg = String::from_utf8_lossy(&out.stderr);
        Err(msg.trim().lines().next().unwrap_or("git が断りました").to_string())
    }
}

/// `git worktree list --porcelain` を読む。
///
/// 1 つの塊が空行で区切られ、`worktree <path>` / `branch refs/heads/<name>` /
/// `detached` が並ぶ。**枝が無い塊もある**ので、そこで諦めない。
fn parse_worktrees(out: &str) -> Vec<WorktreeInfo> {
    let mut items: Vec<WorktreeInfo> = Vec::new();
    let mut path = String::new();
    let mut branch = String::new();
    let flush = |path: &mut String, branch: &mut String, items: &mut Vec<WorktreeInfo>| {
        if !path.is_empty() {
            items.push(WorktreeInfo {
                path: std::mem::take(path),
                branch: std::mem::take(branch),
                // 1 つ目が本体。git がその順で出す。
                main: items.is_empty(),
            });
        }
        branch.clear();
    };
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            flush(&mut path, &mut branch, &mut items);
            path = p.trim().to_string();
        } else if let Some(b) = line.strip_prefix("branch ") {
            branch = b.trim().trim_start_matches("refs/heads/").to_string();
        }
    }
    flush(&mut path, &mut branch, &mut items);
    items
}

/// 終わったコマンドの数（終了コードが付いた塊）。
fn finished_blocks(marks: &tsg_term::SemanticMarks) -> usize {
    marks
        .blocks()
        .iter()
        .filter(|b| b.exit_code.is_some())
        .count()
}

/// `start` / `end` を実際の行数へ収める。**外から来る数を信じない。**
fn clip_range(total: usize, start: Option<usize>, end: Option<usize>) -> (usize, usize) {
    let from = start.unwrap_or(0).min(total);
    let to = end.map_or(total, |e| e.saturating_add(1)).clamp(from, total);
    (from, to)
}

// ---------------------------------------------------------------------------

pub struct ServerHandle {
    pub session: String,
    tx: Sender<Event>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl ServerHandle {
    /// 止める。**口も手放す**ので、同じ名前ですぐ開き直せる。
    ///
    /// 受け側は待たない受け方をしているので、合図を立てれば次に目を開けた
    /// ときに気づく（`accept_loop`）。
    pub fn shutdown(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.tx.send(Event::Stop);
    }
}

// ソケットは `endpoint` が開く。**自分だけに閉じた口でなければ開かない。**
// バインドを同期的に済ませるのは、`spawn()` が返った直後にクライアントが
// 繋ぎに来て「接続できない」で落ちるのを避けるため（実際に踏んだ）。
// 開くところまでは呼び出し側の同期処理にして、accept ループだけを別スレッドへ渡す。

/// 接続を受け続ける。**止まれと言われたら、待たずに抜ける。**
///
/// `accept` を塞いだまま待つと、止める合図に気づけない。気づかないと
/// 口を握ったままになり、同じ名前で開き直せない（実測: 6 秒待っても
/// 開けなかった）。捨て玉を繋いで起こす手もあるが、込み合っていると
/// その接続自体が弾かれて、起こせないことがある。
///
/// **待たない受け方にして、合図を自分で見に行く。** 1 秒に 20 回
/// 目を開けるだけなので、寝ている間の負担にはならない。
fn accept_loop(
    listener: interprocess::local_socket::Listener,
    tx: Sender<Event>,
    stop: &std::sync::atomic::AtomicBool,
) {
    use interprocess::local_socket::ListenerNonblockingMode;
    use interprocess::local_socket::traits::Listener as _;
    use std::sync::atomic::Ordering;

    // 受けるところだけ待たない。**繋がったあとの読み書きは今までどおり**
    // （待たない読み書きにすると、全部の経路を書き換えることになる）。
    let polling = listener
        .set_nonblocking(ListenerNonblockingMode::Accept)
        .is_ok();

    let mut next_id = 1u64;
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let stream = match listener.accept() {
            Ok(s) => s,
            Err(e) if polling && e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
            Err(_) => continue,
        };
        // **受けた口は待つ側へ戻す。** BSD 系（macOS）では、待たない受け方を
        // した listener から受けた口が「待たない」を引き継ぐ。そのまま読むと
        // 中身が来ていないだけで終わったものとして扱われ、返事が返らなくなる
        // （CI の macOS で往復が丸ごと落ちて気づいた。Linux では起きない）。
        if polling {
            use interprocess::local_socket::traits::Stream as _;
            let _ = stream.set_nonblocking(false);
        }
        let Ok(write_half) = stream.try_clone() else {
            continue;
        };
        let id = next_id;
        next_id += 1;

        if tx
            .send(Event::ClientConnected {
                id,
                writer: Box::new(write_half),
            })
            .is_err()
        {
            break;
        }

        let tx2 = tx.clone();
        thread::spawn(move || {
            let reader = BufReader::new(stream);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<ClientMsg>(&line) {
                    Ok(msg) => {
                        if tx2.send(Event::ClientMsg { id, msg }).is_err() {
                            return;
                        }
                    }
                    // 壊れた行は捨てる。落とさない。
                    Err(_) => continue,
                }
            }
            let _ = tx2.send(Event::ClientGone { id });
        });
    }
}

/// listener を起こし、イベントの受け口を返す。
type Setup = (
    Sender<Event>,
    mpsc::Receiver<Event>,
    Endpoint,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
);

fn setup(session: &str) -> Result<Setup> {
    let (tx, rx) = mpsc::channel::<Event>();
    let endpoint = Endpoint::for_session(session)?;
    // バインドはここで済ませる。返った時点で接続を受けられることを保証する。
    let listener = endpoint.bind()?;
    let t = tx.clone();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let s = stop.clone();
    thread::Builder::new()
        .name("tsg-mux-listener".into())
        .spawn(move || accept_loop(listener, t, &s))?;

    // 時計。**外で書き換えられたファイルに気づく**ためだけにある。
    // 送り先が消えたら終わる（サーバが止まったということ）。
    let tick = tx.clone();
    thread::Builder::new()
        .name("tsg-mux-tick".into())
        .spawn(move || {
            while tick.send(Event::Tick).is_ok() {
                thread::sleep(std::time::Duration::from_secs(1));
            }
        })?;

    Ok((tx, rx, endpoint, stop))
}

fn state_loop(state: &mut State, rx: mpsc::Receiver<Event>) {
    while let Ok(event) = rx.recv() {
        if !state.handle(event) {
            break;
        }
    }
    for pane in state.panes.values_mut() {
        let _ = pane.pty.kill();
    }
    // **自分から終えたときは控えを捨てる。** ここまで来たということは
    // 「終わりにする」と言われたということ。次の起動で前の形が出てきたら、
    // 終えたはずのものが戻ってくることになる。
    //
    // 電源が落ちた場合はここを通らない。控えが残るのはそのときだけでいい。
    crate::restore::clear(&state.session);
}

/// サーバを別スレッドで起こす（テストと、同一プロセスからの利用向け）。
pub fn spawn(session: &str) -> Result<ServerHandle> {
    let (tx, rx, endpoint, stop) = setup(session)?;
    let mut state = State::new(session.to_string(), tx.clone());
    thread::Builder::new()
        .name("tsg-mux-state".into())
        .spawn(move || {
            state_loop(&mut state, rx);
            endpoint.cleanup();
        })?;
    Ok(ServerHandle {
        session: session.to_string(),
        tx,
        stop,
    })
}

/// `restore` を切ると、前回の形から組み直さずに素の 1 ペインで開く。
pub fn run_with(
    session: &str,
    restore: bool,
    lsp: std::collections::BTreeMap<String, tsg_lsp::servers::Spec>,
) -> Result<()> {
    let (tx, rx, endpoint, _stop) = setup(session)?;
    // 一覧に出せるよう、生きている間だけ控えを置く（`sessions` の説明を参照）
    crate::sessions::register(session);
    let mut state = State::new(session.to_string(), tx);
    state.restore = restore;
    state.lsp.set_overrides(lsp);
    state_loop(&mut state, rx);
    crate::sessions::unregister(session);
    endpoint.cleanup();
    Ok(())
}

/// 木の中のペイン id を置き換える。戻せなかったペインは取り除く。
///
/// **1 つも残らない枝は消す。** 空の枝を残すと、割り付けで幅 0 のペインが
/// できて、そこへは二度と行けなくなる。
fn remap_layout(layout: &Layout, made: &std::collections::BTreeMap<u32, u32>) -> Option<Layout> {
    match layout {
        Layout::Leaf { pane } => made.get(pane).map(|id| Layout::leaf(*id)),
        Layout::Split {
            dir,
            children,
            weights,
        } => {
            let kept: Vec<Layout> = children
                .iter()
                .filter_map(|c| remap_layout(c, made))
                .collect();
            match kept.len() {
                0 => None,
                1 => kept.into_iter().next(),
                _ => Some(Layout::Split {
                    dir: *dir,
                    children: kept,
                    weights: weights.clone(),
                }),
            }
        }
    }
}

/// 同じディレクトリへ書いてから置き換える。
///
/// `fs::write` は**先に切り詰めてから書く**。途中で電源が落ちたり書き込みが
/// 失敗したりすると、元の中身も新しい中身も無い空のファイルが残る。
/// エディタとしてこれは起こしてはいけない事故なので、置き換えでやる。
///
/// 同じディレクトリに作るのは、`rename` が同一ファイルシステム内でしか
/// 原子的にならないため（`/tmp` に作ると跨いでコピーになる）。
fn write_atomically(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    let Some(dir) = path.parent() else {
        return std::fs::write(path, text);
    };
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.tsg-{}.tmp", std::process::id()));

    let result = (|| {
        use std::io::Write as _;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        // rename の前に落とす。書けたことにして名前を付け替え、中身が
        // 空だった、が一番たちが悪い。
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn default_shell() -> String {
    if cfg!(windows) {
        std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".into())
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    }
}

/// `file://host/C:/dev/x` 形式（OSC 7）をパスへ戻す。
///
/// シェル統合が吐くのはこの形。パーセントエンコードは戻す。
/// ホスト名は見ない（リモートの cwd をローカルで開いても意味が無いが、
/// そこを弾くよりホームへ落ちる方が実害が小さい）。
fn parse_file_url(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    let (host, path) = rest.split_once('/').map_or(("", rest), |(h, p)| (h, p));
    // **ホストを見る。** OSC 7 は子プロセスが自由に言える。よそのホストを
    // 名乗られたら断る（`file://evil/…` を触りに行かない）。
    if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
        return None;
    }
    let decoded = percent_decode(path);
    let cleaned = decoded.trim_start_matches('/');
    // Windows は `C:/dev/x`、Unix は `/home/x`
    let out = if cleaned.chars().nth(1) == Some(':') {
        cleaned.to_string()
    } else {
        decoded
    };
    if !is_safe_local_dir(&out) {
        return None;
    }
    std::path::Path::new(&out).is_dir().then_some(out)
}

/// 触りに行ってよい場所か。**`is_dir()` を呼ぶ前に見る。**
///
/// Windows で `\host\share` を `is_dir()` すると、その場で SMB へ繋ぎに行き、
/// 現在のユーザの資格情報を相手へ渡してしまう。**画面に字を出しただけで
/// それが起きる**（OSC 7 は子プロセスが自由に言える）ので、その形を先に断る。
fn is_safe_local_dir(path: &str) -> bool {
    if path.is_empty() || path.len() > 4096 {
        return false;
    }
    // UNC（`\host\share` `//host/share`）とデバイスパス（`\?\` `\.\`）
    let head: String = path.chars().take(2).collect();
    if head == "\\\\" || head == "//" {
        return false;
    }
    // 制御文字を含む場所は相手にしない
    !path.chars().any(char::is_control)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// ディスク側の状態（更新時刻と大きさ）。読めなければ `None`。
///
/// **大きさも見る。** 更新時刻の粒度は環境で違い、同じ秒の中の
/// 書き換えを取りこぼすことがある。
fn disk_stamp(path: &std::path::Path) -> Option<(std::time::SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

fn home_dir() -> Option<String> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(id: u32) -> Layout {
        Layout::leaf(id)
    }

    /// 葉を並べた木を組む。`split` は「その葉を割る」形なので、
    /// 試験からはこの形のほうが読める。
    fn row(ids: &[u32]) -> Layout {
        let mut t = leaf(ids[0]);
        for w in ids.windows(2) {
            assert!(t.split(w[0], w[1], Dir::Horizontal), "分割できない");
        }
        t
    }

    fn made(pairs: &[(u32, u32)]) -> std::collections::BTreeMap<u32, u32> {
        pairs.iter().copied().collect()
    }

    /// 戻せたペインは新しい id へ置き換わる。
    #[test]
    fn a_restored_pane_keeps_its_place_in_the_tree() {
        let tree = row(&[1, 2]);
        let out = remap_layout(&tree, &made(&[(1, 10), (2, 11)])).expect("木ごと消えた");
        assert_eq!(out.panes(), vec![10, 11]);
    }

    /// **戻せなかったペインは枝ごと消す。**
    ///
    /// 空の枝を残すと、割り付けで幅 0 のペインができて、そこへは
    /// 二度と行けなくなる。
    #[test]
    fn a_pane_that_could_not_be_restored_leaves_no_empty_branch() {
        let tree = row(&[1, 2]);
        let out = remap_layout(&tree, &made(&[(1, 10)])).expect("残った 1 つも消えた");
        assert_eq!(out.panes(), vec![10], "戻せなかったぶんが残っている");

        assert!(
            remap_layout(&tree, &made(&[])).is_none(),
            "1 つも戻せないのに木が残っている"
        );
    }

    /// 入れ子でも同じ。中の枝が空になったら、その枝ごと消える。
    #[test]
    fn an_inner_branch_collapses_when_it_empties() {
        let mut tree = row(&[1, 2]);
        assert!(tree.split(2, 3, Dir::Vertical), "内側を割れない");
        let out = remap_layout(&tree, &made(&[(1, 10)])).expect("木ごと消えた");
        assert_eq!(out.panes(), vec![10]);
    }

    // ---- 拡張の口 ---------------------------------------------------------

    /// 繋いだ相手の代わり。書かれたものを溜めるだけ。
    #[derive(Clone, Default)]
    struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Sink {
        fn msgs(&self) -> Vec<ServerMsg> {
            let raw = self.0.lock().unwrap().clone();
            String::from_utf8_lossy(&raw)
                .lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        }
    }

    /// 誰も繋がっていない状態から、口だけを試す。
    fn bare_state() -> (State, Sender<Event>) {
        let (tx, rx) = mpsc::channel();
        // 受け側を落とすと `send` が失敗するので、持たせたまま返す。
        std::mem::forget(rx);
        (State::new("test".into(), tx.clone()), tx)
    }

    fn join(state: &mut State, id: u64) -> Sink {
        let sink = Sink::default();
        state.clients.insert(id, Box::new(sink.clone()));
        sink
    }

    #[test]
    fn events_only_reach_the_connections_that_asked_for_them() {
        let (mut st, _tx) = bare_state();
        let asked = join(&mut st, 1);
        let quiet = join(&mut st, 2);

        st.on_client(
            1,
            ClientMsg::Subscribe {
                events: vec!["command_end".into()],
            },
        )
        .unwrap();
        st.emit(PluginEvent::CommandEnd {
            pane: 1,
            exit_code: Some(0),
            command: "ls".into(),
            output_start: Some(1),
            output_end: Some(3),
        });

        assert!(
            asked.msgs().iter().any(|m| matches!(m, ServerMsg::Event { .. })),
            "名乗った相手へ届いていない"
        );
        assert!(
            quiet.msgs().is_empty(),
            "名乗っていない相手にまで配っている"
        );
    }

    #[test]
    fn subscribing_to_a_name_that_does_not_exist_says_so() {
        let (mut st, _tx) = bare_state();
        let sink = join(&mut st, 1);
        st.on_client(
            1,
            ClientMsg::Subscribe {
                events: vec!["comand_end".into()],
            },
        )
        .unwrap();
        assert!(
            sink.msgs()
                .iter()
                .any(|m| matches!(m, ServerMsg::Error { .. })),
            "打ち間違えたまま待たせている"
        );
    }

    #[test]
    fn an_extension_command_needs_the_ext_prefix() {
        let (mut st, _tx) = bare_state();
        let sink = join(&mut st, 1);
        st.on_client(
            1,
            ClientMsg::RegisterCommand {
                command: ExtCommand {
                    id: "blame".into(),
                    title: "blame".into(),
                    title_en: "blame".into(),
                    keys: vec![],
                    menu: None,
                },
            },
        )
        .unwrap();
        assert!(
            sink.msgs()
                .iter()
                .any(|m| matches!(m, ServerMsg::Error { .. })),
            "名前空間の無い id を受け付けている"
        );
        assert!(st.ext.is_empty());
    }

    #[test]
    fn a_registered_command_is_handed_back_to_the_extension_that_owns_it() {
        let (mut st, _tx) = bare_state();
        let ext = join(&mut st, 1);
        let window = join(&mut st, 2);

        st.on_client(
            1,
            ClientMsg::Subscribe {
                events: vec!["command".into()],
            },
        )
        .unwrap();
        st.on_client(
            1,
            ClientMsg::RegisterCommand {
                command: ExtCommand {
                    id: "ext.blame".into(),
                    title: "行の来歴".into(),
                    title_en: "Blame this line".into(),
                    keys: vec![],
                    menu: Some("編集".into()),
                },
            },
        )
        .unwrap();
        // 窓が押した
        st.on_client(
            2,
            ClientMsg::RunCommand {
                id: "ext.blame".into(),
                arg: None,
            },
        )
        .unwrap();

        assert!(
            ext.msgs().iter().any(|m| matches!(
                m,
                ServerMsg::Event {
                    event: PluginEvent::Command { id, .. }
                } if id == "ext.blame"
            )),
            "登録した拡張へ返っていない"
        );
        assert!(
            !window.msgs().iter().any(|m| matches!(m, ServerMsg::RunCommand { .. })),
            "窓は知らない id を受け取るべきではない"
        );
    }

    #[test]
    fn a_dead_extension_takes_its_commands_with_it() {
        let (mut st, _tx) = bare_state();
        let _ext = join(&mut st, 1);
        let window = join(&mut st, 2);
        st.on_client(
            1,
            ClientMsg::RegisterCommand {
                command: ExtCommand {
                    id: "ext.blame".into(),
                    title: "行の来歴".into(),
                    title_en: "Blame this line".into(),
                    keys: vec![],
                    menu: None,
                },
            },
        )
        .unwrap();
        assert_eq!(st.ext.len(), 1);

        st.handle(Event::ClientGone { id: 1 });
        assert!(st.ext.is_empty(), "落ちた拡張の語彙が残っている");
        // 窓には「もう無い」が配り直されている
        assert!(
            window.msgs().iter().any(|m| matches!(
                m,
                ServerMsg::ExtCommands { commands } if commands.is_empty()
            )),
            "窓へ配り直していない"
        );
    }

    #[test]
    fn a_finished_command_is_counted_from_the_shell_marks() {
        // OSC 133 が言ってきたことだけを数える。**画面から当てない。**
        let mut term = Terminal::new(20, 5, tsg_term::ambiguous());
        assert_eq!(finished_blocks(&term.state.marks), 0);
        term.feed(b"]133;A$ ls
]133;Ca.txt
]133;D;0");
        assert_eq!(finished_blocks(&term.state.marks), 1);
        // まだ終わっていない塊は数えない
        term.feed(b"]133;A$ sleep
]133;C");
        assert_eq!(finished_blocks(&term.state.marks), 1);
        term.feed(b"]133;D;1");
        assert_eq!(finished_blocks(&term.state.marks), 2);
    }

    /// 拡張のペインの引き当て。**生きているかを毎回見る。**
    #[test]
    fn an_extension_pane_is_only_handed_back_while_it_lives() {
        let mut ext: BTreeMap<String, (u64, u32)> = BTreeMap::new();
        ext.insert("ext.blame".into(), (1, 7));
        let alive: BTreeMap<u32, ()> = [(7u32, ())].into_iter().collect();
        let gone: BTreeMap<u32, ()> = BTreeMap::new();

        assert_eq!(live_ext_pane(&ext, &alive, "ext.blame", 1), Some(7));
        // 人が閉じた後。**古い番号を握ったままにすると、書いたものが
        // 「もう無いペイン」へ行って黙って消える。**
        assert_eq!(live_ext_pane(&ext, &gone, "ext.blame", 1), None);
        // 別の拡張のものは触らせない。
        assert_eq!(live_ext_pane(&ext, &alive, "ext.blame", 2), None);
        // 開いていない名前。
        assert_eq!(live_ext_pane(&ext, &alive, "ext.other", 1), None);
    }

    /// 落ちた拡張の**語彙は消すが、ペインは残す**。
    ///
    /// 押しても何も起きない項目は嘘になるが、ペインの中身は読めるもので、
    /// 読んでいる途中に消えるほうが困る。
    #[test]
    fn a_dead_extension_keeps_its_pane_but_loses_the_name() {
        let (mut st, _tx) = bare_state();
        let _ext = join(&mut st, 1);
        st.ext_panes.insert("ext.blame".into(), (1, 7));
        st.on_client(
            1,
            ClientMsg::RegisterCommand {
                command: ExtCommand {
                    id: "ext.blame".into(),
                    title: "来歴".into(),
                    title_en: "Blame".into(),
                    keys: vec![],
                    menu: None,
                },
            },
        )
        .unwrap();

        st.handle(Event::ClientGone { id: 1 });
        assert!(st.ext.is_empty(), "落ちた拡張の語彙が残っている");
        assert!(
            st.ext_panes.is_empty(),
            "名前は手放すべき（次の拡張が同じ名前を使えるように）"
        );
    }

    // ---- 拡張の記録 -------------------------------------------------------

    /// **断った理由は必ず残る。** 「繋がっているのに何も起きない」を
    /// 人が調べられる場所が、どこにも無いのが一番困る。
    #[test]
    fn every_refusal_leaves_a_record() {
        let (mut st, _tx) = bare_state();
        let _sink = join(&mut st, 1);
        st.on_client(
            1,
            ClientMsg::Subscribe {
                events: vec!["comand_end".into()],
            },
        )
        .unwrap();
        let refused: Vec<&ExtLogEntry> = st.ext_log.iter().filter(|e| e.refused).collect();
        assert_eq!(refused.len(), 1, "断ったのに記録が無い");
        assert!(refused[0].what.contains("知らない出来事"));
    }

    /// 名乗れば記録が読める形になる。名乗らなければ接続の番号のまま。
    #[test]
    fn naming_yourself_makes_the_record_readable() {
        let (mut st, _tx) = bare_state();
        let _sink = join(&mut st, 1);
        let _other = join(&mut st, 2);

        st.on_client(
            1,
            ClientMsg::ExtHello {
                name: "blame".into(),
            },
        )
        .unwrap();
        st.on_client(
            2,
            ClientMsg::RegisterCommand {
                command: ExtCommand {
                    id: "bad".into(),
                    title: "x".into(),
                    title_en: "x".into(),
                    keys: vec![],
                    menu: None,
                },
            },
        )
        .unwrap();

        let names: Vec<&str> = st.ext_log.iter().map(|e| e.who.as_str()).collect();
        assert!(names.contains(&"blame"), "名乗った名前が出ていない");
        assert!(names.contains(&"#2"), "名乗らない接続は番号で出るべき");
    }

    /// 記録は輪。**走らせっぱなしのサーバで静かに太らない。**
    #[test]
    fn the_record_is_a_ring_and_does_not_grow_forever() {
        let (mut st, _tx) = bare_state();
        let _sink = join(&mut st, 1);
        for i in 0..(EXT_LOG_MAX + 25) {
            st.note(1, format!("{i}"), false);
        }
        assert_eq!(st.ext_log.len(), EXT_LOG_MAX);
        // 捨てるのは古いほうから。
        assert_eq!(st.ext_log.front().map(|e| e.what.as_str()), Some("25"));
    }

    /// 返すのは新しいほうから n 本、並びは**古い順**（読む順）。
    #[test]
    fn the_log_comes_back_newest_first_but_in_reading_order() {
        let (mut st, _tx) = bare_state();
        let sink = join(&mut st, 1);
        for i in 0..10 {
            st.note(1, format!("{i}"), false);
        }
        st.on_client(1, ClientMsg::ExtLog { limit: Some(3) })
            .unwrap();
        let got = sink
            .msgs()
            .into_iter()
            .find_map(|m| match m {
                ServerMsg::ExtLog { entries } => Some(entries),
                _ => None,
            })
            .expect("記録が返っていない");
        let what: Vec<String> = got.into_iter().map(|e| e.what).collect();
        assert_eq!(what, vec!["7", "8", "9"]);
    }

    // ---- 待つときに見る字 -------------------------------------------------

    /// **飾りを落とす。** 色を付けて出すテストランナーで
    /// `PASS` が探せないと、待ちの半分は使いものにならない。
    #[test]
    fn colours_do_not_hide_the_word_you_are_waiting_for() {
        let got = plain_text(b"[32mPASS[0m 12 tests");
        assert_eq!(got, "PASS 12 tests");
    }

    #[test]
    fn osc_sequences_are_dropped_whole() {
        // OSC 7（居場所）は BEL で終わる。
        let got = plain_text(b"]7;file:///c:/wdone");
        assert_eq!(got, "done");
        // ST（`ESC \`）で終わる書き方もある。
        let got = plain_text(b"\x1b]0;title\x1b\\after");
        assert_eq!(got, "after");
    }

    #[test]
    fn newlines_and_tabs_survive_but_other_controls_do_not() {
        let got = plain_text(b"a	b
cd");
        assert_eq!(got, "a	b
cd", "改行とタブは残す。ベルと CR は落とす");
    }

    /// 控えは頭から捨てる。**字の境目で切る**（途中で切ると落ちる）。
    #[test]
    fn the_tail_is_trimmed_from_the_front_at_a_character_boundary() {
        let (mut st, _tx) = bare_state();
        let _sink = join(&mut st, 1);
        st.waits.push(Waiting {
            client: 1,
            pane: None,
            matcher: Match::Substring {
                text: "NOPE".into(),
            },
            tail: String::new(),
            deadline: None,
        });
        // 全角で埋める（1 字 3 バイト）。
        let chunk = "あ".repeat(WAIT_TAIL_MAX);
        st.push_tails(1, &chunk);
        let tail = &st.waits[0].tail;
        assert!(tail.len() <= WAIT_TAIL_MAX, "捨てていない");
        assert!(tail.chars().all(|c| c == 'あ'), "字の途中で切っている");
    }

    // ---- 作業ツリー -------------------------------------------------------

    /// `git worktree list --porcelain` の読み方。
    /// **枝の無い塊もある**ので、そこで諦めないこと。
    #[test]
    fn worktrees_are_read_from_the_porcelain_form() {
        let out = "worktree /home/a/repo
HEAD abc123
branch refs/heads/main

worktree /home/a/repo-fix
HEAD def456
branch refs/heads/fix/login

worktree /home/a/repo-detached
HEAD 999999
detached
";
        let got = parse_worktrees(out);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].branch, "main");
        assert!(got[0].main, "1 つ目が本体");
        // `refs/heads/` は落とすが、`/` を含む枝名は残す。
        assert_eq!(got[1].branch, "fix/login");
        assert!(!got[1].main);
        // detached は枝が空。**そこで塊ごと捨てない。**
        assert_eq!(got[2].path, "/home/a/repo-detached");
        assert!(got[2].branch.is_empty());
    }

    #[test]
    fn an_empty_worktree_list_is_empty_not_a_ghost() {
        assert!(parse_worktrees("").is_empty());
        assert!(parse_worktrees("

").is_empty());
    }

    #[test]
    fn a_range_from_outside_is_clipped_instead_of_panicking() {
        assert_eq!(clip_range(10, None, None), (0, 10));
        assert_eq!(clip_range(10, Some(2), Some(4)), (2, 5));
        // 端を越えて頼まれても、あるところまで
        assert_eq!(clip_range(10, Some(8), Some(99)), (8, 10));
        // 逆さに頼まれても空で返す（落ちない）
        assert_eq!(clip_range(10, Some(6), Some(2)), (6, 6));
        assert_eq!(clip_range(0, Some(3), Some(5)), (0, 0));
    }
}
