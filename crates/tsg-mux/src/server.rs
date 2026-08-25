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

use std::collections::BTreeMap;
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

struct State {
    session: String,
    /// 前回の形から組み直すか。`--no-restore` で切る。
    restore: bool,
    panes: BTreeMap<u32, Pane>,
    tabs: Vec<TabInfo>,
    active_tab: u32,
    clients: BTreeMap<u64, Box<dyn Write + Send>>,
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
            next_pane: 1,
            next_tab: 1,
            tx,
            cols: 80,
            rows: 24,
            spawn_cwd: None,
            spawn_command: None,
            restore: true,
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
        Ok(id)
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

        // 末尾の空行は送らない。送ると再アタッチのたびに画面が下へ押し出される。
        let mut end = grid.document_len();
        while end > 0
            && grid
                .document_line(end - 1)
                .is_some_and(|l| l.ansi().is_empty())
        {
            end -= 1;
        }
        let start = end.saturating_sub(SNAPSHOT_MAX_LINES);

        Some(ServerMsg::Snapshot {
            pane,
            lines: (start..end).filter_map(|i| grid.line_ansi(i)).collect(),
            cursor_line: grid.cursor_absolute().saturating_sub(start),
            cursor_col: grid.cursor.col,
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

            ClientMsg::RunCommand { id, arg } => {
                // サーバは中身を知らない。**そのまま配るだけ。**
                self.broadcast(&ServerMsg::RunCommand { id, arg });
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
                self.tabs.retain(|t| !t.layout.panes().is_empty());
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
                } else {
                    self.send_to(id, &ServerMsg::NeedFullFile { pane });
                }
            }

            ClientMsg::SetFile { pane, text } => {
                if let Some(f) = self.panes.get_mut(&pane).and_then(|p| p.file.as_mut()) {
                    f.dirty = f.text != text;
                    f.text = text;
                }
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
                if let Some(p) = self.panes.get_mut(&pane) {
                    p.term.feed(&data);
                }
                let after = self.panes.get(&pane).and_then(|p| p.term.state.cwd.clone());
                if before != after {
                    self.save_for_restore();
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
                // 終わったペインは控えから外す。**自分で `exit` したものを
                // 次の起動で開き直さない。**
                self.shape_changed();
            }
            Event::Tick => self.reload_changed_files(),
            Event::Stop => return false,
        }
        true
    }
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

/// サーバをこのプロセスの本体として回す（`tsg --server` 用）。
/// state ループが終わるまで返らない。
pub fn run(session: &str) -> Result<()> {
    run_with(session, true)
}

/// `restore` を切ると、前回の形から組み直さずに素の 1 ペインで開く。
pub fn run_with(session: &str, restore: bool) -> Result<()> {
    let (tx, rx, endpoint, _stop) = setup(session)?;
    // 一覧に出せるよう、生きている間だけ控えを置く（`sessions` の説明を参照）
    crate::sessions::register(session);
    let mut state = State::new(session.to_string(), tx);
    state.restore = restore;
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
}
