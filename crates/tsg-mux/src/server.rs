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
use interprocess::local_socket::traits::ListenerExt as _;

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
}

struct State {
    session: String,
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
        // 中で走るものに「ここは tsumugi の中だ」と教える。
        //
        // **これが無いと、中で `tsg` と打ったときに窓がもう 1 枚開く。**
        // 中に居ると分かれば、新しい窓ではなくこのセッションのタブを開ける。
        cmd.env("TSUMUGI_SESSION", &self.session);
        cmd.env("TSUMUGI_PANE", id.to_string());
        match cwd.filter(|c| std::path::Path::new(c).is_dir()) {
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
            },
        );
        Ok(id)
    }

    /// そのペインが今いるディレクトリ（OSC 7 で受け取ったもの）。
    ///
    /// 分割や新しいタブは**今いる場所**で開くのが端末の慣例。
    /// 追加の仕掛けは要らず、すでに解析済みの OSC 7 をそのまま使う。
    /// 今のタブでアクティブなペイン。
    fn active_pane(&self) -> Option<u32> {
        self.tabs
            .iter()
            .find(|t| t.id == self.active_tab)
            .map(|t| t.active_pane)
    }

    fn pane_cwd(&self, pane: u32) -> Option<String> {
        let url = self.panes.get(&pane)?.term.state.cwd.as_deref()?;
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
        });
        self.active_tab = id;
        Ok(id)
    }

    fn tab_of(&mut self, pane: u32) -> Option<&mut TabInfo> {
        self.tabs.iter_mut().find(|t| t.layout.panes().contains(&pane))
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
                    // 起動時の指定は最初のペインにだけ効く。
                    // 再アタッチで既にペインがあるなら、そこは触らない。
                    self.spawn_cwd = cwd;
                    self.spawn_command = command;
                    let (c, r) = (self.cols, self.rows);
                    self.new_tab(c, r)?;
                    self.spawn_command = None;
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

            ClientMsg::SetAgentState { pane, state } => {
                // ペインの指定が無ければ、いま選ばれているところ。
                // hooks は自分がどのペインに居るか知らないので、これが既定。
                let target = pane.or_else(|| self.active_pane());
                if let Some(p) = target.and_then(|id| self.panes.get_mut(&id))
                    && p.agent != Some(state)
                {
                    p.agent = Some(state);
                    let info = self.info();
                    self.broadcast(&ServerMsg::Layout(info));
                }
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
                let changed = self.panes.get(&pane).is_some_and(|p| p.cols != cols || p.rows != rows);
                if let Some(p) = self.panes.get_mut(&pane) {
                    p.cols = cols;
                    p.rows = rows;
                    let _ = p.pty.resize(tsg_pty::size(cols, rows));
                    p.term.state.grid.resize(cols as usize, rows as usize);
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
            }

            ClientMsg::ResizeSplit { pane, delta } => {
                // 分割比はサーバの木が持つ。クライアント側だけで持つと
                // 再アタッチで戻り、複数クライアントで食い違う。
                if let Some(tab) = self.tab_of(pane)
                    && tab.layout.resize(pane, delta)
                {
                    let info = self.info();
                    self.broadcast(&ServerMsg::Layout(info));
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
                    });
                }
                if let Some(tab) = self.tab_of(pane) {
                    tab.layout.split(pane, new_pane, dir);
                    tab.active_pane = new_pane;
                }
                let info = self.info();
                self.broadcast(&ServerMsg::Layout(info));
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
                }
            }

            ClientMsg::Equalize { tab } => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.layout.equalize();
                    let info = self.info();
                    self.broadcast(&ServerMsg::Layout(info));
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
                if let Some(p) = self.panes.get_mut(&pane) {
                    p.term.feed(&data);
                }
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
            }
            Event::Stop => return false,
        }
        true
    }
}

// ---------------------------------------------------------------------------

pub struct ServerHandle {
    pub session: String,
    tx: Sender<Event>,
}

impl ServerHandle {
    pub fn shutdown(&self) {
        let _ = self.tx.send(Event::Stop);
    }
}

// ソケットは `endpoint` が開く。**自分だけに閉じた口でなければ開かない。**
// バインドを同期的に済ませるのは、`spawn()` が返った直後にクライアントが
// 繋ぎに来て「接続できない」で落ちるのを避けるため（実際に踏んだ）。
// 開くところまでは呼び出し側の同期処理にして、accept ループだけを別スレッドへ渡す。

fn accept_loop(listener: interprocess::local_socket::Listener, tx: Sender<Event>) {
    let mut next_id = 1u64;
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
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
fn setup(session: &str) -> Result<(Sender<Event>, mpsc::Receiver<Event>, Endpoint)> {
    let (tx, rx) = mpsc::channel::<Event>();
    let endpoint = Endpoint::for_session(session)?;
    // バインドはここで済ませる。返った時点で接続を受けられることを保証する。
    let listener = endpoint.bind()?;
    let t = tx.clone();
    thread::Builder::new()
        .name("tsg-mux-listener".into())
        .spawn(move || accept_loop(listener, t))?;
    Ok((tx, rx, endpoint))
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
}

/// サーバを別スレッドで起こす（テストと、同一プロセスからの利用向け）。
pub fn spawn(session: &str) -> Result<ServerHandle> {
    let (tx, rx, endpoint) = setup(session)?;
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
    })
}

/// サーバをこのプロセスの本体として回す（`tsg --server` 用）。
/// state ループが終わるまで返らない。
pub fn run(session: &str) -> Result<()> {
    let (tx, rx, endpoint) = setup(session)?;
    // 一覧に出せるよう、生きている間だけ控えを置く（`sessions` の説明を参照）
    crate::sessions::register(session);
    let mut state = State::new(session.to_string(), tx);
    state_loop(&mut state, rx);
    crate::sessions::unregister(session);
    endpoint.cleanup();
    Ok(())
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
    let path = rest.split_once('/').map_or(rest, |(_, p)| p);
    let decoded = percent_decode(path);
    let cleaned = decoded.trim_start_matches('/');
    // Windows は `C:/dev/x`、Unix は `/home/x`
    let out = if cleaned.chars().nth(1) == Some(':') {
        cleaned.to_string()
    } else {
        decoded
    };
    std::path::Path::new(&out).is_dir().then_some(out)
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

fn home_dir() -> Option<String> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
}
