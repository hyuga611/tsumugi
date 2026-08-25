// Explorer やショートカットから起動したときにコンソールの黒窓を出さない。
// ターミナルから叩いたときの出力は `platform::attach_parent_console` で拾う。
#![cfg_attr(windows, windows_subsystem = "windows")]

//! tsumugi のホスト。
//!
//! `arch.md` の不変条件 4 の通り、**GUI は常に mux サーバのクライアント**であり、
//! ローカル 1 ウィンドウでも例外を作らない。ウィンドウを閉じてもシェルは死なない。
//!
//! ここの責務は「OS イベントを `KeyInput` / `Command` に翻訳し、`Effect` を実行する」
//! ことだけ。モーダルの判断は `tsg-modal` にしかない。

mod cli;
mod agent_hooks;
mod config;
mod input;
mod mouse;
mod overlay;
mod platform;
mod install;
mod reload;
mod rpc;
mod session;
mod theme;
mod shell;

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tsg_modal::command::{MuxRequest, SplitDir};
use tsg_modal::{
    Buffer, Effect, Engine, KeyInput, KeyOutcome, Mode, Pos, Range, RangeKind, View, t,
};
use tsg_mux::Client;
use tsg_mux::protocol::{AgentState, ClientMsg, Dir, PROTOCOL_VERSION, ServerMsg};
use tsg_render::Renderer;
use tsg_term::{Attrs, InputOwner};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use cli::{Cli, Mode as CliMode};
use config::Config;
use theme::Theme;
use session::{GUTTER, PaneView, Rect, Session};

/// クリア色。**`Color::Default` の解決先とクリア色を別々に持たせない。**
/// 2 か所に書くと、テーマを変えたときに片方だけ古い色のまま残る。
fn background_of(th: &Theme, opacity: f32) -> [f32; 4] {
    [th.bg[0], th.bg[1], th.bg[2], opacity]
}



struct App {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    /// 画面の色。**設定の読み直しで差し替わる**ので const ではない。
    theme: Theme,
    /// 起動時のコマンドライン。読み直しても**指定はコマンドラインが勝つ**
    /// ようにするため、捨てずに持っておく。
    cli: Cli,
    watch: reload::Watch,
    client: Option<Client>,
    session_name: String,
    /// 使い方の画面をどこまで送ったか。低いウィンドウでも最後まで読める。
    help_scroll: usize,
    /// 検索を始めた場所。Esc で戻すために持つ。
    search_from: Option<Pos>,
    /// まだ割っていない数（`--layout agents`）。
    pending_splits: usize,
    /// 端末に出た絵を、どこに載せたか。`(ペイン, 絵の id) -> 面の場所`。
    /// **載せ直さない**ための控え。
    image_slots: BTreeMap<(u32, u64), tsg_render::ImageSlot>,
    /// 画面に出ているものへのラベル。空でなければラベル待ち。
    hints: Vec<Hint>,
    hint_typed: String,
    /// 前に見たエージェントの状態。**変わった瞬間**にだけ知らせるために持つ。
    /// 状態そのものはサーバが持っているので、ここは通知のための控え。
    agent_seen: BTreeMap<u32, AgentState>,
    /// ウィンドウが前に居るか。裏に居るときだけタスクバーを光らせる。
    focused: bool,
    session: Session,
    engine: Engine,
    clipboard: Option<arboard::Clipboard>,

    mods: ModifiersState,
    preedit: String,

    /// マウスの現在位置（物理ピクセル）
    pointer: PhysicalPosition<f64>,
    clicks: mouse::Clicks,
    drag: Option<mouse::Drag>,
    palette: overlay::Palette,
    menu: overlay::Menu,
    picker: overlay::Picker,
    /// タブバーで掴んでいるタブ（並べ替え）。
    tab_drag: Option<u32>,
    /// ポインタの下にある対象（パス・URL・ハッシュ・語）。下線を引く。
    hover: Option<(u32, Range)>,
    /// 右ドラッグの起点。離した場所で「メニュー」か「プロンプトへ落とす」に分かれる。
    right_from: Option<(u32, Pos, usize, usize)>,
    /// 自分で分割を頼んだ。次に届く配置で、増えたペインへ移る。
    ///
    /// **フォーカスはクライアント側のもの**（同じセッションに 2 枚繋いでいるとき、
    /// 片方の操作でもう片方の見ている場所が飛ぶのは困る）。だからサーバの
    /// `active_pane` には従わず、自分が頼んだときだけ移る。
    focus_new_pane: bool,
    /// ドラッグで範囲を作るときの起点
    drag_from: Pos,

    cols: usize,
    rows: usize,
    status_msg: String,
    diagnose: bool,
    cfg: Config,
    /// 未保存の警告をすでに 1 度出したか。
    quit_warned: bool,
    /// `>` で範囲は決まったが、通すコマンドをまだ訊いていない。
    pending_pipe: Option<String>,
    /// マクロ再生の入れ子。`@a` の中で `@a` を呼べてしまうので数える。
    macro_depth: u32,
    /// 最初のペインをどこで開くか / 何を走らせるか（`--cwd` / `-e`）。
    cwd: Option<String>,
    command: Option<Vec<String>>,
}

impl App {
    fn new(cli: &Cli, cfg: Config) -> Self {
        Self {
            window: None,
            renderer: None,
            client: None,
            session_name: cli.session.clone(),
            help_scroll: 0,
            search_from: None,
            pending_splits: match cli.layout.as_deref() {
                Some("agents") => 2,
                _ => 0,
            },
            image_slots: BTreeMap::new(),
            hints: Vec::new(),
            hint_typed: String::new(),
            agent_seen: BTreeMap::new(),
            focused: true,
            session: Session::default(),
            engine: Engine::new(),
            theme: cfg.theme,
            cli: cli.clone(),
            watch: reload::Watch::new(),
            clipboard: None,
            mods: ModifiersState::empty(),
            preedit: String::new(),
            pointer: PhysicalPosition::new(0.0, 0.0),
            clicks: mouse::Clicks::default(),
            drag: None,
            palette: overlay::Palette::default(),
            menu: overlay::Menu::default(),
            picker: overlay::Picker::default(),
            tab_drag: None,
            focus_new_pane: false,
            hover: None,
            right_from: None,
            drag_from: Pos::default(),
            cols: 80,
            rows: 24,
            status_msg: String::new(),
            diagnose: cli.mode == CliMode::Diagnose,
            cfg,
            quit_warned: false,
            pending_pipe: None,
            macro_depth: 0,
            cwd: cli
                .cwd
                .clone()
                .or_else(|| std::env::current_dir().ok())
                .map(|p| p.to_string_lossy().into_owned()),
            command: cli.command.clone(),
        }
    }

    /// タブバーが占める行数。タブが 1 枚のときは出さない。
    fn tab_rows(&self) -> usize {
        usize::from(self.session.info.as_ref().is_some_and(|i| i.tabs.len() > 1))
    }

    fn text_rows(&self) -> usize {
        self.rows
            .saturating_sub(1 + self.tab_rows())
            .max(1)
    }

    fn area(&self) -> Rect {
        Rect {
            x: 0,
            y: self.tab_rows(),
            w: self.cols,
            h: self.text_rows(),
        }
    }

    fn active_view(&self) -> Option<&PaneView> {
        self.session.panes.get(&self.session.active)
    }

    // ---- 効果の実行 -------------------------------------------------------

    /// 🔴 `arch.md` §6.2。IME の可否はモードから機械的に決まる。ここ以外で触らない。
    fn sync_ime(&mut self) {
        let allowed = self.engine.mode().ime_allowed();
        if let Some(w) = &self.window {
            w.set_ime_allowed(allowed);
        }
        if !allowed {
            self.preedit.clear();
        }
    }

    fn run_effects(&mut self, effects: Vec<Effect>, event_loop: &ActiveEventLoop) {
        for effect in effects {
            match effect {
                Effect::ModeChanged(mode) => {
                    // 入力モードの 1 回ぶんを取り消しの 1 単位にする
                    if mode == Mode::Insert {
                        let at = self.engine.cursor();
                        if let Some(f) = self
                            .session
                            .panes
                            .get_mut(&self.session.active)
                            .and_then(|v| v.file.as_mut())
                        {
                            f.begin_group(at);
                        }
                    }
                    self.sync_ime();
                    if mode == Mode::Insert {
                        self.snap_to_live_tail();
                    }
                    self.status_msg.clear();
                }
                Effect::CursorMoved(_) => self.follow_cursor(),
                Effect::Yanked {
                    register,
                    chars,
                    lines,
                } => {
                    self.status_msg = if lines > 1 {
                        format!("\"{register} に {lines} 行をコピー")
                    } else {
                        format!("\"{register} に {chars} 文字をコピー")
                    };
                }
                Effect::SetClipboard(text) => self.set_clipboard(&text),
                Effect::Edit { range, text } => {
                    self.apply_edit(&range, &text);
                    self.push_file();
                    self.quit_warned = false;
                }
                Effect::Insert { at, text, cursor } => {
                    self.insert_text(at, &text, cursor);
                    self.push_file();
                    self.quit_warned = false;
                }
                Effect::MarkSet { .. } | Effect::MacroRecording(_) => {}
                Effect::MacroReplay(keys) => self.replay_macro(&keys, event_loop),
                Effect::Palette(prefix) => self.palette_with(&prefix),
                Effect::OpenSearch { back } => {
                    // 打つたびに飛べるよう、出発点を覚えておく。
                    // Esc で戻せないと、探し始める前の場所を失う。
                    self.search_from = Some(self.engine.cursor());
                    self.palette.show_search(back);
                }
                Effect::History(action) => self.run_history(action),
                Effect::Pipe { input } => {
                    // 範囲は決まった。あとは何に通すかを訊く。
                    self.pending_pipe = Some(input);
                    self.palette_with("> ");
                }
                Effect::File(action) => match action {
                    tsg_modal::FileAction::Save => self.save_file(None),
                    tsg_modal::FileAction::Close => self.close_file(false),
                    tsg_modal::FileAction::CloseDiscard => self.close_file(true),
                },
                Effect::SendToPrompt(text) => {
                    // `!` は挿入するだけ。Enter は押さない（modal-spec.md §7）。
                    self.snap_to_live_tail();
                    self.send_input(text.as_bytes());
                    self.status_msg = t!(format!("プロンプトへ {} 文字を送りました", text.chars().count()),
                        format!("sent {} characters to the prompt", text.chars().count()));
                }
                Effect::Scrolled(delta) => self.scroll_by(delta),
                Effect::Message(msg) => self.status_msg = msg,
                Effect::HelpToggled(_) => self.status_msg.clear(),
                Effect::SetTheme(name) => self.apply_theme(&name),
                Effect::OpenConfig => self.open_config(),
                Effect::Mux(req) => self.run_mux(req, event_loop),
                Effect::Bell => {}
                // ウィンドウを閉じても、開いているファイルはセッションに残る。
                // だから止めない（止めるのは本当に捨てるときだけ）。
                Effect::Quit => event_loop.exit(),
            }
        }
    }

    /// パレットを開いて、あらかじめ文字を入れておく。
    fn palette_with(&mut self, prefix: &str) {
        self.palette.show();
        for c in prefix.chars() {
            self.palette.push(c);
        }
    }

    fn run_mux(&mut self, req: MuxRequest, event_loop: &ActiveEventLoop) {
        if let Some(msg) = self.mux_message(req, event_loop) {
            self.send_msg(&msg);
        }
    }

    /// 配置の要求を mux のメッセージへ翻訳する。
    ///
    /// 送るものが無い要求（ペイン移動・デタッチ）はここで済ませて `None` を返す。
    fn mux_message(&mut self, req: MuxRequest, event_loop: &ActiveEventLoop) -> Option<ClientMsg> {
        let pane = self.session.active;
        match req {
            MuxRequest::Split(SplitDir::Horizontal) => {
                self.focus_new_pane = true;
                Some(ClientMsg::Split {
                    pane,
                    dir: Dir::Horizontal,
                })
            }
            MuxRequest::Split(SplitDir::Vertical) => {
                self.focus_new_pane = true;
                Some(ClientMsg::Split {
                    pane,
                    dir: Dir::Vertical,
                })
            }
            MuxRequest::ClosePane => Some(ClientMsg::ClosePane { pane }),
            MuxRequest::NewTab => Some(ClientMsg::NewTab {
                cwd: None,
                command: None,
            }),
            MuxRequest::NextTab | MuxRequest::PrevTab => {
                let info = self.session.info.as_ref()?;
                let ids: Vec<u32> = info.tabs.iter().map(|t| t.id).collect();
                if ids.is_empty() {
                    return None;
                }
                let cur = ids
                    .iter()
                    .position(|id| *id == info.active_tab)
                    .unwrap_or(0);
                let next = if req == MuxRequest::NextTab {
                    (cur + 1) % ids.len()
                } else {
                    (cur + ids.len() - 1) % ids.len()
                };
                Some(ClientMsg::SelectTab { tab: ids[next] })
            }
            MuxRequest::Focus(dir) => {
                if let Some(next) = self.session.neighbor(pane, dir) {
                    self.session.active = next;
                    self.snap_to_live_tail();
                }
                None
            }
            // どちらが隣かを知っているのは割り付けを描いている側だけなので、
            // ここで決めてから 2 枚を渡す（サーバは幾何を持たない）。
            MuxRequest::Swap(dir) => self
                .session
                .neighbor(pane, dir)
                .map(|b| ClientMsg::SwapPanes { a: pane, b }),
            MuxRequest::Zoom => self.session.active_tab().map(|tab| ClientMsg::SetZoom {
                tab,
                pane: Some(pane),
            }),
            MuxRequest::Equalize => self
                .session
                .active_tab()
                .map(|tab| ClientMsg::Equalize { tab }),
            MuxRequest::Resize(delta) => Some(ClientMsg::ResizeSplit { pane, delta }),
            MuxRequest::MoveTab(delta) => {
                let info = self.session.info.as_ref()?;
                let from = info.tabs.iter().position(|t| t.id == info.active_tab)?;
                let to = (from as isize + delta).clamp(0, info.tabs.len() as isize - 1) as usize;
                (to != from).then_some(ClientMsg::MoveTab {
                    tab: info.active_tab,
                    to,
                })
            }
            MuxRequest::Sessions => {
                self.show_sessions();
                None
            }
            MuxRequest::NextAgent => {
                self.jump_to_waiting_agent();
                None
            }
            MuxRequest::PaneFiles => {
                self.show_paths();
                None
            }
            MuxRequest::TogglePreview => {
                self.toggle_preview();
                None
            }
            MuxRequest::Hints => {
                self.show_hints();
                None
            }
            MuxRequest::GitDiff => {
                self.show_git_diff();
                None
            }
            MuxRequest::Broadcast => {
                // パレットの入力欄を借りる。**入力の窓を 2 種類作らない。**
                self.palette_with("b ");
                self.status_msg = t!(
                    "全部のペインへ投げる文を打って Enter",
                    "type one prompt for every pane, then Enter"
                )
                .into();
                None
            }
            MuxRequest::ToggleFold => {
                self.toggle_fold_here();
                None
            }
            MuxRequest::FoldAll(on) => {
                self.fold_all(on);
                None
            }
            MuxRequest::Detach => {
                // プロセスもファイルも生かしたままウィンドウを閉じる。
                self.send_msg(&ClientMsg::Detach);
                event_loop.exit();
                None
            }
            MuxRequest::Shutdown => {
                // ここだけは本当に捨てる。未保存があれば 1 度止める。
                if !self.unsaved().is_empty() && !self.quit_warned {
                    self.warn_unsaved();
                    return None;
                }
                self.send_msg(&ClientMsg::Shutdown);
                event_loop.exit();
                None
            }
        }
    }

    /// 走っているセッションを並べる（`Space S`）。
    fn show_sessions(&mut self) {
        let mut names = tsg_mux::sessions::live();
        if !names.contains(&self.session_name) {
            names.push(self.session_name.clone());
            names.sort_unstable();
        }
        self.menu.hide();
        self.palette.hide();
        self.picker.show(
            t!("セッション（Enter で切り替え）", "Sessions (Enter to switch)"),
            names,
            overlay::PickKind::Session,
        );
    }

    /// 別のセッションへ乗り換える。**今のセッションは殺さない**（デタッチ）。
    fn switch_session(&mut self, name: &str) {
        if name == self.session_name {
            self.status_msg = t!(format!("すでに {name} にいます"), format!("already in {name}"));
            return;
        }
        self.send_msg(&ClientMsg::Detach);
        self.client = None;
        self.session = Session::default();
        self.session_name = name.to_string();
        match connect_or_spawn(name) {
            Ok(client) => {
                self.client = Some(client);
                let area = self.area();
                self.send_msg(&ClientMsg::Attach {
                    version: PROTOCOL_VERSION,
                    cols: area.w as u16,
                    rows: area.h as u16,
                    cwd: self.cwd.clone(),
                    command: None,
                });
                self.status_msg = t!(format!("{name} に移りました"), format!("switched to {name}"));
            }
            Err(e) => self.status_msg = t!(format!("{name} へ繋げません: {e:#}"), format!("cannot reach {name}: {e:#}")),
        }
    }

    /// 範囲を置き換える。相手がファイルか端末かを知っているのはここだけ。
    ///
    /// 端末では**表示から消すだけ**でプロセスには何もしない
    /// （`modal-spec.md` §7 の `d` の定義）。サーバの控えは無傷なので、
    /// 再アタッチすれば戻る。
    fn apply_edit(&mut self, range: &Range, text: &str) {
        let active = self.session.active;
        let at = self.engine.cursor();
        let Some(view) = self.session.panes.get_mut(&active) else {
            return;
        };
        if let Some(file) = view.file.as_mut() {
            // オペレータ 1 回で取り消し 1 段
            file.begin_group(at);
            file.replace(range, text);
            return;
        }

        // 行を消すときだけ `state` 越しにする。印を一緒に動かす必要があるのは
        // 行の増減があるときだけで、桁を空白にする道は印に影響しない。
        if range.kind == RangeKind::Line {
            view.term
                .state
                .remove_document_lines(range.start.line, range.end.line);
            return;
        }
        let grid = &mut view.term.state.grid;
        match range.kind {
            RangeKind::Line => unreachable!("上で返している"),
            RangeKind::Block => {
                let (a, b) = (
                    range.start.col.min(range.end.col),
                    range.start.col.max(range.end.col),
                );
                for line in range.start.line..=range.end.line {
                    grid.blank_document_cells(line, a, b);
                }
            }
            RangeKind::Char => {
                if range.start.line == range.end.line {
                    grid.blank_document_cells(range.start.line, range.start.col, range.end.col);
                } else {
                    grid.blank_document_cells(range.start.line, range.start.col, usize::MAX);
                    for line in range.start.line + 1..range.end.line {
                        grid.blank_document_cells(line, 0, usize::MAX);
                    }
                    grid.blank_document_cells(range.end.line, 0, range.end.col);
                }
            }
        }
    }

    /// レジスタの中身を差し込む。
    ///
    /// 端末では `p` が `SendToPrompt` に化けるので、ここへ来るのはファイルだけ。
    fn insert_text(&mut self, at: Pos, text: &str, cursor: Option<Pos>) {
        let active = self.session.active;
        let group_at = self.engine.cursor();
        let Some(view) = self.session.panes.get_mut(&active) else {
            return;
        };
        let Some(file) = view.file.as_mut() else {
            return;
        };
        // 貼り付け 1 回で取り消し 1 段
        file.begin_group(group_at);
        let end = file.insert(at, text);
        // 行き先を決めるのはエンジン。ここで場合分けすると、貼り方を
        // 増やすたびに条件が増える。
        let next = file.clamp(cursor.unwrap_or(end));
        let buf = view.buffer();
        self.engine.set_cursor(next, &buf);
    }

    /// 記録したキー列を流し直す。
    ///
    /// **1 キーごとにバッファを取り直す。** まとめて回すと、マクロ内の編集が
    /// 次のキーに反映されず `dd` の 2 回目が消えた行を見に行く。
    fn replay_macro(&mut self, keys: &[KeyInput], event_loop: &ActiveEventLoop) {
        if self.macro_depth >= 8 {
            self.status_msg = t!("マクロの入れ子が深すぎます", "macro nested too deep").into();
            return;
        }
        self.macro_depth += 1;
        for key in keys {
            self.update_view();
            let outcome = {
                let Some(view) = self.session.panes.get(&self.session.active) else {
                    break;
                };
                let buf = view.buffer();
                self.engine.key(*key, &buf)
            };
            match outcome {
                KeyOutcome::Handled(effects) => self.run_effects(effects, event_loop),
                KeyOutcome::PassThrough => self.replay_passthrough(*key),
            }
        }
        self.macro_depth -= 1;
    }

    /// 記録した入力モードの打鍵を、キーボードから打ったのと同じ道へ戻す。
    fn replay_passthrough(&mut self, input: KeyInput) {
        let (key, text) = match input {
            KeyInput::Char(c) => (Key::Character(c.to_string().into()), Some(c.to_string())),
            KeyInput::Enter => (Key::Named(NamedKey::Enter), None),
            KeyInput::Backspace => (Key::Named(NamedKey::Backspace), None),
            KeyInput::Tab => (Key::Named(NamedKey::Tab), None),
            KeyInput::Esc => (Key::Named(NamedKey::Escape), None),
            KeyInput::Ctrl(_) | KeyInput::Function(_) => return,
        };
        if self.active_view().is_some_and(PaneView::editing) {
            self.type_into_file(&key, text.as_deref());
            return;
        }
        let app_cursor = self
            .active_view()
            .is_some_and(|v| v.term.state.modes.app_cursor_keys);
        if let Some(bytes) = input::encode(&key, text.as_deref(), ModifiersState::empty(), app_cursor)
        {
            self.send_input(&bytes);
        }
    }

    /// `>` の相手を実行して、結果を新しいペインで開く。
    ///
    /// プロセスを起こすのはここ。PTY と違って一度きりの実行なので、
    /// サーバへ持たせずクライアントで走らせ、**結果だけ**セッションへ預ける。
    fn run_pipe(&mut self, command: &str) {
        let Some(input) = self.pending_pipe.take() else {
            return;
        };
        let command = command.trim();
        if command.is_empty() {
            self.status_msg = t!("通すコマンドがありません", "no command to pipe through").into();
            return;
        }

        let cwd = self
            .active_view()
            .and_then(|v| v.term.state.cwd.clone())
            .and_then(|u| file_url_to_path(&u))
            .or_else(|| self.cwd.clone());

        match pipe_through(command, &input, cwd.as_deref()) {
            Ok(output) if output.trim().is_empty() => {
                self.status_msg = t!(format!("{command} は何も返しませんでした"), format!("{command} returned nothing"));
            }
            Ok(output) => {
                let pane = self.session.active;
                self.send_msg(&ClientMsg::PipeResult {
                    pane,
                    dir: Dir::Horizontal,
                    title: format!("> {command}"),
                    text: output,
                });
            }
            Err(e) => self.status_msg = t!(format!("{command} を実行できません: {e}"), format!("cannot run {command}: {e}")),
        }
    }

    /// 取り消し / やり直し。
    ///
    /// 端末の表示は履歴を持たない。`d` で消したものはサーバ側の控えに残っているので、
    /// 取り消しではなく**入り直せば戻る**。そう言う。
    fn run_history(&mut self, action: tsg_modal::HistoryAction) {
        let active = self.session.active;
        let Some(view) = self.session.panes.get_mut(&active) else {
            return;
        };
        let Some(file) = view.file.as_mut() else {
            self.status_msg = t!(
                "端末の表示は取り消せません（消した行はセッションに入り直すと戻ります）",
                "the terminal view has no undo (reattach to get removed lines back)"
            )
            .into();
            return;
        };
        let (moved, verb) = match action {
            tsg_modal::HistoryAction::Undo => (file.undo(), t!("取り消し", "undo")),
            tsg_modal::HistoryAction::Redo => (file.redo(), t!("やり直し", "redo")),
        };
        match moved {
            Some(pos) => {
                let buf = view.buffer();
                self.engine.set_cursor(pos, &buf);
                self.status_msg = t!(format!("{verb}ました"), format!("{verb} done"));
                self.quit_warned = false;
                self.push_file();
            }
            None => {
                self.status_msg = t!(
                    format!("これ以上{verb}できません"),
                    format!("nothing left to {verb}")
                );
            }
        }
    }

    /// ファイルを開く。**中身を持つのはサーバ**なので、頼んで返事を待つ。
    ///
    /// パスはここで解決して絶対パスにする。サーバは常駐で、
    /// クライアントがどこから起動されたかを知らないため。
    fn open_file(&mut self, path: &str) {
        let full = self
            .cwd
            .as_ref()
            .map(|c| std::path::Path::new(c).join(path))
            .unwrap_or_else(|| std::path::PathBuf::from(path));
        let pane = self.session.active;
        self.send_msg(&ClientMsg::OpenFile {
            pane,
            path: full.display().to_string(),
        });
    }

    /// 編集をサーバへ預ける。これを忘れるとウィンドウを閉じた時点で消える。
    ///
    /// **普段は差分だけ送る。** 打鍵ごとに全文を流すと、大きなファイルでは
    /// 1 文字打つたびにファイル長ぶんが socket を通ることになる。
    /// 取り消しの後だけは全文を渡し直す（何段でも一度に動くため）。
    fn push_file(&mut self) {
        let pane = self.session.active;
        let Some(file) = self
            .session
            .panes
            .get_mut(&pane)
            .and_then(|v| v.file.as_mut())
        else {
            return;
        };
        let splices = file.take_splices();
        let resync = file.take_resync();
        if resync {
            let text = file.text();
            self.send_msg(&ClientMsg::SetFile { pane, text });
            return;
        }
        if splices.is_empty() {
            return;
        }
        // 当てる前のサーバ側の長さ。**当てた後の長さから逆算する**ので、
        // ここで数え違えると受け側が黙って壊れる（だから受け側でも確かめる）。
        let after = file.text().len();
        let delta: isize = splices
            .iter()
            .map(|s| s.inserted.len() as isize - s.removed.len() as isize)
            .sum();
        let base_len = (after as isize - delta).max(0) as usize;
        let edits: Vec<tsg_mux::Edit> = splices
            .into_iter()
            .map(|s| tsg_mux::Edit {
                start: s.start,
                remove: s.removed.len(),
                insert: s.inserted,
            })
            .collect();
        self.send_msg(&ClientMsg::EditFile {
            pane,
            base_len,
            edits,
        });
    }

    /// サーバが差分を当てられなかった。全文を渡し直す。
    fn resend_file(&mut self, pane: u32) {
        let Some(text) = self
            .session
            .panes
            .get_mut(&pane)
            .and_then(|v| v.file.as_mut())
            .map(|f| {
                f.take_splices();
                f.take_resync();
                f.text()
            })
        else {
            return;
        };
        self.send_msg(&ClientMsg::SetFile { pane, text });
    }

    fn save_file(&mut self, path: Option<String>) {
        let pane = self.session.active;
        if self
            .session
            .panes
            .get(&pane)
            .is_none_or(|v| v.file.is_none())
        {
            self.status_msg = t!("このペインはファイルではありません", "this pane is not a file").into();
            return;
        }
        // 先に今の中身を渡してから書かせる（取りこぼしを作らない）
        self.push_file();
        self.send_msg(&ClientMsg::SaveFile { pane, path });
    }

    /// エディタを閉じて端末へ戻る。下のシェルは走ったままなのですぐ続けられる。
    fn close_file(&mut self, force: bool) {
        let pane = self.session.active;
        let dirty = self
            .session
            .panes
            .get(&pane)
            .and_then(|v| v.file.as_ref())
            .is_some_and(|f| f.dirty);
        if dirty && !force {
            self.status_msg = t!("保存していません（:w で保存、:q! で捨てる）", "unsaved (:w to save, :q! to discard)").into();
            return;
        }
        self.send_msg(&ClientMsg::CloseFile { pane });
    }

    /// 保存していないファイルを開いているペイン。
    ///
    /// 中身はサーバが預かるのでウィンドウを閉じても消えないが、
    /// **セッションごと終了すれば消える**。そこだけは止める。
    fn unsaved(&self) -> Vec<String> {
        self.session
            .panes
            .values()
            .filter(|v| v.file.as_ref().is_some_and(|f| f.dirty))
            .filter_map(PaneView::label)
            .collect()
    }

    /// 未保存を知らせて 1 回だけ止める。
    ///
    /// `dirty` は落とさない（落とすと表示から `*` が消えて、次は黙って捨てることになる）。
    /// 代わりに「一度警告した」ことだけを覚える。警告して二度と閉じられない、が一番困る。
    fn warn_unsaved(&mut self) {
        let names = self.unsaved().join(" ");
        self.status_msg = t!(format!("保存していません: {names}（:w で保存、もう一度で破棄）"),
            format!("unsaved: {names} (:w to save, or repeat to discard)"));
        self.quit_warned = true;
    }

    fn set_clipboard(&mut self, text: &str) {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        if let Some(cb) = &mut self.clipboard
            && let Err(e) = cb.set_text(text.to_string())
        {
            self.status_msg = t!(format!("クリップボードへ書けません: {e}"), format!("cannot write to the clipboard: {e}"));
        }
    }

    /// 生きている末尾（シェルのカーソル）へ寄せる。
    ///
    /// **ファイルを開いているペインでは何もしない。** そこに「生きた末尾」は無く、
    /// 下で走っているシェルのカーソル位置へ飛ばすことになる。
    /// これを見落として、エディタで `A` を押すとシェルのカーソル行へ
    /// 飛ぶという不可解な動きをしていた（実機で気づいた）。
    fn snap_to_live_tail(&mut self) {
        let active = self.session.active;
        let Some(view) = self.session.panes.get(&active) else {
            return;
        };
        if view.editing() {
            return;
        }
        let pos = Pos::new(
            view.term.state.grid.cursor_absolute(),
            view.term.state.grid.cursor.col,
        );
        let buf = view.buffer();
        self.engine.set_cursor(pos, &buf);
        if let Some(v) = self.session.panes.get_mut(&active) {
            v.follow_tail = true;
        }
    }

    fn scroll_by(&mut self, delta: isize) {
        let active = self.session.active;
        if let Some(view) = self.session.panes.get_mut(&active) {
            let height = view.rect.h.max(1);
            let doc = view.doc_len();
            let max_top = doc.saturating_sub(height);
            let next = view.top as isize + delta;
            view.top = next.clamp(0, max_top as isize) as usize;
            view.follow_tail = view.top >= max_top;
        }
    }

    fn follow_cursor(&mut self) {
        if self.engine.mode() == Mode::Insert {
            return;
        }
        let line = self.engine.cursor().line;
        let active = self.session.active;
        let Some(view) = self.session.panes.get_mut(&active) else {
            return;
        };
        let height = view.rect.h.max(1);
        if line < view.top {
            view.top = line;
        } else if view.row_of(line, height).is_none() {
            // 畳んだ分を数に入れて下端を決める。**畳んでいるのに
            // 畳む前の行数でずらすと、行き先を通り越す。**
            view.top = line + 1 - height;
        }
        view.follow_tail = view.top + height >= view.doc_len();
    }

    fn update_view(&mut self) {
        let active = self.session.active;
        if let Some(view) = self.session.panes.get_mut(&active) {
            let height = view.rect.h.max(1);
            if view.follow_tail {
                view.top = view.doc_len().saturating_sub(height);
            }
            self.engine.set_view(View {
                top: view.top,
                height,
            });
        }
    }

    // ---- mux とのやりとり -------------------------------------------------

    fn send_msg(&mut self, msg: &ClientMsg) {
        if let Some(c) = &mut self.client
            && let Err(e) = c.send(msg)
        {
            self.status_msg = t!(format!("サーバへ送れません: {e}"), format!("cannot reach the server: {e}"));
        }
    }

    fn send_input(&mut self, bytes: &[u8]) {
        let pane = self.session.active;
        self.send_msg(&ClientMsg::Input {
            pane,
            data: tsg_mux::encode_bytes(bytes),
        });
        if let Some(v) = self.session.panes.get_mut(&pane) {
            v.follow_tail = true;
        }
    }

    /// サーバからのメッセージを取り込む。何か来ていれば true。
    fn pump(&mut self) -> bool {
        let mut got = false;
        while let Some(msg) = self.client.as_ref().and_then(Client::try_recv) {
            got = true;
            match msg {
                ServerMsg::Attached { session, .. } => {
                    let active = session
                        .tabs
                        .iter()
                        .find(|t| t.id == session.active_tab)
                        .map(|t| t.active_pane)
                        .unwrap_or(0);
                    self.session.info = Some(session);
                    self.session.active = active;
                    self.sync_layout();
                    // 起動時の配置は**繋がってから**組む。ペインが 1 枚
                    // 在るところから始めないと、割る相手が決まらない。
                    if self.pending_splits > 0 {
                        self.pending_splits -= 1;
                        self.send_msg(&ClientMsg::Split {
                            pane: active,
                            dir: Dir::Horizontal,
                        });
                    }
                }
                ServerMsg::Layout(info) => {
                    let fallback = info
                        .tabs
                        .iter()
                        .find(|t| t.id == info.active_tab)
                        .map(|t| t.active_pane)
                        .unwrap_or(self.session.active);
                    let before = self.session.visible_panes();
                    self.session.info = Some(info);
                    // 自分で割ったなら、増えた方へ移る。割った直後に打った文字が
                    // 元のペインへ行くのは、他のどの端末とも違う驚き方をする。
                    if self.focus_new_pane
                        && let Some(new) = self
                            .session
                            .visible_panes()
                            .into_iter()
                            .find(|id| !before.contains(id))
                    {
                        self.focus_new_pane = false;
                        self.session.active = new;
                        self.snap_to_live_tail();
                    }
                    if !self.session.visible_panes().contains(&self.session.active) {
                        self.session.active = fallback;
                    }
                    self.sync_layout();
                    self.sync_previews();
                    self.notice_agent_changes();
                    // 残りの分割。1 枚ずつ増やす（返事を待ってから次を頼む）。
                    if self.pending_splits > 0 {
                        self.pending_splits -= 1;
                        let pane = self.session.active;
                        self.send_msg(&ClientMsg::Split {
                            pane,
                            dir: Dir::Horizontal,
                        });
                        if self.pending_splits == 0 {
                            self.status_msg = t!(
                                "3 分割で開きました。Space b で全部へ同じ指示を投げられます",
                                "opened three panes; Space b sends one prompt to all of them"
                            )
                            .into();
                        }
                    }
                }
                ServerMsg::Snapshot { pane, lines, .. } => {
                    let area = self.area();
                    let (w, h) = self
                        .session
                        .panes
                        .get(&pane)
                        .map(|v| (v.rect.w.max(1), v.rect.h.max(1)))
                        .unwrap_or((area.w, area.h));
                    self.session
                        .panes
                        .entry(pane)
                        .or_insert_with(|| PaneView::new(w, h))
                        .restore(&lines, w, h);
                }
                ServerMsg::FileState {
                    pane,
                    path,
                    title,
                    text,
                    dirty,
                } => {
                    let area = self.area();
                    let view = self
                        .session
                        .panes
                        .entry(pane)
                        .or_insert_with(|| PaneView::new(area.w, area.h));
                    let mut file = tsg_modal::FileBuffer::from_text(&text, tsg_term::ambiguous());
                    file.path = path.as_deref().map(std::path::PathBuf::from);
                    file.dirty = dirty;
                    view.file = Some(file);
                    view.title = title.clone();
                    view.top = 0;
                    view.follow_tail = false;
                    // 開いた直後のカーソルは文書の頭。端末に居たときの位置を
                    // そのまま持ち込むと、`A` が思わぬ行に効く（実機で踏んだ）。
                    let buf = view.buffer();
                    self.engine.set_cursor(Pos::default(), &buf);
                    self.session.active = pane;
                    got = true;
                    self.status_msg = t!(format!("{title} を開きました"), format!("opened {title}"));
                }
                ServerMsg::FileSaved { pane, path } => {
                    if let Some(f) = self
                        .session
                        .panes
                        .get_mut(&pane)
                        .and_then(|v| v.file.as_mut())
                    {
                        f.dirty = false;
                    }
                    self.status_msg = t!(format!("{path} を保存しました"), format!("saved {path}"));
                    got = true;
                }
                ServerMsg::NeedFullFile { pane } => {
                    // 差分がずれた。黙って進まず、全文で立て直す。
                    self.resend_file(pane);
                }
                ServerMsg::FileClosed { pane } => {
                    if let Some(view) = self.session.panes.get_mut(&pane) {
                        view.file = None;
                        view.preview = None;
                        view.follow_tail = true;
                    }
                    self.status_msg = t!("端末に戻りました", "back to the terminal").into();
                    got = true;
                }
                ServerMsg::Resized { pane, cols, rows } => {
                    // 鏡を広げるのはここだけ。ウィンドウのリサイズ時に自分で広げると、
                    // 古い桁で組まれたバイトを新しい桁で読んで表示がずれる（実機で再現した）。
                    if let Some(v) = self.session.panes.get_mut(&pane) {
                        v.term.state.grid.resize(cols as usize, rows as usize);
                    }
                }
                ServerMsg::Output { pane, data } => {
                    if let Some(bytes) = tsg_mux::decode_bytes(&data) {
                        let area = self.area();
                        self.session
                            .panes
                            .entry(pane)
                            .or_insert_with(|| PaneView::new(area.w, area.h))
                            .term
                            .feed(&bytes);
                    }
                }
                ServerMsg::PaneExited { pane } => {
                    if let Some(v) = self.session.panes.get_mut(&pane) {
                        v.alive = false;
                    }
                    self.status_msg = t!(format!("ペイン {pane} のシェルが終了しました"), format!("the shell in pane {pane} exited"));
                }
                ServerMsg::Pong => {}
                ServerMsg::Error { message } => self.status_msg = message,
            }
        }
        got
    }

    /// レイアウトを割り付け直し、各ペインのサイズをサーバへ伝える。
    fn sync_layout(&mut self) {
        let area = self.area();
        for id in self.session.visible_panes() {
            self.session
                .panes
                .entry(id)
                .or_insert_with(|| PaneView::new(area.w, area.h));
        }
        self.session.assign_rects(area);

        let sizes: Vec<(u32, u16, u16)> = self
            .session
            .visible_panes()
            .into_iter()
            .filter_map(|id| {
                let v = self.session.panes.get(&id)?;
                let text = v.text_rect();
                Some((id, text.w.max(1) as u16, text.h.max(1) as u16))
            })
            .collect();
        for (pane, cols, rows) in sizes {
            // 鏡はここでは触らない。ConPTY が実際にサイズを変えた位置で
            // `ServerMsg::Resized` が届くので、合わせるのはそのとき。
            self.send_msg(&ClientMsg::Resize { pane, cols, rows });
        }
        self.sync_previews();
    }


    // ---- マウス -----------------------------------------------------------
    //
    // `mouse-parity.md` §3 のポインタ語彙。どの操作も最後は `Command` になり、
    // キーボードと同じディスパッチャを通る（`arch.md` の不変条件 1）。

    /// 物理ピクセル -> セル座標。
    fn cell_at(&self, p: PhysicalPosition<f64>) -> (usize, usize) {
        let (cw, ch) = self
            .renderer
            .as_ref()
            .map_or((1.0, 1.0), Renderer::cell_size);
        let col = (p.x.max(0.0) / f64::from(cw.max(1.0))) as usize;
        let row = (p.y.max(0.0) / f64::from(ch.max(1.0))) as usize;
        (
            col.min(self.cols.saturating_sub(1)),
            row.min(self.rows.saturating_sub(1)),
        )
    }

    /// ペイン内のセル座標 -> ドキュメント位置。
    fn doc_pos(&self, id: u32, col: usize, row: usize) -> Option<Pos> {
        let v = self.session.panes.get(&id)?;
        let text = v.text_rect();
        Some(Pos::new(
            v.line_at(row.saturating_sub(text.y)),
            col.saturating_sub(text.x),
        ))
    }

    /// ガターの上か。
    fn on_gutter(&self, id: u32, col: usize) -> bool {
        self.session
            .panes
            .get(&id)
            .is_some_and(|v| col < v.rect.x + GUTTER)
    }

    /// ガターのクリック。`mouse-parity.md` §4.3。
    ///
    /// 1 回でコマンドブロック全体（`ac`）、2 回で出力だけ（`io`）。
    /// キーボードの `vac` / `vio` と同じ `textobj` を通る。
    fn on_gutter_click(&mut self, id: u32, col: usize, row: usize, event_loop: &ActiveEventLoop) {
        let Some(pos) = self.doc_pos(id, 0, row) else {
            return;
        };
        // ガターの右半分は印の欄。印が出ている行なら、そこへ飛ぶ
        // （`mouse-parity.md` §4.2 の「ガターのマーク印をクリック」）。
        let mark_col = self.session.panes.get(&id).map_or(1, |v| v.rect.x + 1);
        if col >= mark_col
            && let Some(name) = self.engine.marks.at_line(pos.line)
        {
            self.dispatch(
                tsg_modal::Command::JumpMark { name, exact: true },
                event_loop,
            );
            self.status_msg = t!(format!("マーク {name} へ飛びました"), format!("jumped to mark {name}"));
            return;
        }
        let clicks = self.clicks.press((usize::MAX, pos.line), Instant::now());
        let (obj, around) = if clicks >= 2 {
            (tsg_modal::TextObject::OutputBlock, false)
        } else {
            (tsg_modal::TextObject::CommandBlock, true)
        };

        let range = {
            let Some(view) = self.session.panes.get(&id) else {
                return;
            };
            let buf = view.buffer();
            tsg_modal::textobj::range_of(&buf, pos, obj, around)
        };
        match range {
            Some(range) => {
                self.dispatch(tsg_modal::Command::Select { range }, event_loop);
                self.status_msg = if clicks >= 2 {
                    t!("出力だけを選びました（y でコピー）", "selected the output (y to copy)").into()
                } else {
                    t!(
                        "コマンドと出力を選びました（y でコピー）",
                        "selected the command and its output (y to copy)"
                    )
                    .into()
                };
            }
            None => self.status_msg = t!("この行にコマンドブロックがありません", "no command block on this line").into(),
        }
    }

    /// タブバーの各タブが占める列。描画とヒット判定で同じものを使う。
    fn tab_spans(&self) -> Vec<(usize, usize, u32, String, bool)> {
        let Some(info) = self.session.info.as_ref() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut x = 0usize;
        for (n, t) in info.tabs.iter().enumerate() {
            let title = self
                .session
                .panes
                .get(&t.active_pane)
                .map(|v| v.term.state.title.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("タブ{}", n + 1));
            // 状態の印はタブ名の前。**名前を読む前に目に入る**位置にする。
            let mark = match self.tab_agent(t.id) {
                Some(AgentState::Blocked) => "● ",
                Some(AgentState::Failed) => "✕ ",
                Some(AgentState::Done) => "✓ ",
                Some(AgentState::Working) => "◍ ",
                _ => "",
            };
            let label = format!(" {} {}{} ", n + 1, mark, truncate_width(&title, 18));
            let w = display_width(&label);
            if x + w > self.cols {
                break;
            }
            out.push((x, w, t.id, label, t.id == info.active_tab));
            x += w;
        }
        out
    }

    /// Markdown を「読む形」と「素のまま」で行き来する。
    ///
    /// **写しを作るだけ**で、元のファイルには触らない。読む形は色付きの
    /// 文字列にすぎないので、端末に食わせてセルにしてもらう。描画・選択・
    /// コピー・Ctrl＋クリックは、ふつうのペインとまったく同じ経路に乗る。
    fn toggle_preview(&mut self) {
        let pane = self.session.active;
        if !self.session.panes.get(&pane).is_some_and(PaneView::editing) {
            self.status_msg = t!(
                "ファイルを開いてから使ってください（Space e）",
                "open a file first (Space e)"
            )
            .into();
            return;
        }
        // **決めるのはサーバ**。開き直したときに素のテキストへ戻ると、
        // 読んでいた場所も見え方も失う。開いているファイルと同じ扱いにする。
        self.send_msg(&ClientMsg::SetPreview {
            pane: Some(pane),
            on: None,
        });
    }

    /// サーバが言う「読む形かどうか」に写しを合わせる。
    ///
    /// 組み立てはここ 1 か所。幅が変わったときの組み直しも同じ道を通る。
    fn sync_previews(&mut self) {
        let want: Vec<(u32, bool)> = self
            .session
            .info
            .as_ref()
            .map(|i| i.panes.iter().map(|p| (p.id, p.preview)).collect())
            .unwrap_or_default();
        for (id, on) in want {
            let Some(view) = self.session.panes.get_mut(&id) else {
                continue;
            };
            if !on {
                if view.preview.take().is_some() {
                    view.top = 0;
                }
                continue;
            }
            let width = view.text_rect().w.max(20);
            let fresh = view
                .preview
                .as_ref()
                .is_some_and(|p| p.state.grid.cols == width);
            if fresh {
                continue;
            }
            let Some(text) = view.file.as_ref().map(tsg_modal::FileBuffer::text) else {
                continue;
            };
            let rendered = tsg_modal::markdown::render(&text, width);
            let rows = rendered.lines().count().max(1);
            let mut term = tsg_term::Terminal::new(width, rows, tsg_term::ambiguous());
            term.state.grid.set_max_scrollback(rows + 8);
            term.feed(rendered.as_bytes());
            view.preview = Some(term);
            view.top = 0;
        }
    }

    /// このペインに出てきたファイルパスを集めて一覧にする。
    ///
    /// エージェントは「どのファイルを読んで、どこを直したか」を字で言う。
    /// **その字はもう画面に在る**ので、集めて並べれば、触られたファイルの
    /// 一覧になる。エージェントごとの出力の形に依存しないのが要点で、
    /// `Read(...)` のような書式を覚え込ませるとその日のうちに古くなる。
    fn show_paths(&mut self) {
        let Some(view) = self.active_view() else {
            return;
        };
        let buf = view.buffer();
        let mut seen: Vec<String> = Vec::new();
        for line in 0..buf.line_count() {
            let Some(cells) = buf.cells(line) else {
                continue;
            };
            let text: String = cells.iter().map(|c| c.text.as_str()).collect();
            for word in text.split_whitespace() {
                // 前後の飾り（引用符・括弧・句読点）を落としてから見る。
                let w = word.trim_matches(|c: char| {
                    "\"'`（）()[]{}<>、。,;:！!？?".contains(c)
                });
                if !matches!(open_kind(w), Some(OpenKind::Path)) {
                    continue;
                }
                let w = w.to_string();
                if !seen.contains(&w) {
                    seen.push(w);
                }
            }
        }
        if seen.is_empty() {
            self.status_msg = t!(
                "この画面にファイルパスはありません",
                "no file paths on this screen"
            )
            .into();
            return;
        }
        self.picker.show(
            t!(
                "この画面に出てきたファイル（Enter で開く）",
                "Files on this screen (Enter to open)"
            ),
            seen,
            overlay::PickKind::Path,
        );
    }

    /// 状態が変わったら知らせる。**変わった瞬間だけ**。
    ///
    /// ずっと返事待ちのペインを見るたびに光らせても、すぐ無視するようになる。
    /// 窓が前に居るときは何もしない（タブの印を見れば分かる）。
    fn notice_agent_changes(&mut self) {
        let Some(info) = self.session.info.as_ref() else {
            return;
        };
        let now: Vec<(u32, Option<AgentState>)> = info
            .panes
            .iter()
            .map(|p| (p.id, self.agent_state(p.id)))
            .collect();
        let mut call = false;
        for (id, state) in now {
            let Some(state) = state else {
                self.agent_seen.remove(&id);
                continue;
            };
            let before = self.agent_seen.insert(id, state);
            if before != Some(state) && state.wants_you() {
                call = true;
            }
        }
        // 開いているペインが消えたぶんを落とす
        let live: Vec<u32> = self
            .session
            .info
            .as_ref()
            .map(|i| i.panes.iter().map(|p| p.id).collect())
            .unwrap_or_default();
        self.agent_seen.retain(|id, _| live.contains(id));

        if call
            && !self.focused
            && let Some(w) = &self.window
        {
            platform::attention(w.as_ref());
        }
    }

    /// 次の「人の番」のエージェントへ飛ぶ。別のタブに居れば、そのタブごと。
    ///
    /// **探すのを人にやらせない。** 3 本並べて放っておくと、どれが止まって
    /// いるのかを毎回目で探すことになる。それが要らないのがこの端末の理由。
    fn jump_to_waiting_agent(&mut self) {
        let waiting = self.panes_wanting_you();
        if waiting.is_empty() {
            self.status_msg = t!(
                "返事を待っているエージェントはありません",
                "no agent is waiting for you"
            )
            .into();
            return;
        }
        let Some(next) = Self::next_waiting(&waiting, self.session.active) else {
            return;
        };

        // 別のタブなら、タブごと移る。
        if let Some(info) = self.session.info.as_ref()
            && let Some(tab) = info
                .tabs
                .iter()
                .find(|t| t.layout.panes().contains(&next))
            && tab.id != info.active_tab
        {
            let id = tab.id;
            self.send_msg(&ClientMsg::SelectTab { tab: id });
        }
        self.session.active = next;
        self.snap_to_live_tail();
        let state = self.agent_state(next);
        self.status_msg = match state {
            Some(AgentState::Blocked) => t!("返事待ち", "waiting for you").into(),
            Some(AgentState::Failed) => t!("失敗して止まっています", "stopped with an error").into(),
            _ => t!("終わっています", "finished").into(),
        };
    }

    /// そのペインのエージェントの状態。
    ///
    /// **名乗ったものが最優先。** 画面を読んで当てにいくと、エージェントが
    /// 出力の形を変えた日に黙って壊れる。名乗らないペイン（ただのシェル）は
    /// シェル統合（OSC 133）から分かる範囲だけを見る。どちらも無ければ `None`。
    fn agent_state(&self, pane: u32) -> Option<AgentState> {
        if let Some(s) = self
            .session
            .info
            .as_ref()
            .and_then(|i| i.panes.iter().find(|p| p.id == pane))
            .and_then(|p| p.agent)
        {
            return Some(s);
        }
        let view = self.session.panes.get(&pane)?;
        let last = view.term.state.marks.blocks().pop()?;
        if last.is_running() {
            return Some(AgentState::Working);
        }
        None
    }

    /// 人の番になっているペイン。タブの印・ジャンプ・通知が全部これを見る。
    fn panes_wanting_you(&self) -> Vec<u32> {
        let Some(info) = self.session.info.as_ref() else {
            return Vec::new();
        };
        info.panes
            .iter()
            .filter(|p| self.agent_state(p.id).is_some_and(AgentState::wants_you))
            .map(|p| p.id)
            .collect()
    }

    /// タブに出す印。**一番強い状態を出す。** 3 つ並んでいて 1 つが返事待ちなら、
    /// そのタブは返事待ちとして見えないと意味がない。
    fn tab_agent(&self, tab: u32) -> Option<AgentState> {
        let info = self.session.info.as_ref()?;
        let t = info.tabs.iter().find(|t| t.id == tab)?;
        let mut best: Option<AgentState> = None;
        for pane in t.layout.panes() {
            let Some(s) = self.agent_state(pane) else {
                continue;
            };
            let rank = |a: AgentState| match a {
                AgentState::Failed => 4,
                AgentState::Blocked => 3,
                AgentState::Done => 2,
                AgentState::Working => 1,
                AgentState::Idle => 0,
            };
            if best.is_none_or(|b| rank(s) > rank(b)) {
                best = Some(s);
            }
        }
        best
    }

    /// 🔴 マウスの所有者。`concept.md` の所有権モデル。判断は `tsg-term` にしか無い。
    fn mouse_goes_to_child(&self) -> bool {
        self.active_view()
            .is_some_and(|v| v.term.state.mouse_owner() == InputOwner::Child)
    }

    /// 子プロセスへマウスレポートを流す。
    fn forward_mouse(
        &mut self,
        button: mouse::Button,
        phase: mouse::Phase,
        col: usize,
        row: usize,
    ) -> bool {
        let Some(view) = self.active_view() else {
            return false;
        };
        let (tracking, encoding) = (view.term.state.modes.mouse, view.term.state.modes.mouse_encoding);
        let rect = view.rect;
        let bits = mouse::modifier_bits(
            self.mods.shift_key(),
            self.mods.alt_key(),
            self.mods.control_key(),
        );
        let Some(bytes) = mouse::report(
            tracking,
            encoding,
            button,
            phase,
            col.saturating_sub(rect.x),
            row.saturating_sub(rect.y),
            bits,
        ) else {
            return false;
        };
        self.send_input(&bytes);
        true
    }

    /// 1 つの `Command` を実行する。マウス側の入口。
    fn dispatch(&mut self, cmd: tsg_modal::Command, event_loop: &ActiveEventLoop) {
        self.update_view();
        let was_insert = self.engine.mode() == Mode::Insert;
        let effects = {
            let Some(view) = self.session.panes.get(&self.session.active) else {
                return;
            };
            let buf = view.buffer();
            self.engine.execute(cmd, &buf)
        };
        self.run_effects(effects, event_loop);
        self.snap_after_leaving_insert(was_insert);
    }

    /// 入力モードを抜けたら、**打っていた行**から読み始める。
    ///
    /// エンジンのカーソルは入力中は動かない（打鍵は素通しでシェルへ行く）ので、
    /// 抜けた瞬間に端末のカーソルへ合わせ直さないと、履歴の先頭に居ることになる。
    /// そうなると「k で上へ遡る」が最初の 1 回から効かない（実機で気づいた）。
    ///
    /// モードは `Engine::key` / `execute` の時点でもう変わっているので、
    /// 呼ぶ側が**前のモードを覚えておく**必要がある。
    fn snap_after_leaving_insert(&mut self, was_insert: bool) {
        if was_insert && self.engine.mode() == Mode::Normal {
            self.snap_to_live_tail();
        }
    }

    /// レジストリの id を実行する。メニューとパレットの唯一の出口。
    /// パレットの入力欄に打たれたものが、絞り込みではなくコマンドか。
    ///
    /// `:e パス` `:w` `:q` は vim の作法で、ここだけ別扱いにする。
    /// 一覧の絞り込みと同じ欄で受けるので、入り口を 2 つ作らずに済む。
    fn palette_command(&mut self, query: &str) -> bool {
        let q = query.trim();
        if let Some(path) = q.strip_prefix("e ").map(str::trim)
            && !path.is_empty()
        {
            self.palette.hide();
            self.open_file(path);
            return true;
        }
        if let Some(text) = q.strip_prefix("b ").map(str::trim)
            && !text.is_empty()
        {
            self.palette.hide();
            self.broadcast(text);
            return true;
        }
        if let Some(cmd) = q.strip_prefix('>').map(str::trim)
            && self.pending_pipe.is_some()
        {
            self.palette.hide();
            self.run_pipe(cmd);
            return true;
        }
        if let Some(path) = q.strip_prefix("w ").map(str::trim)
            && !path.is_empty()
        {
            self.palette.hide();
            self.save_file(Some(path.to_string()));
            return true;
        }
        match q {
            "w" => {
                self.palette.hide();
                self.save_file(None);
                true
            }
            "q" | "q!" => {
                self.palette.hide();
                self.close_file(q == "q!");
                true
            }
            "wq" => {
                self.palette.hide();
                self.save_file(None);
                self.close_file(false);
                true
            }
            _ => false,
        }
    }

    fn invoke(&mut self, id: &'static str, event_loop: &ActiveEventLoop) {
        if id == overlay::OPEN_PALETTE {
            self.menu.hide();
            self.palette.show();
            return;
        }
        self.menu.hide();
        self.palette.hide();
        self.update_view();
        let effects = {
            let Some(view) = self.session.panes.get(&self.session.active) else {
                return;
            };
            let buf = view.buffer();
            self.engine.invoke(id, &buf)
        };
        self.run_effects(effects, event_loop);
    }

    /// 一覧からの返事を捌く。
    fn apply_action(&mut self, action: overlay::Action, event_loop: &ActiveEventLoop) {
        match action {
            overlay::Action::Run(id) => self.invoke(id, event_loop),
            overlay::Action::Pick(name) => {
                let kind = self.picker.kind;
                self.picker.hide();
                match kind {
                    overlay::PickKind::Session => self.switch_session(&name),
                    overlay::PickKind::Path => self.open_file(&name),
                }
            }
            overlay::Action::Close => {
                self.menu.hide();
                self.palette.hide();
                self.picker.hide();
            }
            overlay::Action::Redraw | overlay::Action::None => {}
        }
    }

    fn on_mouse_press(&mut self, button: MouseButton, event_loop: &ActiveEventLoop) {
        let (col, row) = self.cell_at(self.pointer);

        // 使い方を出している間は、キーと同じくクリックでも閉じるだけにする。
        // ここを抜かすと、見えていない下の本文をクリックしてしまう
        // （実機で、使い方が出たまま端末に文字が入った）。
        if self.engine.help_visible() {
            self.dispatch(tsg_modal::Command::ToggleHelp, event_loop);
            return;
        }

        // 開いている一覧が最優先。下の本文へ吸われない。
        if self.menu.open {
            let a = self.menu.click(col, row);
            self.apply_action(a, event_loop);
            return;
        }
        if self.palette.open {
            let a = match row.checked_sub(self.palette_origin().1 + 1) {
                Some(r) => self.palette.click(r),
                None => overlay::Action::Close,
            };
            self.apply_action(a, event_loop);
            return;
        }
        if self.picker.open {
            let a = match row.checked_sub(self.palette_origin().1 + 1) {
                Some(r) => self.picker.click(r),
                None => overlay::Action::Close,
            };
            self.apply_action(a, event_loop);
            return;
        }

        // タブバー（`mouse-parity.md` §4.5「タブバーをクリック」「タブをドラッグ」）
        if row < self.tab_rows() {
            if let Some((_, _, tab, ..)) = self
                .tab_spans()
                .into_iter()
                .find(|(x, w, ..)| col >= *x && col < *x + *w)
            {
                self.send_msg(&ClientMsg::SelectTab { tab });
                // 離した場所が別のタブなら並べ替え。掴んだ時点では動かさない。
                if button == MouseButton::Left {
                    self.tab_drag = Some(tab);
                }
            }
            return;
        }

        // ステータス行
        if row + 1 >= self.rows {
            self.on_status_click(col, event_loop);
            return;
        }

        // ペイン境界（§4.5「境界をドラッグ」「境界をダブルクリックで均等化」）
        if button == MouseButton::Left
            && let Some((pane, dir)) = self.session.divider_at(col, row)
        {
            if self.clicks.press((usize::MAX - 1, row * 4096 + col), Instant::now()) >= 2 {
                self.run_mux(MuxRequest::Equalize, event_loop);
                self.status_msg = t!("分割比をそろえました", "splits evened out").into();
                return;
            }
            self.drag = Some(mouse::Drag::Divider {
                pane,
                dir,
                from: if dir == Dir::Horizontal { col } else { row },
            });
            return;
        }

        let Some(id) = self.session.pane_at(col, row) else {
            return;
        };
        if id != self.session.active {
            self.session.active = id;
            self.snap_to_live_tail();
        }

        // 畳んだ行を押したら開く。**要約に「クリックで開く」と書いてある**ので、
        // 押して何も起きないのは約束を破ることになる。
        if button == MouseButton::Left
            && let Some(pos) = self.doc_pos(id, col, row)
            && self
                .session
                .panes
                .get(&id)
                .is_some_and(|v| v.fold_at(pos.line).is_some())
        {
            if let Some(view) = self.session.panes.get_mut(&id) {
                view.folds.retain(|(s, _)| *s != pos.line);
            }
            self.status_msg = t!("開きました", "opened").into();
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            return;
        }

        // alt screen かつ子がマウスを要求しているなら、そのまま渡す（§5）
        let mouse_button = match button {
            MouseButton::Left => mouse::Button::Left,
            MouseButton::Middle => mouse::Button::Middle,
            MouseButton::Right => mouse::Button::Right,
            _ => return,
        };
        if self.mouse_goes_to_child() {
            self.forward_mouse(mouse_button, mouse::Phase::Press, col, row);
            return;
        }

        // 左ガター（§4.2「左ガターは、ターミナル固有モーションのマウス版そのもの」）
        if button == MouseButton::Left && self.on_gutter(id, col) {
            self.on_gutter_click(id, col, row, event_loop);
            return;
        }

        let Some(pos) = self.doc_pos(id, col, row) else {
            return;
        };

        match button {
            MouseButton::Left => self.on_left_press(id, pos, event_loop),
            MouseButton::Middle => {
                // Unix の慣習。Windows でもクリップボードから貼るのが素直。
                let text = self
                    .clipboard
                    .as_mut()
                    .and_then(|c| c.get_text().ok())
                    .unwrap_or_default();
                if text.is_empty() {
                    self.status_msg = t!("貼り付けるものがありません", "nothing to paste").into();
                } else {
                    self.snap_to_live_tail();
                    self.send_input(text.as_bytes());
                }
            }
            MouseButton::Right => {
                // ここではまだメニューを出さない。離した場所で
                // 「メニュー」か「プロンプトへ落とす」かが決まる（§4.4）。
                self.right_from = Some((id, pos, col, row));
            }
            _ => {}
        }
    }

    /// Ctrl＋クリック / `gx` の相手を開く（`mouse-parity.md` §4.4）。
    ///
    /// **勝手に実行はしない。** URL はブラウザ、パスはエディタ、ハッシュは
    /// `git show` をプロンプトへ置くところまで。最後の Enter は人が押す。
    fn open_at(&mut self, id: u32, pos: Pos) {
        let Some(view) = self.session.panes.get(&id) else {
            return;
        };
        let buf = view.buffer();
        let Some(range) = tsg_modal::textobj::at_pointer(&buf, pos) else {
            self.status_msg = t!("ここには開けるものがありません", "nothing to open here").into();
            return;
        };
        let text = tsg_modal::extract(&buf, &range);
        match open_kind(&text) {
            Some(OpenKind::Url) => match open_in_os(&text) {
                Ok(()) => self.status_msg = t!(format!("{text} を開きました"), format!("opened {text}")),
                Err(e) => self.status_msg = t!(format!("{text} を開けません: {e}"), format!("cannot open {text}: {e}")),
            },
            Some(OpenKind::Path) => {
                let path = strip_position(&text).trim_matches('"').to_string();
                self.open_file(&path);
            }
            Some(OpenKind::Hash) => {
                self.snap_to_live_tail();
                self.send_input(format!("git show {text}").as_bytes());
                self.dispatch_insert();
                self.status_msg = t!("プロンプトへ置きました（Enter は自分で）", "put on the prompt (press Enter yourself)").into();
            }
            None => self.status_msg = t!(format!("{text} は開ける形ではありません"), format!("{text} is not something I can open")),
        }
    }

    /// プロンプトへ何か置いた後は入力モードにしておく（続けて打てる）。
    fn dispatch_insert(&mut self) {
        let buf_kind = self.active_view().map(|v| v.buffer().kind());
        if buf_kind.is_some() {
            self.engine.execute(
                tsg_modal::Command::EnterInsert(tsg_modal::InsertAt::Here),
                &self.session.panes[&self.session.active].buffer(),
            );
            self.sync_ime();
        }
    }

    fn on_left_press(&mut self, id: u32, pos: Pos, event_loop: &ActiveEventLoop) {
        let clicks = self.clicks.press((pos.col, pos.line), Instant::now());
        let grain = mouse::Grain::of(clicks);
        self.drag = Some(mouse::Drag::Select { pane: id, grain });
        self.drag_from = pos;

        // Ctrl＋クリックは「開く」（§4.4）。選択より先に見る。
        if self.mods.control_key() {
            self.open_at(id, pos);
            return;
        }

        // Alt＋ドラッグは矩形選択（§3）
        if self.mods.alt_key() {
            let range = Range::new(pos, pos, RangeKind::Block);
            self.dispatch(tsg_modal::Command::Select { range }, event_loop);
            return;
        }

        if clicks == 1 {
            let view = self.session.panes.get(&id);
            let editing = view.is_some_and(PaneView::editing);
            let live = view.map_or(0, |v| v.term.state.grid.cursor_absolute());
            match click_intent(editing, pos.line, live) {
                ClickIntent::MoveOnly => {
                    self.dispatch(tsg_modal::Command::SetCursor(pos), event_loop);
                }
                ClickIntent::ToInsert => {
                    self.dispatch(
                        tsg_modal::Command::EnterInsert(tsg_modal::InsertAt::Here),
                        event_loop,
                    );
                }
                ClickIntent::ToNormal => {
                    self.dispatch(tsg_modal::Command::EnterNormal, event_loop);
                    self.dispatch(tsg_modal::Command::SetCursor(pos), event_loop);
                }
            }
            return;
        }

        // 2 回以上は範囲を作る。ここが `vif` と同じ `textobj` を通る。
        let range = {
            let Some(view) = self.session.panes.get(&id) else {
                return;
            };
            let buf = view.buffer();
            drag_range(&buf, pos, pos, grain, false)
        };
        self.dispatch(tsg_modal::Command::Select { range }, event_loop);
    }

    /// ステータス行の左に常設するボタン。**描画とヒット判定で同じものを使う。**
    ///
    /// 別々に持つと、字幅を変えた瞬間に「見えているのに押せない」が生まれる
    /// （実機で 1 度やった）。
    fn status_buttons(&self) -> Vec<(String, StatusTarget)> {
        let macro_label = match (self.engine.macros.recording(), self.engine.macros.last()) {
            (Some(n), _) => format!(" ● {n} "),
            (None, Some(n)) => format!(" ▶ {n} "),
            (None, None) => format!(" ● {} ", t!("記録", "rec")),
        };
        let mut out = vec![
            (
                format!(" ≡ {} ", t!("メニュー", "menu")),
                StatusTarget::Palette,
            ),
            (format!(" ? {} ", t!("使い方", "help")), StatusTarget::Help),
            (macro_label, StatusTarget::Macro),
            // モードの帯もボタン。押すと入力 ⇄ 読むが切り替わる。
            // キーを覚えていない人が、モードを行き来する唯一の手段になる。
            (format!("  {}  ", self.engine.mode().label()), StatusTarget::Mode),
        ];
        // 人の番になっているエージェントが居れば、その数を出す。
        // **押すとそこへ飛ぶ。** 数を見せるだけだと、探すのは結局手作業になる。
        let waiting = self.panes_wanting_you();
        if !waiting.is_empty() {
            // ⚑ は Consolas にも MS ゴシックにも無い（実機で空白になった）。
            // 記号だけに頼らず、言葉を添える。
            out.push((
                format!(" ● {} {} ", t!("返事待ち", "waiting"), waiting.len()),
                StatusTarget::AgentNext,
            ));
        }

        // Markdown を開いているあいだは、読む形への行き来を出す。
        // **キーを知らなくても押せる**ことがこのボタンの理由。
        if self
            .active_view()
            .is_some_and(|v| v.previewing() || v.lang() == tsg_modal::SyntaxLang::Markdown)
        {
            let on = self.active_view().is_some_and(PaneView::previewing);
            out.push((
                if on {
                    format!(" ◱ {} ", t!("素のまま", "raw"))
                } else {
                    format!(" ◱ {} ", t!("読む形", "render"))
                },
                StatusTarget::Preview,
            ));
        }

        // ファイルを開いているあいだだけ、戻る道を常に出しておく。
        // **ここが無いと帰れない。** `:q` も右クリックメニューも知らない人には、
        // エディタになったペインが行き止まりに見える（実機で行き止まった）。
        if self.active_view().is_some_and(|v| v.file.is_some()) {
            out.push((
                format!(" ✕ {} ", t!("端末へ戻る", "back to shell")),
                StatusTarget::CloseFile,
            ));
        }
        out
    }

    /// モードの色。帯・カーソル・区切り線で同じ色を使う。
    fn mode_color(th: &Theme, mode: Mode) -> [f32; 4] {
        match mode {
            Mode::Insert => th.mode_insert,
            Mode::Normal => th.mode_normal,
            Mode::Visual(_) => th.mode_visual,
            Mode::Layout => th.mode_layout,
            Mode::OperatorPending => th.mode_pending,
        }
    }

    fn on_status_click(&mut self, col: usize, event_loop: &ActiveEventLoop) {
        // §4.7 の「常設ボタン 1 クリック」。ここがマウス経路の最終保証。
        match status_hit(&self.status_buttons(), col) {
            StatusTarget::Palette => {
                self.palette.show();
                return;
            }
            StatusTarget::Help => {
                self.dispatch(tsg_modal::Command::ToggleHelp, event_loop);
                return;
            }
            StatusTarget::Mode => {
                // 入力中なら読むモードへ、それ以外なら入力へ。
                let cmd = if self.engine.mode() == Mode::Insert {
                    tsg_modal::Command::EnterNormal
                } else {
                    tsg_modal::Command::EnterInsert(tsg_modal::InsertAt::Here)
                };
                self.dispatch(cmd, event_loop);
                return;
            }
            StatusTarget::CloseFile => {
                self.invoke("file.close", event_loop);
                return;
            }
            StatusTarget::AgentNext => {
                self.jump_to_waiting_agent();
                return;
            }
            StatusTarget::Preview => {
                self.toggle_preview();
                return;
            }
            StatusTarget::Macro => {
                // 出ているバッジと同じことをする。記録中なら止め、
                // 録ったものがあれば流し、無ければ名前を訊く。
                let id = if self.engine.macros.recording().is_some()
                    || self.engine.macros.last().is_none()
                {
                    "macro.record"
                } else {
                    "macro.replay"
                };
                self.invoke(id, event_loop);
                return;
            }
            StatusTarget::Ownership => {}
        }
        if let Some(v) = self.session.panes.get_mut(&self.session.active) {
            v.term.state.pins.mouse = Some(InputOwner::Tsumugi);
            v.term.state.pins.key = Some(InputOwner::Tsumugi);
        }
        self.dispatch(tsg_modal::Command::EnterNormal, event_loop);
        self.status_msg = t!("入力の所有権を tsumugi に固定しました", "input is pinned to tsumugi now").into();
    }

    fn on_mouse_release(&mut self, button: MouseButton) {
        let (col, row) = self.cell_at(self.pointer);

        // 右ボタン: 掴んだ場所と離した場所が違えば「プロンプトへ落とす」、
        // 同じならただの右クリックなのでメニューを出す（§4.4 のドラッグ＆ドロップ）。
        if button == MouseButton::Right
            && let Some((id, from, fx, fy)) = self.right_from.take()
        {
            if (col, row) == (fx, fy) {
                let has_selection = self.engine.selection().is_some();
                self.menu.show((col, row), has_selection);
            } else {
                self.drop_on_prompt(id, from);
            }
            return;
        }

        // タブを別のタブの上で離したら、そこへ差し込む（§4.5「タブをドラッグ」）
        if let Some(dragged) = self.tab_drag.take()
            && row < self.tab_rows()
            && let Some(to) = self
                .tab_spans()
                .into_iter()
                .position(|(x, w, ..)| col >= x && col < x + w)
            && self
                .session
                .info
                .as_ref()
                .is_some_and(|i| i.tabs.get(to).is_some_and(|t| t.id != dragged))
        {
            self.send_msg(&ClientMsg::MoveTab { tab: dragged, to });
            self.status_msg = t!(format!("タブを {} 番目へ動かしました", to + 1), format!("moved the tab to position {}", to + 1));
        }

        if self.mouse_goes_to_child() {
            let b = match button {
                MouseButton::Left => mouse::Button::Left,
                MouseButton::Middle => mouse::Button::Middle,
                MouseButton::Right => mouse::Button::Right,
                _ => return,
            };
            self.forward_mouse(b, mouse::Phase::Release, col, row);
        }
        self.drag = None;
    }

    /// 掴んだものをプロンプトへ落とす（右ドラッグ＆ドロップ）。
    ///
    /// 選択があってその中から掴んだなら選択全体、無ければポインタ下の対象。
    /// キーボードの `!` と同じ `Effect::SendToPrompt` に合流する。
    fn drop_on_prompt(&mut self, id: u32, from: Pos) {
        let text = {
            let Some(view) = self.session.panes.get(&id) else {
                return;
            };
            let buf = view.buffer();
            let range = self
                .engine
                .selection()
                .filter(|r| from >= r.start && from <= r.end)
                .or_else(|| tsg_modal::textobj::at_pointer(&buf, from));
            let Some(range) = range else {
                return;
            };
            tsg_modal::extract(&buf, &range)
        };
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.snap_to_live_tail();
        self.send_input(text.as_bytes());
        self.dispatch_insert();
        self.status_msg = t!("プロンプトへ落としました", "dropped on the prompt").into();
    }

    /// ポインタの下の対象を拾い直す。下線と Ctrl＋クリックが同じものを指すよう、
    /// **両方ともここが返す範囲だけ**を見る。
    fn update_hover(&mut self) {
        let (col, row) = self.cell_at(self.pointer);
        let next = self
            .session
            .pane_at(col, row)
            .filter(|id| !self.on_gutter(*id, col))
            .and_then(|id| Some((id, self.doc_pos(id, col, row)?)))
            .and_then(|(id, pos)| {
                let view = self.session.panes.get(&id)?;
                let buf = view.buffer();
                let range = tsg_modal::textobj::at_pointer(&buf, pos)?;
                // 語の下線は騒がしいだけなので、開けるものだけを光らせる。
                open_kind(&tsg_modal::extract(&buf, &range)).map(|_| (id, range))
            });
        if next.map(|(i, r)| (i, r.start, r.end)) != self.hover.map(|(i, r)| (i, r.start, r.end)) {
            self.hover = next;
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
    }

    fn on_mouse_move(&mut self, event_loop: &ActiveEventLoop) {
        let (col, row) = self.cell_at(self.pointer);

        if self.menu.open {
            let a = self.menu.hover(col, row);
            self.apply_action(a, event_loop);
            return;
        }

        match self.drag {
            Some(mouse::Drag::Divider { pane, dir, from }) => {
                let now = if dir == Dir::Horizontal { col } else { row };
                let delta = now as i32 - from as i32;
                if delta != 0 {
                    // 1 セル動かすたびに送る。木はサーバが持つので、
                    // 返ってきた Layout で割り付け直される。
                    self.send_msg(&ClientMsg::ResizeSplit {
                        pane,
                        delta: delta * 4,
                    });
                    self.drag = Some(mouse::Drag::Divider {
                        pane,
                        dir,
                        from: now,
                    });
                }
            }
            Some(mouse::Drag::Select { pane, grain }) => {
                if self.mouse_goes_to_child() {
                    self.forward_mouse(mouse::Button::Left, mouse::Phase::Drag, col, row);
                    return;
                }
                let Some(to) = self.doc_pos(pane, col, row) else {
                    return;
                };
                let from = self.drag_from;
                let block = self.mods.alt_key();
                let range = {
                    let Some(view) = self.session.panes.get(&pane) else {
                        return;
                    };
                    let buf = view.buffer();
                    drag_range(&buf, from, to, grain, block)
                };
                self.dispatch(tsg_modal::Command::Select { range }, event_loop);
            }
            None => {}
        }
    }

    /// ホイール。所有権に従って行き先が変わる（`mouse-parity.md` §5）。
    fn on_wheel(&mut self, delta: MouseScrollDelta) {
        let lines = match delta {
            MouseScrollDelta::LineDelta(_, y) => -(y as isize) * 3,
            MouseScrollDelta::PixelDelta(p) => -(p.y as isize) / 20,
        };
        if lines == 0 {
            return;
        }
        // Ctrl＋ホイールは字の大きさ（`mouse-parity.md` §3）。
        // 子プロセスへ渡す前に見る。全画面アプリの上でも効いてほしい。
        if self.mods.control_key() {
            let step = if lines < 0 { 1.0 } else { -1.0 };
            let Some(r) = self.renderer.as_mut() else {
                return;
            };
            let next = r.font_size() + step;
            if r.set_font_size(next) {
                let size = r.font_size();
                self.cfg.font_size = size;
                self.resize_window();
                self.status_msg = t!(format!("文字の大きさ {size:.0}px"), format!("font size {size:.0}px"));
            }
            return;
        }

        if self.engine.help_visible() {
            self.scroll_help(if lines < 0 { -3 } else { 3 });
            return;
        }

        // 一覧が開いているあいだは、ホイールはその一覧を動かす。
        // ここが無いと**後ろの本文が動くだけ**で、出ている一覧はびくともしない。
        if self.palette.open || self.picker.open || self.menu.open {
            let step = if lines < 0 { -1 } else { 1 };
            let _ = self.move_overlay(step);
            return;
        }

        let (col, row) = self.cell_at(self.pointer);

        if self.mouse_goes_to_child() {
            let b = if lines < 0 {
                mouse::Button::WheelUp
            } else {
                mouse::Button::WheelDown
            };
            for _ in 0..lines.abs().min(5) {
                self.forward_mouse(b, mouse::Phase::Press, col, row);
            }
            return;
        }

        // alt screen だがマウスレポートを要求していない（less 等）。
        // 慣例に合わせて矢印キーへ変換する。
        let alt_no_mouse = self.active_view().is_some_and(|v| {
            v.term.state.grid.is_alt() && v.term.state.modes.mouse == tsg_term::MouseTracking::Off
        });
        if alt_no_mouse {
            let app = self
                .active_view()
                .is_some_and(|v| v.term.state.modes.app_cursor_keys);
            let key: &[u8] = match (lines < 0, app) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            let mut out = Vec::new();
            for _ in 0..lines.abs().min(5) {
                out.extend_from_slice(key);
            }
            self.send_input(&out);
            return;
        }

        self.scroll_by(lines);
    }

    // ---- 入力 -------------------------------------------------------------

    fn on_key(&mut self, event_loop: &ActiveEventLoop, key: Key, text: Option<String>) {
        self.update_view();

        // ラベル待ちのあいだは、そのキーはラベルの一部。
        if !self.hints.is_empty() {
            match &key {
                Key::Named(NamedKey::Escape) => {
                    self.hints.clear();
                    self.hint_typed.clear();
                    self.status_msg.clear();
                }
                _ => {
                    if let Some(c) = text.as_deref().and_then(|t| t.chars().next())
                        && c.is_ascii_alphabetic()
                    {
                        self.hint_key(c);
                    }
                }
            }
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            return;
        }

        // 使い方を読んでいる最中の上下だけは、閉じずに送る。
        // それ以外のキーは今までどおり閉じる（engine が決めている）。
        if self.engine.help_visible() {
            let step = match &key {
                Key::Named(NamedKey::ArrowDown) => 1,
                Key::Named(NamedKey::ArrowUp) => -1,
                Key::Named(NamedKey::PageDown) => 8,
                Key::Named(NamedKey::PageUp) => -8,
                _ => 0,
            };
            if step != 0 {
                self.scroll_help(step);
                return;
            }
            self.help_scroll = 0;
        }

        if self.palette.open || self.menu.open || self.picker.open {
            self.overlay_key(&key, text.as_deref(), event_loop);
            return;
        }

        // `:` でコマンドパレット（`mouse-parity.md` §4.7）
        if self.engine.mode() != Mode::Insert
            && matches!(&key, Key::Character(c) if c.as_str() == ":")
        {
            self.palette.show();
            return;
        }

        // Esc の所有権。alt screen では子プロセスのものになる（concept.md）。
        // `C-\` はこの裁定を迂回して常に tsumugi が取る。
        let forced =
            self.mods.control_key() && matches!(&key, Key::Character(s) if s.as_str() == "\\");
        let child_owns_esc = self
            .active_view()
            .is_some_and(|v| v.term.state.key_owner() == InputOwner::Child);
        if !forced
            && self.engine.mode() == Mode::Insert
            && matches!(key, Key::Named(NamedKey::Escape))
            && child_owns_esc
        {
            self.send_input(&[0x1b]);
            return;
        }

        let Some(input) = to_key_input(&key, self.mods, self.engine.mode()) else {
            return;
        };

        let was_insert = self.engine.mode() == Mode::Insert;
        let outcome = {
            let Some(view) = self.session.panes.get(&self.session.active) else {
                return;
            };
            let buf = view.buffer();
            self.engine.key(input, &buf)
        };

        match outcome {
            KeyOutcome::Handled(effects) => {
                self.run_effects(effects, event_loop);
                self.snap_after_leaving_insert(was_insert);
            }
            KeyOutcome::PassThrough => {
                // エディタとして開いていれば、打鍵はファイルへ入る。
                // 下のシェルには 1 バイトも行かない。
                // 読む形のあいだは書き換えない。**写しを編集しても
                // 元のファイルには入らない**ので、入ったように見せない。
                if self.active_view().is_some_and(PaneView::previewing) {
                    self.status_msg = t!(
                        "読む形のあいだは編集できません（Space m で戻る）",
                        "read-only while rendered (Space m to go back)"
                    )
                    .into();
                    return;
                }
                if self.active_view().is_some_and(PaneView::editing) {
                    self.type_into_file(&key, text.as_deref());
                    return;
                }
                let app_cursor = self
                    .active_view()
                    .is_some_and(|v| v.term.state.modes.app_cursor_keys);
                if let Some(bytes) = input::encode(&key, text.as_deref(), self.mods, app_cursor) {
                    self.send_input(&bytes);
                }
            }
        }
    }

    /// 入力モードの打鍵をファイルへ反映する。
    fn type_into_file(&mut self, key: &Key, text: Option<&str>) {
        let active = self.session.active;
        let cursor = self.engine.cursor();
        let Some(view) = self.session.panes.get_mut(&active) else {
            return;
        };
        let Some(file) = view.file.as_mut() else {
            return;
        };

        let next = match key {
            Key::Named(NamedKey::Enter) => Some(file.insert(cursor, "\n")),
            Key::Named(NamedKey::Tab) => Some(file.insert(cursor, "    ")),
            Key::Named(NamedKey::Backspace) => {
                let prev = if cursor.col > 0 {
                    Pos::new(cursor.line, cursor.col - 1)
                } else if cursor.line > 0 {
                    let above = cursor.line - 1;
                    Pos::new(above, tsg_modal::line_text(file, above).chars().count())
                } else {
                    return;
                };
                let end = Pos::new(cursor.line, cursor.col.saturating_sub(1));
                if cursor.col > 0 {
                    file.delete(&Range::new(prev, end, RangeKind::Char));
                } else {
                    // 行頭の後退は上の行と繋ぐ
                    file.delete(&Range::new(prev, Pos::new(cursor.line, 0), RangeKind::Char));
                }
                Some(prev)
            }
            _ => text
                .filter(|t| !t.is_empty() && !t.chars().any(char::is_control))
                .map(|t| file.insert(cursor, t)),
        };

        if let Some(pos) = next {
            // ここで `clamp` を掛けない。行末に打った直後のカーソルは
            // 最後の文字の 1 つ先に居るべきで、収めるのは `set_cursor` の仕事。
            let buf = view.buffer();
            self.engine.set_cursor(pos, &buf);
            self.push_file();
        }
    }

    /// 一覧が開いている間のキー。
    fn overlay_key(&mut self, key: &Key, text: Option<&str>, event_loop: &ActiveEventLoop) {
        // 検索は打つたびに飛ぶ。**探す前に一覧を眺める手間を入れない。**
        if self.palette.searching() {
            match key {
                Key::Named(NamedKey::Escape) => {
                    // 探し始める前の場所へ戻す。見つからないまま迷子にしない。
                    if let Some(p) = self.search_from.take()
                        && let Some(view) = self.session.panes.get(&self.session.active)
                    {
                        let buf = view.buffer();
                        self.engine.set_cursor(p, &buf);
                    }
                    self.palette.hide();
                    self.engine.search = None;
                    self.status_msg.clear();
                }
                Key::Named(NamedKey::Enter) => {
                    self.search_from = None;
                    self.palette.hide();
                    let n = self.search_hits();
                    self.status_msg = t!(
                        format!("{} 件（n で次、N で前）", n),
                        format!("{n} matches (n / N to step)")
                    );
                }
                Key::Named(NamedKey::Backspace) => {
                    self.palette.query.pop();
                    self.run_search();
                }
                _ => {
                    if let Some(c) = text.and_then(|t| t.chars().next())
                        && !c.is_control()
                    {
                        self.palette.query.push(c);
                        self.run_search();
                    }
                }
            }
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            return;
        }

        let action = match key {
            Key::Named(NamedKey::Escape) => overlay::Action::Close,
            Key::Named(NamedKey::Enter) => {
                // `:e パス` などはコマンドとして先に見る
                if self.palette.open {
                    let q = self.palette.query.clone();
                    if self.palette_command(&q) {
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                        return;
                    }
                }
                if self.picker.open {
                    self.picker.accept()
                } else if self.menu.open {
                    match self.menu.chosen() {
                        Some(id) => overlay::Action::Run(id),
                        None => overlay::Action::Close,
                    }
                } else {
                    self.palette.accept()
                }
            }
            Key::Named(NamedKey::ArrowDown) => self.move_overlay(1),
            Key::Named(NamedKey::ArrowUp) => self.move_overlay(-1),
            Key::Named(NamedKey::Backspace) if self.palette.open => self.palette.backspace(),
            _ => {
                if self.mods.control_key() {
                    match key {
                        Key::Character(c) if c.as_str() == "n" => self.move_overlay(1),
                        Key::Character(c) if c.as_str() == "p" => self.move_overlay(-1),
                        _ => overlay::Action::None,
                    }
                } else if self.picker.open {
                    // 一覧は j / k でも動かせる（配置モードから来るので手が近い）
                    match text.and_then(|t| t.chars().next()) {
                        Some('j') => self.picker.move_by(1),
                        Some('k') => self.picker.move_by(-1),
                        _ => overlay::Action::None,
                    }
                } else if self.palette.open {
                    match text.and_then(|t| t.chars().next()) {
                        Some(c) if !c.is_control() => self.palette.push(c),
                        _ => overlay::Action::None,
                    }
                } else {
                    overlay::Action::None
                }
            }
        };
        self.apply_action(action, event_loop);
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// いまの場所の `git diff` を、色を付けて新しいペインに開く。
    ///
    /// **エージェントが何を変えたかを読むための道具。** 端末にそのまま
    /// 流すと流れて消えるが、ペインに開けば `n` で探せて `Space o` で
    /// ファイル単位に畳める。色は `--color=never` にして自前で付ける
    /// （相手の色コードを解釈し直すより、行の頭 1 文字を見るほうが確か）。
    fn show_git_diff(&mut self) {
        let cwd = self
            .active_view()
            .and_then(|v| v.term.state.cwd.clone())
            .and_then(|u| file_url_to_path(&u))
            .or_else(|| self.cwd.clone());
        let mut cmd = std::process::Command::new("git");
        cmd.args(["diff", "--color=never", "--no-ext-diff"]);
        if let Some(dir) = cwd.as_deref() {
            cmd.current_dir(dir);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let out = match cmd.output() {
            Ok(o) => o,
            Err(e) => {
                self.status_msg = t!(
                    format!("git を実行できません: {e}"),
                    format!("cannot run git: {e}")
                );
                return;
            }
        };
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        if text.trim().is_empty() {
            self.status_msg = t!("変更はありません", "no changes").into();
            return;
        }
        let pane = self.session.active;
        self.send_msg(&ClientMsg::PipeResult {
            pane,
            dir: Dir::Horizontal,
            title: "diff".to_string(),
            text,
        });
    }

    /// 見えているペイン全部へ同じ文を投げる。
    ///
    /// **同じ問いを別のエージェントへ同時に。** 返事の速さも中身も違うので、
    /// 揃ったところで見比べる（`tsg --compare`）。投げた先は「動いている」に
    /// しておく — 投げる前から返事待ちだったペインを見て「もう返ってきた」と
    /// 誤らないため。
    fn broadcast(&mut self, text: &str) {
        let panes = self.session.visible_panes();
        if panes.is_empty() {
            return;
        }
        for id in &panes {
            self.send_msg(&ClientMsg::SetAgentState {
                pane: Some(*id),
                state: AgentState::Working,
                cost: None,
            });
        }
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(b'\r');
        self.send_msg(&ClientMsg::Broadcast {
            panes: panes.clone(),
            data: tsg_mux::encode_bytes(&bytes),
        });
        self.status_msg = t!(
            format!("{} 個のペインへ投げました", panes.len()),
            format!("sent to {} panes", panes.len())
        );
    }

    /// カーソルの居るコマンドの出力を畳む / 開く。
    ///
    /// **境目はシェル統合（OSC 133）が教える。** 空行で当てにいくと、
    /// 出力の途中の空行で切れる。
    fn toggle_fold_here(&mut self) {
        let pane = self.session.active;
        let here = self.engine.cursor().line;
        let Some(view) = self.session.panes.get(&pane) else {
            return;
        };
        // 畳んである中に居るなら開く。
        if let Some((start, _)) = view.fold_covering(here) {
            let Some(view) = self.session.panes.get_mut(&pane) else {
                return;
            };
            view.folds.retain(|(s, _)| *s != start);
            self.status_msg = t!("開きました", "opened").into();
            return;
        }
        let Some((start, end)) = self.output_span(pane, here) else {
            self.status_msg = t!(
                "ここには畳める出力がありません（シェル統合が要ります）",
                "no output to fold here (needs shell integration)"
            )
            .into();
            return;
        };
        if let Some(view) = self.session.panes.get_mut(&pane) {
            view.toggle_fold(start, end);
        }
        self.status_msg = t!(
            format!("{} 行を畳みました", end + 1 - start),
            format!("folded {} lines", end + 1 - start)
        );
    }

    /// その行を含む「畳める範囲」。
    ///
    /// 端末はコマンドの出力（OSC 133）、diff はファイル 1 つぶん。
    /// **境目を当てにいかない**のが要点で、どちらも印がはっきりしている。
    fn output_span(&self, pane: u32, line: usize) -> Option<(usize, usize)> {
        let view = self.session.panes.get(&pane)?;
        if view.lang() == tsg_modal::SyntaxLang::Diff {
            return self.diff_span(pane, line);
        }
        let buf = view.buffer();
        let blocks = buf.marks().blocks();
        let last = buf.line_count().saturating_sub(1);
        blocks.iter().find_map(|b| {
            let start = b.output_start?;
            let end = b.output_end.unwrap_or(last);
            // 出力が 1 行しかないなら畳む意味がない。
            (end > start && (start..=end).contains(&line) || b.command_line == Some(line))
                .then_some((start, end))
        })
    }

    /// diff の「ファイル 1 つぶん」。`diff --git` から次の `diff --git` の手前まで。
    fn diff_span(&self, pane: u32, line: usize) -> Option<(usize, usize)> {
        let view = self.session.panes.get(&pane)?;
        let buf = view.buffer();
        let last = buf.line_count().saturating_sub(1);
        let heads: Vec<usize> = (0..=last)
            .filter(|l| tsg_modal::line_text(&buf, *l).starts_with("diff --git"))
            .collect();
        let i = heads.iter().rposition(|h| *h <= line)?;
        let start = heads[i];
        let end = heads.get(i + 1).map_or(last, |h| h - 1);
        (end > start).then_some((start, end))
    }

    /// 画面の出力を全部畳む / 全部開く。
    fn fold_all(&mut self, on: bool) {
        let pane = self.session.active;
        if !on {
            if let Some(view) = self.session.panes.get_mut(&pane) {
                view.folds.clear();
            }
            self.status_msg = t!("全部開きました", "opened everything").into();
            return;
        }
        let Some(view) = self.session.panes.get(&pane) else {
            return;
        };
        let buf = view.buffer();
        let last = buf.line_count().saturating_sub(1);
        let spans: Vec<(usize, usize)> = if view.lang() == tsg_modal::SyntaxLang::Diff {
            // diff はファイルごと。**1 行目は開いたままにする**ので、
            // 何のファイルかは畳んだままでも分かる。
            let heads: Vec<usize> = (0..=last)
                .filter(|l| tsg_modal::line_text(&buf, *l).starts_with("diff --git"))
                .collect();
            heads
                .iter()
                .enumerate()
                .filter_map(|(i, h)| {
                    let end = heads.get(i + 1).map_or(last, |n| n - 1);
                    (end > *h).then_some((*h, end))
                })
                .collect()
        } else {
            buf.marks()
                .blocks()
                .iter()
                .filter_map(|b| {
                    let start = b.output_start?;
                    let end = b.output_end.unwrap_or(last);
                    (end > start).then_some((start, end))
                })
                .collect()
        };
        let n = spans.len();
        if let Some(view) = self.session.panes.get_mut(&pane) {
            view.folds = spans;
        }
        self.status_msg = if n == 0 {
            t!(
                "畳める出力がありません（シェル統合が要ります）",
                "nothing to fold (needs shell integration)"
            )
            .into()
        } else {
            t!(
                format!("{n} 個の出力を畳みました（Space U で開く）"),
                format!("folded {n} outputs (Space U to open)")
            )
        };
    }

    /// 画面に出ているパス・URL・ハッシュにラベルを振る。
    ///
    /// **目で見つけたものへ、手を動かさずに届く。** マウスへ持ち替えるほど
    /// でもなく、かといって座標を打つわけにもいかない距離が毎回ある。
    /// ラベルはホームポジションから順に配る。
    fn show_hints(&mut self) {
        const LABELS: &[u8] = b"asdfghjklqwertyuiopzxcvbnm";
        let mut hints: Vec<Hint> = Vec::new();
        let visible = self.session.visible_panes();
        for id in visible {
            let Some(view) = self.session.panes.get(&id) else {
                continue;
            };
            let rect = view.text_rect();
            let buf = view.buffer();
            let top = view.top;
            for r in 0..rect.h {
                let line = top + r;
                let Some(cells) = buf.cells(line) else {
                    continue;
                };
                // 桁を数えながら語に切る。**空白で切るだけ**にして、
                // 何を拾うかの判断は `open_kind` 1 か所に任せる。
                let mut col = 0usize;
                let mut start = 0usize;
                let mut word = String::new();
                let flush = |word: &mut String, start: usize, hints: &mut Vec<Hint>| {
                    let trimmed = word.trim_matches(|c: char| "\"'`（）()[]{}<>、。,;:!?".contains(c));
                    if !trimmed.is_empty() && open_kind(trimmed).is_some() {
                        hints.push(Hint {
                            pane: id,
                            line,
                            col: start,
                            text: trimmed.to_string(),
                            label: String::new(),
                        });
                    }
                    word.clear();
                };
                for cell in cells.iter().take(rect.w) {
                    let ch = cell.text.chars().next().unwrap_or(' ');
                    if cell.width == 0 {
                        col += 1;
                        continue;
                    }
                    if ch.is_whitespace() {
                        flush(&mut word, start, &mut hints);
                    } else {
                        if word.is_empty() {
                            start = col;
                        }
                        word.push_str(&cell.text);
                    }
                    col += 1;
                }
                flush(&mut word, start, &mut hints);
            }
        }
        if hints.is_empty() {
            self.status_msg = t!(
                "画面に開けるものがありません",
                "nothing on screen to jump to"
            )
            .into();
            return;
        }
        // ラベルを配る。数が多ければ 2 文字にする。
        let two = hints.len() > LABELS.len();
        for (i, h) in hints.iter_mut().enumerate() {
            h.label = if two {
                let a = LABELS[i / LABELS.len() % LABELS.len()] as char;
                let b = LABELS[i % LABELS.len()] as char;
                format!("{a}{b}")
            } else {
                (LABELS[i] as char).to_string()
            };
        }
        self.hints = hints;
        self.hint_typed.clear();
        self.status_msg = t!(
            "ラベルの字を押すと開きます（Esc でやめる）",
            "press a label to open it (Esc to cancel)"
        )
        .into();
    }

    /// ラベルが押された。決まれば開く。
    fn hint_key(&mut self, c: char) {
        self.hint_typed.push(c.to_ascii_lowercase());
        let typed = self.hint_typed.clone();
        if let Some(h) = self.hints.iter().find(|h| h.label == typed).cloned() {
            self.hints.clear();
            self.hint_typed.clear();
            self.session.active = h.pane;
            self.status_msg = t!(format!("{} を開きます", h.text), format!("opening {}", h.text));
            self.open_at(h.pane, Pos::new(h.line, h.col));
            return;
        }
        if !self.hints.iter().any(|h| h.label.starts_with(&typed)) {
            self.hints.clear();
            self.hint_typed.clear();
            self.status_msg = t!("そのラベルはありません", "no such label").into();
        }
    }

    /// 打った文字で探し直して、最初の一致へ飛ぶ。
    ///
    /// **出発点から探す。** 前の一致から探すと、1 文字消したときに
    /// 後ろへ後ろへ流れていって戻れなくなる。
    fn run_search(&mut self) {
        let q = self.palette.query.clone();
        self.engine.search = (!q.is_empty()).then(|| q.clone());
        let Some(from) = self.search_from else {
            return;
        };
        let Some(view) = self.session.panes.get(&self.session.active) else {
            return;
        };
        let buf = view.buffer();
        if q.is_empty() {
            self.engine.set_cursor(from, &buf);
            self.status_msg.clear();
            return;
        }
        let back = matches!(self.palette.kind, overlay::PaletteKind::Search { back: true });
        // 出発点そのものも一致し得るので、1 つ手前から探し始める。
        let seed = if back {
            Pos::new(from.line, from.col + 1)
        } else {
            Pos::new(from.line, from.col.saturating_sub(1))
        };
        match tsg_modal::find_match(&buf, seed, &q, back) {
            Some(p) => {
                self.engine.set_cursor(p, &buf);
                self.status_msg.clear();
            }
            None => {
                self.engine.set_cursor(from, &buf);
                self.status_msg = t!("見つかりません", "no match").into();
            }
        }
        self.follow_cursor();
    }

    /// いま探しているものが文書に何件あるか。
    fn search_hits(&self) -> usize {
        let Some(q) = self.engine.search.as_deref() else {
            return 0;
        };
        let Some(view) = self.session.panes.get(&self.session.active) else {
            return 0;
        };
        let buf = view.buffer();
        (0..buf.line_count())
            .map(|l| tsg_modal::matches_in(&buf, l, q).len())
            .sum()
    }

    /// 使い方の画面を送る。行数は組み立ててから数えるので、必ず最後まで行ける。
    fn scroll_help(&mut self, delta: isize) {
        let avail = self.rows.saturating_sub(3);
        let max = help_lines().len().saturating_sub(avail);
        let next = (self.help_scroll as isize + delta).clamp(0, max as isize);
        self.help_scroll = next as usize;
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn move_overlay(&mut self, delta: isize) -> overlay::Action {
        if self.picker.open {
            return self.picker.move_by(delta);
        }
        if self.menu.open {
            return self.menu.step(delta);
        }
        self.palette.move_by(delta)
    }

    /// パレットの左上のセル座標。
    fn palette_origin(&self) -> (usize, usize) {
        let w = self.palette_width();
        ((self.cols.saturating_sub(w)) / 2, 2)
    }

    fn palette_width(&self) -> usize {
        self.cols.clamp(20, 96).min(self.cols)
    }

    // ---- 描画 -------------------------------------------------------------

    fn draw(&mut self) {
        let th = self.theme;
        self.update_view();

        let active = self.session.active;
        let mode = self.engine.mode();
        let selection = self.engine.selection();
        let search = self.engine.search.clone();
        let visible = self.session.visible_panes();
        let help = self.engine.help_visible();
        let help_scroll = self.help_scroll;
        let cursor_engine = self.engine.cursor();
        let preedit = self.preedit.clone();
        let (cols, rows) = (self.cols, self.rows);

        {
            let Some(renderer) = self.renderer.as_mut() else {
                return;
            };
            renderer.begin();

            if help {
                draw_help(renderer, &th, cols, rows, help_scroll);
            } else {
                for id in &visible {
                    let Some(view) = self.session.panes.get(id) else {
                        continue;
                    };
                    let rect = view.rect;
                    let is_active = *id == active;
                    let divider = if is_active { th.divider_active } else { th.divider };

                    if rect.x > 0 {
                        renderer.rect(
                            (rect.x - 1) as f32,
                            rect.y as f32,
                            1.0,
                            rect.h as f32,
                            divider,
                        );
                    }
                    if rect.y > 0 {
                        renderer.rect(
                            rect.x as f32,
                            (rect.y - 1) as f32,
                            rect.w as f32,
                            1.0,
                            divider,
                        );
                    }

                    let doc = view.buffer();
                    let top = view.top.min(doc.line_count().saturating_sub(1));
                    let rect = view.text_rect();
                    // 画面の r 行目に出す文書行。畳んだ分は飛ぶ。
                    // **見せる側と当てる側で同じ関数を通す**（`line_at`）。
                    let row_line: Vec<usize> = (0..rect.h).map(|r| view.line_at(r)).collect();
                    let at = |r: usize| row_line.get(r).copied().unwrap_or(top + r);

                    // 左ガター。OSC 133 のブロックをそのまま印にする。
                    // 出力を正規表現で当てにいかないので、嘘の印が出ない。
                    // ファイルを開いている間はプロンプトが無いので出さない。
                    for b in doc.marks().blocks() {
                        let Some(r) = b.prompt_line.checked_sub(top) else {
                            continue;
                        };
                        if r >= rect.h {
                            continue;
                        }
                        // 記号ではなく矩形で描く。`\u{276f}` はフォントチェーンに
                        // 無いことがあり（実機で緑のマーカーだけ消えた）、
                        // 字が出るかどうかに製品の見た目を賭けない。
                        // 太さでも区別するので、色が見分けにくくても伝わる。
                        let (w, color) = if b.is_running() {
                            (0.55, th.gut_run)
                        } else if b.is_error() {
                            (0.9, th.gut_err)
                        } else {
                            (0.25, th.gut_ok)
                        };
                        renderer.rect(
                            view.rect.x as f32 + 0.2,
                            (rect.y + r) as f32,
                            w,
                            1.0,
                            if is_active { color } else { th.fade(color, 0.45) },
                        );
                    }

                    // ホバー中の対象に下線（§3「ホバー: 対象のハイライト」）。
                    if let Some((hid, hr)) = self.hover
                        && hid == *id
                        && let Some(r) = hr.start.line.checked_sub(top)
                        && r < rect.h
                    {
                        let x0 = hr.start.col.min(rect.w);
                        let x1 = (hr.end.col + 1).min(rect.w);
                        if x1 > x0 {
                            renderer.rect(
                                (rect.x + x0) as f32,
                                (rect.y + r) as f32 + 0.88,
                                (x1 - x0) as f32,
                                0.08,
                                th.hover,
                            );
                        }
                    }

                    // 置いた印。ガターの右半分に出す。クリックで飛べる。
                    for r in 0..rect.h {
                        let Some(name) = self.engine.marks.at_line(at(r)) else {
                            continue;
                        };
                        renderer.text(
                            (view.rect.x + 1) as f32,
                            (rect.y + r) as f32,
                            &name.to_string(),
                            if is_active {
                                th.gut_mark
                            } else {
                                th.fade(th.gut_mark, 0.45)
                            },
                            true,
                        );
                    }

                    // 背景。同色の連なりを 1 枚の矩形にまとめる。
                    // セルごとに出すと 80x24 で 2000 枚近くになり、色の無い画面でも払う。
                    for r in 0..rect.h {
                        if view.fold_at(at(r)).is_some() {
                            continue; // 畳んだ行は要約だけ出す
                        }
                        let Some(cells) = doc.cells(at(r)) else {
                            break;
                        };
                        let mut start = 0usize;
                        let mut run: Option<[f32; 4]> = None;
                        for c in 0..=rect.w {
                            let bg = cells
                                .get(c)
                                .filter(|_| c < rect.w)
                                .and_then(|cell| cell_colors(&th, &cell.attrs).1)
                                .map(|b| if is_active { b } else { th.fade(b, 0.35) });
                            if bg != run {
                                if let Some(color) = run {
                                    renderer.rect(
                                        (rect.x + start) as f32,
                                        (rect.y + r) as f32,
                                        (c - start) as f32,
                                        1.0,
                                        color,
                                    );
                                }
                                run = bg;
                                start = c;
                            }
                        }
                    }

                    // 探しているものを光らせる。**選択より先に塗る**ので、
                    // 選んだところは選択の色が勝つ。
                    if let Some(q) = search.as_deref() {
                        for r in 0..rect.h {
                            for (from, w) in tsg_modal::matches_in(&doc, at(r), q) {
                                if from >= rect.w {
                                    continue;
                                }
                                let w = w.min(rect.w - from) as f32;
                                renderer.rect(
                                    (rect.x + from) as f32,
                                    (rect.y + r) as f32,
                                    w,
                                    1.0,
                                    th.search_hit,
                                );
                            }
                        }
                    }

                    if is_active
                        && let Some(range) = selection
                    {
                        for r in 0..rect.h {
                            if let Some((from, to)) = selection_span(&range, at(r), rect.w) {
                                let w = (to + 1).saturating_sub(from) as f32;
                                renderer.rect(
                                    (rect.x + from) as f32,
                                    (rect.y + r) as f32,
                                    w,
                                    1.0,
                                    th.selection,
                                );
                            }
                        }
                    }

                    // ファイルではカーソルは常にエンジンのもの。端末では入力中だけ
                    // シェルのカーソル（そこが「生きている末尾」だから）。
                    //
                    // 字を描く**前**に決める。合字の run をカーソルの位置で
                    // 切る必要があり、そのためには先に居場所が要る。
                    let cursor = if view.editing() || mode != Mode::Insert {
                        cursor_engine
                    } else {
                        let g = &view.term.state.grid;
                        Pos::new(g.cursor_absolute(), g.cursor.col)
                    };

                    // 開いているファイルの言語。端末には付けない
                    // （出力は SGR で色を持っているので、上から塗ると嘘になる）。
                    let lang = view.lang();

                    for r in 0..rect.h {
                        if let Some((start, end)) = view.fold_at(at(r)) {
                            // 畳んだ出力は 1 行の要約に置き換える。
                            // **何行隠したかを必ず出す。** 数が分からないと、
                            // 畳んだことに気づかないまま読み飛ばす。
                            let n = end + 1 - start;
                            let label = t!(
                                format!("▸ 出力 {n} 行（クリックで開く）"),
                                format!("▸ {n} lines of output (click to open)")
                            );
                            renderer.rect(
                                rect.x as f32,
                                (rect.y + r) as f32,
                                rect.w as f32,
                                1.0,
                                th.fold_bg,
                            );
                            renderer.text(
                                rect.x as f32,
                                (rect.y + r) as f32,
                                &truncate_width(&label, rect.w),
                                th.dim,
                                true,
                            );
                            continue;
                        }
                        let Some(cells) = doc.cells(at(r)) else {
                            break;
                        };
                        let syn = tsg_modal::highlight(lang, cells);
                        // 合字は**1 つの字形**なので、色が変わるところとカーソルの
                        // 居るところで run を切る。切らないと、途中で色を変えられず、
                        // カーソルの下の字が何だったか分からなくなる。
                        let caret = (is_active && cursor.line == at(r)).then_some(cursor.col);
                        let mut run = String::new();
                        let mut run_at = 0usize;
                        let mut run_fg = [0.0f32; 4];

                        for (c, cell) in cells.iter().enumerate().take(rect.w) {
                            if cell.is_spacer() {
                                continue;
                            }
                            let mut fg = match syn.get(c) {
                                Some(tsg_modal::Token::Comment) => th.syn_comment,
                                Some(tsg_modal::Token::Str) => th.syn_str,
                                Some(tsg_modal::Token::Number) => th.syn_num,
                                Some(tsg_modal::Token::Keyword) => th.syn_key,
                                Some(tsg_modal::Token::Added) => th.diff_add,
                                Some(tsg_modal::Token::Removed) => th.diff_del,
                                Some(tsg_modal::Token::DiffHead) => th.diff_head,
                                _ => cell_colors(&th, &cell.attrs).0,
                            };
                            if !is_active {
                                fg = th.fade(fg, 0.45);
                            }
                            let (x, y) = ((rect.x + c) as f32, (rect.y + r) as f32);

                            // この字を run に足せるか。
                            let joinable = cell.width == 1
                                && cell.text != " "
                                && cell.text.chars().count() == 1
                                && caret != Some(c);
                            let continues = joinable
                                && !run.is_empty()
                                && fg == run_fg
                                && run_at + run.chars().count() == c;
                            if !continues && !run.is_empty() {
                                renderer.glyph_run(
                                    (rect.x + run_at) as f32,
                                    y,
                                    &run,
                                    run_fg,
                                );
                                run.clear();
                            }
                            if joinable {
                                if run.is_empty() {
                                    run_at = c;
                                    run_fg = fg;
                                }
                                run.push_str(&cell.text);
                            } else if cell.text != " " {
                                renderer.glyph(x, y, &cell.text, fg);
                            }

                            let w = f32::from(cell.width.max(1));
                            if cell.attrs.has(Attrs::UNDERLINE) {
                                renderer.rect(x, y + 0.92, w, 0.06, fg);
                            }
                            if cell.attrs.has(Attrs::STRIKE) {
                                renderer.rect(x, y + 0.52, w, 0.06, fg);
                            }
                        }
                        if !run.is_empty() {
                            renderer.glyph_run(
                                (rect.x + run_at) as f32,
                                (rect.y + r) as f32,
                                &run,
                                run_fg,
                            );
                        }
                    }

                    if is_active
                        && let Some(cr) = cursor.line.checked_sub(top).filter(|r| *r < rect.h)
                        && cursor.col < rect.w
                    {
                        let (cx, cy) = ((rect.x + cursor.col) as f32, (rect.y + cr) as f32);
                        match mode {
                            Mode::Insert => renderer.rect(cx, cy, 0.15, 1.0, th.cursor),
                            _ => renderer.rect(cx, cy, 1.0, 1.0, th.cursor),
                        }
                        if !preedit.is_empty() {
                            renderer.text(cx, cy, &preedit, th.preedit, true);
                            let w = display_width(&preedit) as f32;
                            renderer.rect(cx, cy + 0.92, w, 0.08, th.preedit);
                        }
                    }
                }
            }
        }

        if !help {
            self.draw_tabs();
            self.draw_status(mode, &visible);
        }
        self.draw_images();
        self.draw_hints();
        self.draw_menu();
        self.draw_palette();
        self.draw_picker();

        if let Some(renderer) = self.renderer.as_mut()
            && let Err(e) = renderer.present()
        {
            eprintln!("描画に失敗: {e:#}");
        }
    }

    /// タブバー。1 枚しか無いときは行を使わない（`tab_rows`）。
    fn draw_tabs(&mut self) {
        let th = self.theme;
        if self.tab_rows() == 0 {
            return;
        }
        let spans = self.tab_spans();
        // 印だけは状態の色で塗り直す。字の形（●✕✓◍）だけだと、
        // 並んだときにどれが人待ちなのかが一瞬で分からない。
        let marks: Vec<Option<[f32; 4]>> = spans
            .iter()
            .map(|(_, _, id, ..)| self.tab_agent(*id).map(|a| Self::agent_color(&th, a)))
            .collect();
        let cols = self.cols;
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        renderer.rect(0.0, 0.0, cols as f32, 1.0, th.status_bg);
        for (i, (x, w, _, label, active)) in spans.iter().enumerate() {
            let (x, w, active) = (*x, *w, *active);
            if active {
                renderer.rect(x as f32, 0.0, w as f32, 1.0, th.tab_active);
            }
            renderer.text(x as f32, 0.0, label, if active { th.fg } else { th.dim }, true);
            // 「 1 ● 名前 」の ● は 3 桁目から（番号が 2 桁になっても 1 桁ずれるだけ）。
            if let Some(Some(c)) = marks.get(i) {
                let head: String = label.chars().take_while(|c| *c != '●' && *c != '✕' && *c != '✓' && *c != '◍').collect();
                let mark: String = label.chars().skip(head.chars().count()).take(1).collect();
                if !mark.is_empty() {
                    renderer.text((x + display_width(&head)) as f32, 0.0, &mark, *c, true);
                }
            }
        }
    }

    /// 次に飛ぶ先。**何度も押すと順に回る**ように、いま居るところより後ろを先に見る。
///
/// 3 本が同時に返事待ちのとき、押すたびに同じ 1 本へ戻るのでは
/// 「探す手間を消す」という目的を果たさない。
    fn next_waiting(waiting: &[u32], here: u32) -> Option<u32> {
        waiting
            .iter()
            .copied()
            .find(|p| *p > here)
            .or_else(|| waiting.first().copied())
    }

/// 状態の色。タブの印とステータスで同じものを使う。
    fn agent_color(th: &Theme, a: AgentState) -> [f32; 4] {
        match a {
            AgentState::Working => th.agent_working,
            AgentState::Blocked => th.agent_blocked,
            AgentState::Done => th.agent_done,
            AgentState::Failed => th.agent_failed,
            AgentState::Idle => th.dim,
        }
    }

    /// 端末に出た絵。**セルの上に重ねる**ので、文字と同じ格子に乗る。
    ///
    /// 置き場所はドキュメント絶対行なので、スクロールしても紙に貼った
    /// ように一緒に動く（`concept.md` の中心命題のまま）。
    fn draw_images(&mut self) {
        // どのペインの、どの絵を、どこへ出すか。借りを切ってから描く。
        struct Put {
            pane: u32,
            id: u64,
            col: f32,
            row: f32,
            w: f32,
            h: f32,
        }
        let mut puts: Vec<Put> = Vec::new();
        let mut uploads: Vec<(u32, u64, Vec<u8>, u32, u32)> = Vec::new();
        for id in self.session.visible_panes() {
            let Some(view) = self.session.panes.get(&id) else {
                continue;
            };
            if view.previewing() || view.editing() {
                continue; // ファイルを開いている間は端末の絵を出さない
            }
            let rect = view.text_rect();
            for img in &view.term.state.images {
                let Some(row) = img.line.checked_sub(view.top) else {
                    continue;
                };
                if row >= rect.h || img.col >= rect.w {
                    continue;
                }
                if !self.image_slots.contains_key(&(id, img.id)) {
                    uploads.push((id, img.id, img.rgba.clone(), img.width, img.height));
                }
                puts.push(Put {
                    pane: id,
                    id: img.id,
                    col: (rect.x + img.col) as f32,
                    row: (rect.y + row) as f32,
                    w: img.cols.min(rect.w - img.col) as f32,
                    h: img.rows.min(rect.h - row) as f32,
                });
            }
        }
        if puts.is_empty() {
            return;
        }
        let Some(r) = self.renderer.as_mut() else {
            return;
        };
        for (pane, img_id, rgba, w, h) in uploads {
            if let Some(slot) = r.upload_image(&rgba, w, h) {
                self.image_slots.insert((pane, img_id), slot);
            }
        }
        let Some(r) = self.renderer.as_mut() else {
            return;
        };
        for p in puts {
            if let Some(slot) = self.image_slots.get(&(p.pane, p.id)) {
                r.image(p.col, p.row, p.w, p.h, *slot);
            }
        }
    }

    /// ラベル。**中身の上に重ねる。** 隣に出すと桁がずれて、
    /// どれに付いたラベルなのか分からなくなる。
    fn draw_hints(&mut self) {
        if self.hints.is_empty() {
            return;
        }
        let th = self.theme;
        let typed = self.hint_typed.clone();
        let spots: Vec<(usize, usize, String, bool)> = self
            .hints
            .iter()
            .filter(|h| h.label.starts_with(&typed))
            .filter_map(|h| {
                let view = self.session.panes.get(&h.pane)?;
                let rect = view.text_rect();
                let row = h.line.checked_sub(view.top)?;
                (row < rect.h && h.col < rect.w).then(|| {
                    (
                        rect.x + h.col,
                        rect.y + row,
                        h.label.clone(),
                        !typed.is_empty(),
                    )
                })
            })
            .collect();
        let Some(r) = self.renderer.as_mut() else {
            return;
        };
        for (x, y, label, partial) in spots {
            let w = display_width(&label) as f32;
            r.rect(x as f32, y as f32, w, 1.0, th.hint_bg);
            let fg = if partial { th.hint_next } else { th.mode_fg };
            r.text(x as f32, y as f32, &label, fg, true);
        }
    }

    /// 右クリックメニュー。
    fn draw_menu(&mut self) {
        let th = self.theme;
        if !self.menu.open {
            return;
        }
        // 上に 1 行（タブ）、下に 1 行（ステータス）を空ける。
        // 入り切らない分は切り捨てず、`fit` がたどれる形に詰める。
        self.menu.fit(self.rows.saturating_sub(3));
        let (x, y) = self.menu.at;
        let (w, h) = (self.menu.width(), self.menu.height());
        // 画面からはみ出さない位置へ寄せる
        let x = x.min(self.cols.saturating_sub(w));
        // ステータス行に食い込ませない（最後の項目が押せなくなる）
        let y = y.min(self.rows.saturating_sub(h + 2));
        let (start, view) = self.menu.view();
        let rows: Vec<(overlay::Row, bool)> = view
            .iter()
            .enumerate()
            .map(|(i, r)| (r.clone(), self.menu.selected == Some(start + i)))
            .collect();
        let (above, below) = self.menu.hidden();
        self.menu.at = (x, y);

        let Some(r) = self.renderer.as_mut() else {
            return;
        };
        panel(r, &th, x, y, w, h);
        for (i, (row, sel)) in rows.iter().enumerate() {
            let ry = (y + i) as f32;
            match row {
                overlay::Row::Header(title) => {
                    r.text((x + 1) as f32, ry, title, th.accent, true);
                }
                overlay::Row::Item(it) => {
                    if *sel {
                        r.rect(x as f32, ry, w as f32, 1.0, th.panel_sel);
                    }
                    r.text((x + 2) as f32, ry, it.title, th.fg, true);
                    if !it.keys.is_empty() {
                        let kw = display_width(&it.keys);
                        r.text(
                            (x + w).saturating_sub(kw + 1) as f32,
                            ry,
                            &it.keys,
                            th.dim,
                            true,
                        );
                    }
                }
            }
        }
        // 隠れている分があることを見せる。**出ていない項目は無いのと同じ**なので、
        // せめて「まだ続く」と分かる印を置く。
        if above > 0 {
            r.text(x as f32, y as f32, "▲", th.accent, true);
        }
        if below > 0 {
            r.text(x as f32, (y + h).saturating_sub(1) as f32, "▼", th.accent, true);
        }
    }

    /// コマンドパレット。
    fn draw_palette(&mut self) {
        let th = self.theme;
        if !self.palette.open {
            return;
        }
        let (x, y) = self.palette_origin();
        let w = self.palette_width();
        // 検索は入力欄 1 行だけ。一覧を出すと、探している画面が隠れる。
        if self.palette.searching() {
            let back = matches!(self.palette.kind, overlay::PaletteKind::Search { back: true });
            let head = if back { "?" } else { "/" };
            let line = format!("{head} {}", self.palette.query);
            let hits = self.search_hits();
            let note = if self.palette.query.is_empty() {
                t!("探す文字を打ってください", "type what to look for").to_string()
            } else {
                t!(format!("{hits} 件"), format!("{hits} matches"))
            };
            let w = self.palette_width();
            let Some(r) = self.renderer.as_mut() else {
                return;
            };
            panel(r, &th, x, y, w, 2);
            r.rect(x as f32, y as f32, w as f32, 1.0, th.tab_active);
            r.text((x + 1) as f32, y as f32, &line, th.fg, true);
            let qw = display_width(&line);
            r.rect((x + 1 + qw) as f32, y as f32, 0.15, 1.0, th.cursor);
            r.text((x + 2) as f32, (y + 1) as f32, &note, th.dim, true);
            return;
        }

        let max_rows = self.rows.saturating_sub(y + 3).min(16);
        let total = self.palette.items().len();
        let shown = total.min(max_rows);
        let query = format!(": {}", self.palette.query);
        let q = self.palette.query.trim_start();
        let hint = if self.pending_pipe.is_some() {
            t!("通すコマンドを書いて Enter（例: sort / jq . / findstr x）",
                "type a command and press Enter (sort / jq . / findstr x)").to_string()
        } else if q.starts_with("e ") {
            t!("Enter でそのファイルを開きます", "Enter opens that file").to_string()
        } else if matches!(q, "w" | "q" | "q!" | "wq") || q.starts_with("w ") {
            t!("Enter で実行します", "Enter runs it").to_string()
        } else {
            t!("一致するものがありません", "nothing matches").to_string()
        };
        // 選んでいる項目が窓の中に来るように寄せてから切り取る。
        let selected = self.palette.selected;
        let (start, view) = self.palette.view(shown);
        let rows: Vec<(String, String, bool)> = view
            .iter()
            .enumerate()
            .map(|(i, it)| {
                (
                    it.title.to_string(),
                    it.keys.clone(),
                    start + i == selected,
                )
            })
            .collect();
        let below = total.saturating_sub(start + shown);

        let Some(r) = self.renderer.as_mut() else {
            return;
        };
        panel(r, &th, x, y, w, shown + 3);
        r.rect(x as f32, y as f32, w as f32, 1.0, th.tab_active);
        r.text((x + 1) as f32, y as f32, &query, th.fg, true);
        // カーソル
        let qw = display_width(&query);
        r.rect((x + 1 + qw) as f32, y as f32, 0.15, 1.0, th.cursor);

        if rows.is_empty() {
            // 一覧の絞り込みではなくコマンドを打っている最中は、そう言う。
            // 「一致するものがありません」だけ出ると、打ち間違えたのかと思う。
            r.text((x + 1) as f32, (y + 1) as f32, &hint, th.dim, true);
            return;
        }
        for (i, (title, keys, sel)) in rows.iter().enumerate() {
            let ry = (y + 1 + i) as f32;
            if *sel {
                r.rect(x as f32, ry, w as f32, 1.0, th.panel_sel);
            }
            r.text((x + 2) as f32, ry, title, th.fg, true);
            if !keys.is_empty() {
                let kw = display_width(keys);
                r.text((x + w).saturating_sub(kw + 1) as f32, ry, keys, th.dim, true);
            }
        }
        // 上下に隠れている件数。**「ほか N 件」だけだと下に降りられると思えない**ので、
        // いま何番目を見ているかも出す。
        if total > shown {
            let more = t!(
                format!("{} / {} 件（↑↓ でたどれます）", selected + 1, total),
                format!("{} of {} (use ↑↓)", selected + 1, total)
            );
            r.text((x + 2) as f32, (y + shown + 1) as f32, &more, th.dim, true);
            // 印は本文の左（x, x+1）に置く。題名は x+2 から始まるので重ならない。
            if start > 0 {
                r.text(x as f32, (y + 1) as f32, "▲", th.accent, true);
            }
            if below > 0 {
                r.text(x as f32, (y + shown) as f32, "▼", th.accent, true);
            }
        }
        // 使い方の 1 行。**一覧を出したのに動かし方が分からない**を作らない。
        let foot = t!(
            "↑↓ で選ぶ · Enter で実行 · Esc で閉じる · そのまま打つと絞り込み",
            "↑↓ choose · Enter run · Esc close · type to filter"
        );
        r.text((x + 2) as f32, (y + shown + 2) as f32, foot, th.dim, true);
    }

    /// 名前を選ぶだけの一覧（セッション切り替え）。
    fn draw_picker(&mut self) {
        let th = self.theme;
        if !self.picker.open {
            return;
        }
        let (x, y) = self.palette_origin();
        let w = self.palette_width();
        let max_rows = self.rows.saturating_sub(y + 3).min(16);
        let total = self.picker.items.len();
        let shown = total.min(max_rows);
        let title = self.picker.title.clone();
        let here = self.session_name.clone();
        let selected = self.picker.selected;
        let (start, view) = self.picker.view(shown);
        let rows: Vec<(String, bool, bool)> = view
            .iter()
            .enumerate()
            .map(|(i, name)| (name.clone(), start + i == selected, *name == here))
            .collect();

        let Some(r) = self.renderer.as_mut() else {
            return;
        };
        panel(r, &th, x, y, w, shown.max(1) + 1);
        r.rect(x as f32, y as f32, w as f32, 1.0, th.tab_active);
        r.text((x + 1) as f32, y as f32, &title, th.fg, true);
        if rows.is_empty() {
            r.text(
                (x + 1) as f32,
                (y + 1) as f32,
                t!("走っているセッションがありません", "no sessions are running"),
                th.dim,
                true,
            );
            return;
        }
        for (i, (name, sel, current)) in rows.iter().enumerate() {
            let ry = (y + 1 + i) as f32;
            if *sel {
                r.rect(x as f32, ry, w as f32, 1.0, th.panel_sel);
            }
            r.text((x + 2) as f32, ry, name, th.fg, true);
            if *current {
                r.text((x + w).saturating_sub(6) as f32, ry, t!("今ここ", "here"), th.dim, true);
            }
        }
    }

    /// ステータス行。**この 1 行が、初めて触る人にとっての説明書**になる。
    ///
    /// 記号を並べず、押せるものはボタンの形にして、今できることを言葉で書く。
    fn draw_status(&mut self, mode: Mode, visible: &[u32]) {
        let th = self.theme;
        let buttons = self.status_buttons();
        let recording = self.engine.macros.recording().is_some();

        // 子プロセスが入力を持っているときだけ出す。普段は静かにしておく。
        let owned_by_child = self
            .active_view()
            .is_some_and(|v| v.term.state.mouse_owner() == InputOwner::Child);

        let pending = self.engine.pending_hint();
        let hint = if !self.status_msg.is_empty() {
            self.status_msg.clone()
        } else if !pending.is_empty() {
            format!("{pending} …")
        } else {
            mode.hint().to_string()
        };

        let tabs = self.session.info.as_ref().map_or(0, |i| i.tabs.len());
        // ファイルを開いていれば、セッション名の代わりにファイル名を出す。`*` は未保存。
        let where_ = self
            .active_view()
            .and_then(PaneView::label)
            .unwrap_or_else(|| self.session_name.clone());
        let mut right = String::new();
        // エージェントが名乗った「いくら使ったか」。名乗らなければ出さない。
        if let Some(cost) = self
            .session
            .info
            .as_ref()
            .and_then(|i| i.panes.iter().find(|p| p.id == self.session.active))
            .and_then(|p| p.cost.clone())
        {
            right.push_str(&cost);
            right.push_str("  ");
        }
        if owned_by_child {
            right.push_str(t!("🖱 アプリ側  ", "🖱 app  "));
        }
        right.push_str(&where_);
        if visible.len() > 1 || tabs > 1 {
            right.push_str(&format!("  {}/{}", visible.len(), tabs));
        }
        right.push(' ');

        let status_row = self.rows.saturating_sub(1) as f32;
        let cols = self.cols;
        let mode_bg = Self::mode_color(&th, mode);

        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        renderer.rect(0.0, status_row, cols as f32, 1.0, th.status_bg);

        let mut x = 0usize;
        for (label, target) in &buttons {
            let w = display_width(label);
            match target {
                StatusTarget::Mode => {
                    renderer.rect(x as f32, status_row, w as f32, 1.0, mode_bg);
                    renderer.text(x as f32, status_row, label, th.mode_fg, true);
                }
                StatusTarget::Macro if recording => {
                    renderer.text(x as f32, status_row, label, th.rec_on, true);
                }
                StatusTarget::CloseFile => {
                    renderer.text(x as f32, status_row, label, th.accent, true);
                }
                StatusTarget::AgentNext => {
                    renderer.text(x as f32, status_row, label, th.agent_blocked, true);
                }
                StatusTarget::Preview => {
                    renderer.text(x as f32, status_row, label, th.accent, true);
                }
                _ => renderer.text(x as f32, status_row, label, th.status_fg, true),
            }
            x += w;
        }

        let rw = display_width(&right);
        renderer.text(
            cols.saturating_sub(rw) as f32,
            status_row,
            &right,
            th.dim,
            true,
        );

        // 説明は残った幅に収める。切れるくらいなら出さない、はしない
        // （出ないより、途中で切れてでも出ているほうが手がかりになる）。
        let room = cols.saturating_sub(rw).saturating_sub(x + 3);
        if room > 8 {
            renderer.text(
                (x + 2) as f32,
                status_row,
                &truncate_width(&hint, room),
                th.dim,
                true,
            );
        }
    }

    /// 設定ファイルを読み直して、変えられるものを当てる。
    ///
    /// **開かなくなるより、古い設定で動き続けるほうがまし。** 読めない設定は
    /// 当てずに、何が悪いかだけ画面に出す（起動時は既定へ倒すが、動いている
    /// 途中で既定へ戻すと、それまでの見た目まで巻き戻って驚かせる）。
    fn reload_config(&mut self) -> bool {
        let (mut next, warning) = Config::load();
        if let Some(w) = warning {
            self.status_msg = format!("{}{w}", t!("設定: ", "config: "));
            return true;
        }
        // コマンドラインの指定は読み直しても勝つ。設定ファイルに引きずられて
        // `--opacity 1.0` で開いた窓が勝手に透け始める、を起こさない。
        next.override_with(&self.cli);
        if next == self.cfg {
            return false;
        }

        // 言語だけは変えない。`t!` は起動時に決めた値を見るので、途中で
        // 変えると画面の一部だけ古い言語のまま残る。
        let lang_changed = next.lang != self.cfg.lang;
        next.lang = self.cfg.lang;
        // 幅も同じ理由で据え置く（桁の勘定が変わると既存のグリッドと食い違う）。
        let width_changed = next.ambiguous_width != self.cfg.ambiguous_width;
        next.ambiguous_width = self.cfg.ambiguous_width;

        let font_changed = (next.font_size - self.cfg.font_size).abs() > 0.01;
        let scrollback_changed = next.scrollback != self.cfg.scrollback;
        let blur_changed = next.blur != self.cfg.blur;
        self.cfg = next;
        self.theme = self.cfg.theme;

        if scrollback_changed {
            session::set_scrollback(self.cfg.scrollback);
            for view in self.session.panes.values_mut() {
                view.term.state.grid.set_max_scrollback(self.cfg.scrollback);
            }
        }
        if let Some(r) = self.renderer.as_mut() {
            r.background = background_of(&self.theme, self.cfg.opacity);
            r.set_ligatures(self.cfg.ligatures);
            if font_changed {
                r.set_font_size(self.cfg.font_size);
            }
        }
        if blur_changed
            && let Some(w) = &self.window
        {
            platform::decorate(w.as_ref(), self.cfg.blur);
        }
        if font_changed {
            self.resize_window();
        }

        self.status_msg = if lang_changed || width_changed {
            t!(
                "設定を読み直した（言語と文字幅は次に開いたときから）",
                "config reloaded (language and char width apply next launch)"
            )
            .to_string()
        } else {
            t!("設定を読み直した", "config reloaded").to_string()
        };
        true
    }

    /// 設定ファイルを開く。**開いて保存すればその場で効く**（`reload`）ので、
    /// 設定を詰める道がこれ 1 本でつながる。まだ無ければ空で開く。
    fn open_config(&mut self) {
        let Some(path) = self.watch.path().map(std::path::Path::to_path_buf) else {
            self.status_msg = t!(
                "設定ファイルの場所が分かりません",
                "cannot locate the config file"
            )
            .to_string();
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // まだ無ければ雛形を置いてから開く。**空のファイルが出てくると、
        // 何が書けるのか分からないまま行き止まる**（実機で行き止まった）。
        if !path.exists() {
            let _ = std::fs::write(&path, config::template());
        }
        self.open_file(&path.to_string_lossy());
    }

    /// 配色を差し替える。
    ///
    /// **クリア色も一緒に変える。** ここを忘れると、セルは新しい色で描かれるのに
    /// 何も無い部分だけ古い背景のまま残る。
    fn apply_theme(&mut self, name: &str) {
        if !self.cfg.set_theme(name) {
            self.status_msg = t!(
                "その配色は知りません: ",
                "no such theme: "
            )
            .to_string()
                + name;
            return;
        }
        self.theme = self.cfg.theme;
        if let Some(r) = self.renderer.as_mut() {
            r.background = background_of(&self.theme, self.cfg.opacity);
        }
        self.status_msg = format!(
            "{}{}",
            t!("配色: ", "theme: "),
            self.cfg.theme_name
        );
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn resize_window(&mut self) {
        let Some(renderer) = self.renderer.as_ref() else {
            return;
        };
        let (cols, rows) = renderer.grid_size();
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.sync_layout();
    }

    fn update_ime_area(&self) {
        let (Some(w), Some(r), Some(v)) = (&self.window, &self.renderer, self.active_view()) else {
            return;
        };
        let (cw, ch) = r.cell_size();
        let x = (v.rect.x + v.term.state.grid.cursor.col) as f32 * cw;
        let y = (v.rect.y + v.term.state.grid.cursor.row + 1) as f32 * ch;
        w.set_ime_cursor_area(
            PhysicalPosition::new(x as i32, y as i32),
            PhysicalSize::new(cw as u32 * 20, ch as u32),
        );
    }
}

// ---------------------------------------------------------------------------

/// その行で選択されている列の範囲（両端含む）。ペイン幅で切り詰める。
fn selection_span(range: &Range, line: usize, cols: usize) -> Option<(usize, usize)> {
    if line < range.start.line || line > range.end.line {
        return None;
    }
    let last = cols.saturating_sub(1);
    Some(match range.kind {
        RangeKind::Line => (0, last),
        RangeKind::Block => {
            let (a, b) = (range.start.col, range.end.col);
            (a.min(b).min(last), a.max(b).min(last))
        }
        RangeKind::Char => match (line == range.start.line, line == range.end.line) {
            (true, true) => (range.start.col.min(last), range.end.col.min(last)),
            (true, false) => (range.start.col.min(last), last),
            (false, true) => (0, range.end.col.min(last)),
            (false, false) => (0, last),
        },
    })
}

fn to_key_input(key: &Key, mods: ModifiersState, mode: Mode) -> Option<KeyInput> {
    match key {
        Key::Named(NamedKey::Escape) => Some(KeyInput::Esc),
        Key::Named(NamedKey::Enter) => Some(KeyInput::Enter),
        Key::Named(NamedKey::Backspace) => Some(KeyInput::Backspace),
        Key::Named(NamedKey::Tab) => Some(KeyInput::Tab),
        Key::Named(NamedKey::Space) => Some(KeyInput::Char(' ')),
        Key::Named(NamedKey::F1) => Some(KeyInput::Function(1)),
        // Normal 系では矢印もモーションとして扱う（マウス派・初見の人向けの導線）。
        Key::Named(NamedKey::ArrowLeft) if mode != Mode::Insert => Some(KeyInput::Char('h')),
        Key::Named(NamedKey::ArrowDown) if mode != Mode::Insert => Some(KeyInput::Char('j')),
        Key::Named(NamedKey::ArrowUp) if mode != Mode::Insert => Some(KeyInput::Char('k')),
        Key::Named(NamedKey::ArrowRight) if mode != Mode::Insert => Some(KeyInput::Char('l')),
        Key::Character(s) => {
            let c = s.chars().next()?;
            if mods.control_key() {
                Some(KeyInput::Ctrl(c.to_ascii_lowercase()))
            } else {
                Some(KeyInput::Char(c))
            }
        }
        _ => None,
    }
}

fn display_width(s: &str) -> usize {
    s.chars()
        .map(tsg_term::width_of)
        .sum()
}

/// モード別に「いま押せるキー」を返す。
///
/// 起動しただけでは何をすればいいか分からない、という状態を作らないための一行。
/// ここを削るなら、代わりの導線を必ず用意すること。
/// ドラッグで作る範囲。クリック回数で粒度が変わる（`mouse-parity.md` §3）。
///
/// 単語粒度が `textobj::at_pointer` を通るので、
/// 「`src/main.rs:42` の上でドラッグするとパス全体から始まる」が自然に出る。
/// 画面上の「開けるもの」1 つ。
#[derive(Clone, Debug)]
struct Hint {
    pane: u32,
    line: usize,
    col: usize,
    text: String,
    label: String,
}

/// 「開く」対象の種別。ホバーの下線と Ctrl＋クリックで同じ判定を使う。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenKind {
    Url,
    Path,
    Hash,
}

/// その文字列は開けるか。
///
/// ここを甘くすると**画面中の語に下線が出て読めなくなる**ので、
/// 形がはっきりしているものだけを通す。
fn open_kind(text: &str) -> Option<OpenKind> {
    let t = text.trim();
    if t.is_empty() || t.contains(char::is_whitespace) {
        return None;
    }
    if t.starts_with("http://") || t.starts_with("https://") || t.starts_with("file://") {
        return Some(OpenKind::Url);
    }
    // パスらしさ: 区切りを含むか、拡張子が付いている
    let head = strip_position(t);
    let looks_path = head.contains('/')
        || head.contains('\\')
        || head
            .rsplit_once('.')
            .is_some_and(|(name, ext)| {
                !name.is_empty() && (1..=6).contains(&ext.len()) && ext.chars().all(char::is_alphanumeric)
            });
    if looks_path {
        return Some(OpenKind::Path);
    }
    // git のハッシュ。短縮でも 7 桁は要る（数字だけの語を拾わない）
    if (7..=40).contains(&t.len())
        && t.chars().all(|c| c.is_ascii_hexdigit())
        && t.chars().any(|c| c.is_ascii_alphabetic())
    {
        return Some(OpenKind::Hash);
    }
    None
}

/// `src/main.rs:42:8` の行・桁を落として、パスだけにする。
///
/// 単純に最初の `:` で切ると **`C:\dev\x` のドライブ文字で切れる**（実際に踏んだ）。
/// 後ろから、数字だけの部分だけを外す。
fn strip_position(text: &str) -> &str {
    let mut head = text;
    while let Some((rest, tail)) = head.rsplit_once(':') {
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) && rest.len() > 1 {
            head = rest;
        } else {
            break;
        }
    }
    head
}

/// OS の既定のアプリで開く。
fn open_in_os(target: &str) -> std::io::Result<()> {
    let mut cmd = if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        // `start` の第1引数はウィンドウ名として食われるので空文字を置く
        c.args(["/C", "start", "", target]);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(target);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(target);
        c
    };
    cmd.spawn().map(|_| ())
}

fn drag_range(
    buf: &dyn tsg_modal::Buffer,
    from: Pos,
    to: Pos,
    grain: mouse::Grain,
    block: bool,
) -> Range {
    if block {
        return Range::new(from, to, RangeKind::Block);
    }
    match grain {
        mouse::Grain::Line => Range::new(from, to, RangeKind::Line),
        mouse::Grain::Cell => Range::new(from, to, RangeKind::Char),
        mouse::Grain::Word => {
            let at = |p: Pos| {
                tsg_modal::textobj::at_pointer(buf, p)
                    .unwrap_or_else(|| Range::new(p, p, RangeKind::Char))
            };
            let (a, b) = (at(from), at(to));
            Range::new(a.start.min(b.start), a.end.max(b.end), RangeKind::Char)
        }
    }
}

/// 範囲を外部コマンドへ通す。
///
/// シェル経由で走らせる。`jq .` や `sort -u` のようにユーザーが書くのは
/// **シェルの文法**であって argv ではないため、自前で分解しない。
fn pipe_through(command: &str, input: &str, cwd: Option<&str>) -> std::io::Result<String> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let mut cmd = if cfg!(windows) {
        let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
        let mut c = Command::new(shell);
        c.arg("/C").arg(command);
        c
    } else {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut c = Command::new(shell);
        c.arg("-c").arg(command);
        c
    };
    if let Some(dir) = cwd.filter(|d| std::path::Path::new(d).is_dir()) {
        cmd.current_dir(dir);
    }

    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut sink) = child.stdin.take() {
        sink.write_all(input.as_bytes())?;
    }
    let out = child.wait_with_output()?;

    // 失敗しても標準エラーを見せる。黙って空を返すと何が起きたか分からない。
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if !err.trim().is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&err);
        }
    }
    Ok(text)
}

/// `file://host/C:/dev/x`（OSC 7）をパスへ戻す。
fn file_url_to_path(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    let path = rest.split_once('/').map_or(rest, |(_, p)| p);
    let cleaned = path.trim_start_matches('/');
    let out = if cleaned.chars().nth(1) == Some(':') {
        cleaned.to_string()
    } else {
        path.to_string()
    };
    std::path::Path::new(&out).is_dir().then_some(out)
}

/// 一覧の下地。枠を1本引くだけで、下の本文と混ざらなくなる。
fn panel(r: &mut Renderer, th: &Theme, x: usize, y: usize, w: usize, h: usize) {
    r.rect(x as f32, y as f32, w as f32, h as f32, th.panel_bg);
    r.rect(x as f32, y as f32, w as f32, 0.06, th.panel_edge);
    r.rect(x as f32, (y + h) as f32 - 0.06, w as f32, 0.06, th.panel_edge);
    r.rect(x as f32, y as f32, 0.06, h as f32, th.panel_edge);
    r.rect((x + w) as f32 - 0.06, y as f32, 0.06, h as f32, th.panel_edge);
}

/// 左クリック 1 回が何をするか。
#[derive(Debug, PartialEq, Eq)]
enum ClickIntent {
    /// カーソルを置くだけ。モードは変えない
    MoveOnly,
    /// 生きた末尾なので入力へ戻る
    ToInsert,
    /// 過去の出力なので読むモードへ
    ToNormal,
}

/// `mouse-parity.md` §4.1。
///
/// **ファイルではモードを変えない。** エディタで文字を置きに行っただけで
/// 入力モードに入るのは事故で、しかも端末のカーソル位置で判断していたので
/// 開いているファイルの内容と無関係に切り替わっていた（実機で踏んだ）。
fn click_intent(editing: bool, clicked_line: usize, live_line: usize) -> ClickIntent {
    if editing {
        ClickIntent::MoveOnly
    } else if clicked_line >= live_line {
        ClickIntent::ToInsert
    } else {
        ClickIntent::ToNormal
    }
}

/// ステータス行のどこを押したか。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusTarget {
    /// 常設のパレットボタン（`mouse-parity.md` §4.7 の最終保証）
    Palette,
    Help,
    /// モードの帯。押すと入力 ⇄ 読むを行き来する
    Mode,
    /// マクロの記録 / 再生（`mouse-parity.md` §4.6）
    Macro,
    /// 入力の所有権を取り返す
    Ownership,
    /// エディタを閉じて端末へ戻る
    CloseFile,
    /// 次の「人の番」のエージェントへ飛ぶ
    AgentNext,
    /// Markdown を読む形にする / 素に戻す
    Preview,
}

/// ボタンの並びから当たりを引く。列の計算を描画と共有するのが要点。
///
/// 左端の 1 桁はウィンドウのリサイズ枠に食われてクリックが届かない（実機で確かめた）。
/// どのボタンも前後に空白を 1 桁持たせてあるので、字は必ず 1 桁内側に出る。
fn status_hit(buttons: &[(String, StatusTarget)], col: usize) -> StatusTarget {
    let mut x = 0usize;
    for (label, target) in buttons {
        x += display_width(label);
        if col < x {
            return *target;
        }
    }
    StatusTarget::Ownership
}

/// 背景へ向けて色を寄せる。非アクティブなペインを沈ませるのに使う。
/// 灰色に潰さず寄せるだけなので、どのペインでも色の意味は読める。
/// セルの SGR を (前景, 背景) の RGBA へ落とす。背景が `None` なら塗らない。
///
/// `Color::Default` の解決先はここにしかない。テーマを差し替えるならここ。
fn cell_colors(th: &Theme, attrs: &Attrs) -> ([f32; 4], Option<[f32; 4]>) {
    let rev = attrs.has(Attrs::REVERSE);
    let (fg_c, bg_c) = attrs.resolved();
    let fg_c = if attrs.has(Attrs::BOLD) {
        fg_c.brighten()
    } else {
        fg_c
    };

    // 既定色の反転は、テーマの前景と背景を入れ替えた形になる。
    let mut fg = th.resolve(fg_c).unwrap_or(if rev { th.bg } else { th.fg });
    let bg = match th.resolve(bg_c) {
        Some(v) => Some(v),
        None if rev => Some(th.fg),
        None => None,
    };

    if attrs.has(Attrs::DIM) {
        fg = th.fade(fg, 0.45);
    }
    if attrs.has(Attrs::HIDDEN) {
        fg = bg.unwrap_or(th.bg);
    }
    (fg, bg)
}

/// 表示幅で切り詰める。全角を半分で割らない。
fn truncate_width(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = tsg_term::width_of(c);
        if w + cw > max.saturating_sub(1) {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('\u{2026}');
    out
}

/// 使い方の全画面表示。キー一覧は `REGISTRY` から生成するので、
/// コマンドを足したのにヘルプが古いまま、が起きない。
/// 使い方の全画面表示。
///
/// **キーの一覧から書かない。** 初めて開いた人が知りたいのは
/// 「マウスで何ができるか」と「モードとは何か」で、キーはその後でいい。
/// 全部の一覧はコマンドパレットが持っているので、ここでは代表だけ出す。
/// 使い方の 1 行。**先に組み立ててから窓に切る**ので、入り切らない行が
/// 黙って消えることがない。
enum HelpLine {
    Title(&'static str),
    Note(&'static str),
    Pair(&'static str, &'static str),
    Blank,
}

/// 使い方の中身。マウスでできることから始める。
fn help_lines() -> Vec<HelpLine> {
    use HelpLine::{Blank, Note, Pair, Title};
    let mut v = vec![
        Title("tsumugi"),
        Note(t!(
            "ターミナルの画面を、そのまま読んで・選んで・編集できます",
            "read, select and edit the terminal screen itself"
        )),
        Blank,
        Title(t!("■ マウスだけで使えます", "■ The mouse is enough")),
    ];
    for (a, b) in [
        (
            t!("クリック", "click"),
            t!("そこにカーソルを置く（過去の出力なら読むモードへ）", "put the cursor there"),
        ),
        (
            t!("ダブルクリック", "double-click"),
            t!("語・パス・URL をまるごと選ぶ", "select a word, path or URL as one"),
        ),
        (
            t!("ドラッグ", "drag"),
            t!("範囲を選ぶ（そのあと右クリック）", "select a range, then right-click"),
        ),
        (
            t!("右クリック", "right-click"),
            t!("いまできることの一覧", "what you can do right now"),
        ),
        (
            t!("左のふち", "left edge"),
            t!("コマンドとその出力をまるごと選ぶ（赤は失敗）", "select a whole command block (red = failed)"),
        ),
        (
            t!("Ctrl＋クリック", "Ctrl+click"),
            t!("パスを開く / URL をブラウザで開く", "open a path or URL"),
        ),
        (
            t!("下の ≡", "the ≡ below"),
            t!("すべてのコマンド（打って絞り込めます）", "every command, searchable"),
        ),
        (
            t!("下のモードの帯", "the mode chip"),
            t!("押すと 入力 ⇄ 読む が切り替わる", "click to toggle typing / reading"),
        ),
    ] {
        v.push(Pair(a, b));
    }
    v.push(Blank);
    v.push(Title(t!("■ 2 つのモードがあります", "■ Two modes")));
    for (a, b) in [
        (
            t!("入力", "typing"),
            t!("打った文字がそのままシェルへ行く。普通のターミナル", "keys go to the shell, like any terminal"),
        ),
        (
            t!("読む", "reading"),
            t!("キーが操作になる。j は「1 行下へ」の意味", "keys become commands; j means one line down"),
        ),
        (
            "Esc  /  i",
            t!("入力 → 読む  /  読む → 入力", "typing → reading  /  reading → typing"),
        ),
        (
            t!("（読むモードの間は日本語入力が自動で切れます）", "(IME turns off while reading)"),
            "",
        ),
    ] {
        v.push(Pair(a, b));
    }
    v.push(Blank);
    v.push(Title(t!("■ ファイルを開いたら", "■ Once a file is open")));
    for (a, b) in [
        (
            t!("下の ✕ 端末へ戻る", "the ✕ below"),
            t!("エディタを閉じて、元のシェルへ戻る", "close the editor and go back to the shell"),
        ),
        (":w  /  :q", t!("保存 / 端末へ戻る", "save / back to the shell")),
        (
            t!("下の ◱ 読む形", "the ◱ below"),
            t!("Markdown を読む形にする（Space m でも）", "render Markdown (or Space m)"),
        ),
    ] {
        v.push(Pair(a, b));
    }
    v.push(Blank);
    v.push(Title(t!(
        "■ AI エージェントを並べて使うなら",
        "■ Running AI agents side by side"
    )));
    for (a, b) in [
        (
            t!("下の ● 返事待ち", "the ● waiting count"),
            t!("待っているエージェントの数。押すとそこへ飛ぶ", "agents waiting for you; click to jump"),
        ),
        (
            t!("タブの ● ✓ ✕ ◍", "● ✓ ✕ ◍ on a tab"),
            t!("返事待ち / 終わった / 失敗 / 動いている", "waiting / done / failed / working"),
        ),
        ("Space a", t!("次の返事待ちへ飛ぶ", "jump to the next agent waiting")),
        ("Space f", t!("画面に出てきたファイルの一覧", "files mentioned on this screen")),
        ("[a  ]a", t!("前 / 次の発話へ", "previous / next agent message")),
        (
            t!("（先に一度だけ）", "(once, up front)"),
            "tsg --install-agent-hooks",
        ),
    ] {
        v.push(Pair(a, b));
    }
    v.push(Blank);
    v.push(Title(t!("■ キーで速くしたくなったら", "■ When you want to go faster")));
    for (a, b) in [
        ("j  k", t!("下 / 上へ", "down / up")),
        ("V  →  y", t!("行を選んでコピー", "select a line, then copy")),
        ("[[  ]]", t!("前 / 次のコマンドへ", "previous / next command")),
        ("!", t!("選んだものをプロンプトへ送る", "send the selection to the prompt")),
        (":e path", t!("ファイルを開く（このペインがエディタになる）", "open a file in this pane")),
        ("Space", t!("画面を分割・切り替え", "split and switch panes")),
        ("F1", t!("この画面", "this screen")),
    ] {
        v.push(Pair(a, b));
    }
    v.push(Blank);
    v.push(Note(t!(
        "ウィンドウを閉じてもシェルは死にません。開き直せば続きから使えます。",
        "closing the window does not kill your shells; reopen to continue"
    )));
    v
}

/// 使い方。`scroll` 行目から、入るだけ描く。
///
/// **最後の 1 行（閉じ方）は必ず出す。** ここが切れると、開いた人が
/// 閉じ方を知らないまま取り残される（低いウィンドウで実際に切れていた）。
fn draw_help(renderer: &mut Renderer, th: &Theme, cols: usize, rows: usize, scroll: usize) {
    let (title, body, note, panel) = (th.help_title, th.help_body, th.help_note, th.help_bg);
    renderer.rect(0.0, 0.0, cols as f32, rows as f32, panel);

    let lines = help_lines();
    let x = 4.0;
    let col2 = (cols / 3).clamp(18, 30) as f32;
    // 上下に 1 行ずつ余白、最下行は閉じ方に使う。
    let avail = rows.saturating_sub(3);
    let scroll = scroll.min(lines.len().saturating_sub(avail));

    for (i, l) in lines.iter().skip(scroll).take(avail).enumerate() {
        let ry = (i + 1) as f32;
        match l {
            HelpLine::Title(t) => renderer.text(x, ry, t, title, true),
            HelpLine::Note(t) => renderer.text(x, ry, t, note, true),
            HelpLine::Pair(a, b) => {
                renderer.text(x + 2.0, ry, a, body, true);
                renderer.text(x + 2.0 + col2, ry, b, note, true);
            }
            HelpLine::Blank => {}
        }
    }

    let rest = lines.len().saturating_sub(scroll + avail);
    let foot = if rest > 0 || scroll > 0 {
        t!(
            format!("↑↓ でスクロール（あと {rest} 行） · どれかキーを押す / クリックすると閉じます"),
            format!("↑↓ to scroll ({rest} more) · press any key or click to close")
        )
    } else {
        t!(
            "どれかキーを押す / クリックすると閉じます".to_string(),
            "press any key or click to close".to_string()
        )
    };
    renderer.text(x, rows.saturating_sub(1) as f32, &foot, note, true);
}

// ---------------------------------------------------------------------------

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        // 絵は生 RGBA で埋め込んである（実行時に画像デコーダを持ち込まないため）。
        let icon = winit::window::Icon::from_rgba(
            include_bytes!("../../../assets/icon.rgba").to_vec(),
            256,
            256,
        )
        .ok();
        let attrs = Window::default_attributes()
            .with_title("tsumugi")
            .with_window_icon(icon)
            .with_transparent(self.cfg.transparent())
            .with_inner_size(LogicalSize::new(1100.0, 700.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("ウィンドウの作成に失敗: {e}");
                event_loop.exit();
                return;
            }
        };

        // OS 側の装飾（暗いタイトルバー・背景のぼかし）。効かない環境では何も起きない。
        platform::decorate(&window, self.cfg.blur);

        let size = window.inner_size();
        let transparent = self.cfg.transparent();
        let renderer = match Renderer::new(
            window.clone(),
            size.width,
            size.height,
            self.cfg.font_size,
            transparent,
        ) {
            Ok(mut r) => {
                // `Color::Default` の解決先とクリア色を別々に持たせない
                r.background = background_of(&self.theme, self.cfg.opacity);
                r.set_ligatures(self.cfg.ligatures);
                r
            }
            Err(e) => {
                eprintln!("描画の初期化に失敗: {e:#}");
                event_loop.exit();
                return;
            }
        };

        print_font_diagnostics(&renderer);
        if self.diagnose {
            let _ = std::io::stdout().flush();
            event_loop.exit();
            return;
        }

        let (cols, rows) = renderer.grid_size();
        self.window = Some(window.clone());
        self.renderer = Some(renderer);
        self.cols = cols;
        self.rows = rows;

        match connect_or_spawn(&self.session_name) {
            Ok(client) => {
                self.client = Some(client);
                let area = self.area();
                self.send_msg(&ClientMsg::Attach {
                    version: PROTOCOL_VERSION,
                    cols: area.w as u16,
                    rows: area.h as u16,
                    cwd: self.cwd.clone(),
                    command: self.command.clone(),
                });
            }
            Err(e) => {
                eprintln!("mux サーバへ接続できません: {e:#}");
                event_loop.exit();
                return;
            }
        }

        self.sync_ime();
        // 起動しただけで何をすればいいか分からない、を起こさない。
        // **初回だけ。** 毎回出すと、読み終えた人には邪魔なだけになる。
        // 二度目からは `F1` と下の `? 使い方` から出す。
        if config::take_first_run() {
            self.engine.set_help_visible(true);
        }
        window.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(size) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(size.width, size.height);
                }
                self.resize_window();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }

            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),

            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    let text = event.text.as_ref().map(ToString::to_string);
                    self.on_key(event_loop, event.logical_key.clone(), text);
                    self.update_ime_area();
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                self.pointer = position;
                if self.drag.is_some() {
                    self.on_mouse_move(event_loop);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                } else {
                    // 掴んでいないときは、下にある対象を拾って下線を引く（§3）。
                    self.update_hover();
                }
            }

            WindowEvent::MouseInput { state, button, .. } => {
                match state {
                    ElementState::Pressed => self.on_mouse_press(button, event_loop),
                    ElementState::Released => self.on_mouse_release(button),
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                self.on_wheel(delta);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }

            WindowEvent::Ime(ime) => {
                match ime {
                    Ime::Enabled | Ime::Disabled => self.preedit.clear(),
                    Ime::Preedit(text, _) => self.preedit = text,
                    Ime::Commit(text) => {
                        self.preedit.clear();
                        if self.engine.mode() == Mode::Insert {
                            self.send_input(text.as_bytes());
                        }
                    }
                }
                self.update_ime_area();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }

            WindowEvent::Focused(on) => {
                self.focused = on;
                if on && let Some(w) = &self.window {
                    w.request_redraw();
                }
            }

            WindowEvent::RedrawRequested => self.draw(),

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let mut dirty = self.pump();
        if self.watch.changed() {
            dirty |= self.reload_config();
        }
        if dirty
            && let Some(w) = &self.window
        {
            w.request_redraw();
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(8),
        ));
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // 🔴 サーバは落とさない。ウィンドウを閉じてもシェルは生き続ける。
        self.send_msg(&ClientMsg::Detach);
        let _ = std::io::stdout().flush();
    }
}




fn connect_or_spawn(session: &str) -> Result<Client> {
    if let Ok(c) = Client::connect(session) {
        println!("既存の mux セッション '{session}' に再接続しました");
        return Ok(c);
    }
    println!("mux セッション '{session}' を新しく起こします");

    let exe = std::env::current_exe().context("自分の実行ファイルの場所が分かりません")?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("--server").arg(session);
    detach(&mut cmd);
    cmd.spawn()
        .with_context(|| format!("mux サーバを起こせません: {}", exe.display()))?;

    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(50));
        if let Ok(c) = Client::connect(session) {
            println!("mux サーバを起動しました");
            return Ok(c);
        }
    }
    bail!("mux サーバが 5 秒以内に応答しませんでした")
}

/// サーバ子プロセスを親から切り離す。
///
/// これを忘れると、GUI が強制終了されたときにサーバも道連れになり、
/// 「ウィンドウを閉じてもシェルは死なない」という約束が破れる（実際に踏んだ）。
#[cfg(windows)]
fn detach(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(unix)]
fn detach(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // 新しいプロセスグループへ。端末からのシグナルを一緒に受けないようにする。
    cmd.process_group(0);
}

#[cfg(not(any(windows, unix)))]
fn detach(_cmd: &mut std::process::Command) {}

fn print_font_diagnostics(renderer: &Renderer) {
    let f = &renderer.fonts;
    let (cw, ch) = renderer.cell_size();
    println!("tsumugi");
    println!("OS: {} / {}", std::env::consts::OS, std::env::consts::ARCH);
    let chain: Vec<String> = f
        .families()
        .iter()
        .zip(f.scales())
        .map(|(name, scale)| {
            if (scale - 1.0).abs() < 0.001 {
                (*name).to_string()
            } else {
                format!("{name}(x{scale:.3})")
            }
        })
        .collect();
    println!("フォント: {}", chain.join(" -> "));
    println!("セル: {cw:.1} x {ch:.1} px (font {:.0}px)", f.px);
    println!("  ※ 括弧内はセル格子へ合わせるための伸縮倍率");

    // **設定を on にしただけでは合字は出ない。** 字体が持っていなければ出ない
    // （Consolas も SF Mono も持っていない）。字体の話だと分かるように出す。
    let base = f.families().first().copied().unwrap_or("?");
    let ligs = f.ligature_support();
    let total = tsg_render::font::LIGATURE_PROBES.len();
    if ligs == 0 {
        println!("合字: {base} は合字を持っていません（設定を on にしても出ません）");
    } else {
        println!("合字: {base} で {ligs}/{total} 組める");
    }

    println!("\nCJK 幅検査（送り幅がセル幅の何倍か）:");

    let mut all_ok = true;
    for (c, want, label) in [
        ('M', 1.0, "ASCII"),
        ('日', 2.0, "CJK"),
        ('あ', 2.0, "かな"),
        ('※', 2.0, "Ambiguous"),
    ] {
        match f.advance_of(c) {
            Some(adv) => {
                let ratio = adv / cw;
                let ok = (ratio - want).abs() < 0.15;
                all_ok &= ok;
                println!(
                    "  {label:<10} '{c}'  送り幅 {adv:>5.1}px = {ratio:.2} セル  期待 {want:.0}  {}",
                    if ok { "OK" } else { "ズレ" }
                );
            }
            None => {
                all_ok = false;
                println!("  {label:<10} '{c}'  グリフが見つかりません");
            }
        }
    }
    println!(
        "判定: {}",
        if all_ok {
            "🟢 フォントの送り幅はセル格子と整合"
        } else {
            "🟡 送り幅がセル格子とズレる文字がある（フォント選定を見直す）"
        }
    );
    println!();
}

/// 入れた / 外した結果を出す。**何を変えたかを黙らない。**
fn report_install(r: Result<install::Report>) -> Result<()> {
    let r = r?;
    for line in &r.done {
        println!("  ✓ {line}");
    }
    for line in &r.notes {
        println!("  · {line}");
    }
    if r.done.is_empty() {
        println!("  何も変えるものがありませんでした");
    }
    Ok(())
}

/// シェル統合のスクリプトを標準出力へ。`eval "$(tsg --shell-integration bash)"` 用。
fn print_shell_integration(name: Option<&str>) -> Result<()> {
    let shell = pick_shell(name)?;
    print!("{}", shell.script());
    Ok(())
}

fn install_shell_integration(name: Option<&str>) -> Result<()> {
    let shell = pick_shell(name)?;
    println!("{}", shell::install(shell)?);
    Ok(())
}

/// 名前が無ければ今のシェルを見る。分からなければ**黙って諦めず**候補を出す。
fn pick_shell(name: Option<&str>) -> Result<shell::Shell> {
    match name {
        Some(n) => shell::Shell::parse(n)
            .with_context(|| format!("{n} 向けのシェル統合はありません（bash / zsh / fish / pwsh / nu）")),
        None => shell::Shell::detect().context(
            "今のシェルが分かりません。名前を渡してください（bash / zsh / fish / pwsh / nu）",
        ),
    }
}

fn main() -> Result<()> {
    // std が標準出力のハンドルを掴む前に繋ぐ。
    platform::attach_parent_console();

    let cli = cli::parse(std::env::args().skip(1));
    match &cli.mode {
        cli::Mode::Help => {
            print!("{}", cli::HELP);
            return Ok(());
        }
        cli::Mode::Version => {
            println!("tsumugi (tsg) {}", cli::VERSION);
            return Ok(());
        }
        cli::Mode::Server => return tsg_mux::run(&cli.session),
        cli::Mode::Send(text) => return rpc::send(&cli.session, text),
        cli::Mode::Tap => return rpc::tap(&cli.session),
        cli::Mode::List => return rpc::list(),
        cli::Mode::Capture(pane) => return rpc::capture(&cli.session, *pane),
        cli::Mode::Rpc => return rpc::raw(&cli.session),
        cli::Mode::Install => return report_install(install::install()),
        cli::Mode::Uninstall => return report_install(install::uninstall()),
        cli::Mode::ShellIntegration(name) => return print_shell_integration(name.as_deref()),
        cli::Mode::InstallShellIntegration(name) => {
            return install_shell_integration(name.as_deref());
        }
        cli::Mode::Broadcast { text, wait } => {
            let ok = rpc::broadcast(&cli.session, text, *wait)?;
            std::process::exit(i32::from(!ok) * 2);
        }
        cli::Mode::Compare => return rpc::compare(&cli.session),
        cli::Mode::Open { path, render } => return rpc::open(&cli.session, path, *render),
        cli::Mode::Render => return rpc::render(&cli.session, cli.pane),
        cli::Mode::Agents => return rpc::agents(&cli.session),
        cli::Mode::AgentState(state) => {
            return rpc::set_agent_state(&cli.session, state, cli.pane, cli.cost.clone());
        }
        cli::Mode::Wait { until, timeout } => {
            // 終了コードで答える。台本が `if tsg --wait --until blocked` と書ける。
            let ok = rpc::wait(&cli.session, until, *timeout, cli.pane)?;
            std::process::exit(i32::from(!ok) * 2);
        }
        cli::Mode::Prompt { text, wait } => {
            let ok = rpc::prompt(&cli.session, text, cli.pane, *wait)?;
            std::process::exit(i32::from(!ok) * 2);
        }
        cli::Mode::InstallAgentHooks(name) => {
            return report_install(agent_hooks::install(name.as_deref()));
        }
        cli::Mode::UninstallAgentHooks(name) => {
            return report_install(agent_hooks::uninstall(name.as_deref()));
        }
        cli::Mode::Run | cli::Mode::Diagnose => {}
    }

    // tsumugi の中から `tsg` と打たれたら、窓ではなくタブを開いて終わる。
    //
    // **端末を端末の中から起動するのは日常**なので、そのたびに窓が増えるのは
    // 邪魔でしかない。`-n` を書けば今までどおり窓が開く。繋がらなければ
    // そのまま窓を開く（開かないより、窓が開くほうがまし）。
    if cli.mode == cli::Mode::Run
        && !cli.new_window
        && let Some(inside) = std::env::var("TSUMUGI_SESSION").ok().filter(|s| !s.is_empty())
        && (!cli.session_given || cli.session == inside)
    {
        let cwd = cli
            .cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .map(|p| p.display().to_string());
        if rpc::open_tab_here(&inside, cwd, cli.command.clone()) {
            return Ok(());
        }
    }

    let (mut cfg, warning) = Config::load();
    cfg.override_with(&cli);
    // 表示の言葉はここで 1 度だけ決める（以降プロセス全体で変わらない）
    tsg_modal::set_lang(cfg.lang);
    // 桁の勘定も同じ。途中で変えると、組んだあとのグリッドと食い違う。
    tsg_term::set_ambiguous(cfg.ambiguous_width);
    session::set_scrollback(cfg.scrollback);
    if let Some(w) = warning {
        eprintln!("設定: {w}（既定で起動します）");
    }

    let event_loop = EventLoop::new().context("イベントループの作成に失敗")?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App::new(&cli, cfg);
    event_loop
        .run_app(&mut app)
        .context("イベントループが異常終了")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(a: (usize, usize), b: (usize, usize), kind: RangeKind) -> Range {
        Range::new(Pos::new(a.0, a.1), Pos::new(b.0, b.1), kind)
    }

    /// 押すたびに順に回る。同じ 1 本へ戻るのでは「探す手間を消す」に届かない。
    #[test]
    fn pressing_again_moves_to_the_next_waiting_agent() {
        let waiting = [1u32, 3, 5];
        assert_eq!(App::next_waiting(&waiting, 0), Some(1));
        assert_eq!(App::next_waiting(&waiting, 1), Some(3));
        assert_eq!(App::next_waiting(&waiting, 3), Some(5));
        // 最後まで行ったら先頭へ戻る
        assert_eq!(App::next_waiting(&waiting, 5), Some(1));
        // いま居るペインが待っていなくても、後ろから拾う
        assert_eq!(App::next_waiting(&waiting, 4), Some(5));
        assert_eq!(App::next_waiting(&[], 1), None);
    }

    #[test]
    fn charwise_selection_spans_partial_first_and_last_lines() {
        let r = range((1, 3), (3, 5), RangeKind::Char);
        assert_eq!(selection_span(&r, 0, 80), None, "範囲外の行は選択しない");
        assert_eq!(selection_span(&r, 1, 80), Some((3, 79)), "先頭行は途中から");
        assert_eq!(selection_span(&r, 2, 80), Some((0, 79)), "中間行は全幅");
        assert_eq!(selection_span(&r, 3, 80), Some((0, 5)), "末尾行は途中まで");
        assert_eq!(selection_span(&r, 4, 80), None);
    }

    #[test]
    fn single_line_charwise_selection() {
        let r = range((2, 4), (2, 9), RangeKind::Char);
        assert_eq!(selection_span(&r, 2, 80), Some((4, 9)));
    }

    #[test]
    fn linewise_selection_covers_full_width() {
        let r = range((1, 7), (2, 2), RangeKind::Line);
        assert_eq!(selection_span(&r, 1, 40), Some((0, 39)));
        assert_eq!(selection_span(&r, 2, 40), Some((0, 39)));
    }

    #[test]
    fn blockwise_selection_uses_the_same_columns_on_every_line() {
        let r = range((1, 9), (3, 4), RangeKind::Block);
        for line in 1..=3 {
            assert_eq!(selection_span(&r, line, 80), Some((4, 9)));
        }
    }

    #[test]
    fn selection_is_clipped_to_the_pane_width() {
        // 分割で狭くなったペインでも、選択が枠からはみ出さない
        let r = range((0, 3), (0, 90), RangeKind::Char);
        assert_eq!(selection_span(&r, 0, 40), Some((3, 39)));
    }

    // ---- マウス（mouse-parity.md §3・§4.3） ----

    fn term_with(text: &str) -> tsg_term::Terminal {
        let mut t = tsg_term::Terminal::new(80, 24, tsg_term::AmbiguousWidth::Wide);
        t.feed(text.as_bytes());
        t
    }

    fn col_of(t: &tsg_term::Terminal, needle: &str) -> usize {
        let buf = tsg_modal::TermBuffer::new(&t.state.grid, &t.state.marks);
        let text = tsg_modal::line_text(&buf, 0);
        let byte = text.find(needle).expect("目印が無い");
        text[..byte]
            .chars()
            .map(tsg_term::width_of)
            .sum()
    }

    #[test]
    fn a_word_grained_drag_starts_from_the_whole_path() {
        // ダブルクリックしてから引くと、パス全体を掴んだ状態から伸びる。
        // 既存のターミナルは `src` だけを掴む。
        let t = term_with("see src/main.rs and more");
        let buf = tsg_modal::TermBuffer::new(&t.state.grid, &t.state.marks);
        let start = Pos::new(0, col_of(&t, "main"));

        let r = drag_range(&buf, start, start, mouse::Grain::Word, false);
        assert_eq!(tsg_modal::extract(&buf, &r), "src/main.rs");
    }

    #[test]
    fn a_cell_grained_drag_takes_exactly_what_was_swept() {
        let t = term_with("abcdefgh");
        let buf = tsg_modal::TermBuffer::new(&t.state.grid, &t.state.marks);
        let r = drag_range(&buf, Pos::new(0, 1), Pos::new(0, 3), mouse::Grain::Cell, false);
        assert_eq!(tsg_modal::extract(&buf, &r), "bcd");
    }

    #[test]
    fn alt_drag_produces_a_block_range() {
        let t = term_with("abcdef\r\nghijkl\r\n");
        let buf = tsg_modal::TermBuffer::new(&t.state.grid, &t.state.marks);
        let r = drag_range(&buf, Pos::new(0, 1), Pos::new(1, 2), mouse::Grain::Cell, true);
        assert_eq!(r.kind, RangeKind::Block);
        assert_eq!(tsg_modal::extract(&buf, &r), "bc\nhi\n");
    }

    #[test]
    fn a_backwards_drag_is_normalised() {
        let t = term_with("abcdefgh");
        let buf = tsg_modal::TermBuffer::new(&t.state.grid, &t.state.marks);
        let fwd = drag_range(&buf, Pos::new(0, 1), Pos::new(0, 4), mouse::Grain::Cell, false);
        let back = drag_range(&buf, Pos::new(0, 4), Pos::new(0, 1), mouse::Grain::Cell, false);
        assert_eq!(fwd.start, back.start);
        assert_eq!(fwd.end, back.end);
    }

    fn attrs(fg: tsg_term::Color, bg: tsg_term::Color, flags: u8) -> Attrs {
        Attrs { fg, bg, flags }
    }

    fn th() -> Theme {
        Theme::default()
    }

    #[test]
    fn plain_cells_do_not_paint_a_background() {
        // 既定色の背景まで塗ると、80x24 ぶんの無駄な矩形が毎フレーム出る
        let th = th();
        let (fg, bg) = cell_colors(&th, &Attrs::default());
        assert_eq!(bg, None);
        assert_eq!(fg, th.fg);
    }

    #[test]
    fn reverse_on_default_colors_swaps_the_theme() {
        let th = th();
        let (fg, bg) = cell_colors(
            &th,
            &attrs(
                tsg_term::Color::Default,
                tsg_term::Color::Default,
                Attrs::REVERSE,
            ),
        );
        assert_eq!(fg, th.bg);
        assert_eq!(bg, Some(th.fg));
    }

    #[test]
    fn bold_promotes_the_standard_palette() {
        let th = th();
        let (bold, _) = cell_colors(
            &th,
            &attrs(
                tsg_term::Color::Indexed(1),
                tsg_term::Color::Default,
                Attrs::BOLD,
            ),
        );
        let (bright, _) = cell_colors(
            &th,
            &attrs(tsg_term::Color::Indexed(9), tsg_term::Color::Default, 0),
        );
        assert_eq!(bold, bright);
    }

    #[test]
    fn hidden_makes_the_glyph_match_its_background() {
        let (fg, bg) = cell_colors(
            &th(),
            &attrs(
                tsg_term::Color::Indexed(1),
                tsg_term::Color::Indexed(4),
                Attrs::HIDDEN,
            ),
        );
        assert_eq!(Some(fg), bg);
    }

    #[test]
    fn a_pipe_runs_through_the_shell_and_returns_stdout() {
        // ユーザーが書くのはシェルの文法。argv に分解してはいけない。
        // `sort` は Windows にも Unix にもあり、標準入力を読んで書き出す
        let got = pipe_through("sort", "banana\napple\n", None).expect("実行できない");
        let lines: Vec<&str> = got.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines, ["apple", "banana"], "標準入力が渡っていない: {got:?}");
    }

    #[test]
    fn a_failing_pipe_still_shows_what_went_wrong() {
        // 黙って空を返すと、何が起きたのか分からない
        let got = pipe_through("tsumugi-no-such-command-xyz", "x\n", None).expect("起動自体は成功する");
        assert!(!got.trim().is_empty(), "標準エラーが捨てられている");
    }

    /// 下線と Ctrl＋クリックは同じ判定を使う。ここが甘いと
    /// **画面中の語に下線が出て読めなくなる**。
    #[test]
    fn only_things_that_can_be_opened_are_underlined() {
        assert_eq!(open_kind("https://example.com/a"), Some(OpenKind::Url));
        assert_eq!(open_kind("file:///C:/x/y.txt"), Some(OpenKind::Url));
        assert_eq!(open_kind("src/main.rs"), Some(OpenKind::Path));
        assert_eq!(open_kind("src/main.rs:42:8"), Some(OpenKind::Path));
        assert_eq!(open_kind(r"C:\dev\x.toml"), Some(OpenKind::Path));
        assert_eq!(open_kind("README.md"), Some(OpenKind::Path));
        assert_eq!(open_kind("a1b2c3d4e5"), Some(OpenKind::Hash));

        assert_eq!(open_kind("hello"), None, "ただの語に下線が出る");
        assert_eq!(open_kind(""), None);
        assert_eq!(open_kind("行 42"), None, "空白を含むものは 1 つの対象ではない");
        assert_eq!(open_kind("1234567890"), None, "数字だけをハッシュにしない");
        assert_eq!(open_kind("abc"), None, "3 桁ではハッシュと言えない");
    }

    #[test]
    fn a_windows_drive_letter_is_not_a_line_number() {
        assert_eq!(strip_position(r"C:\dev\x.toml"), r"C:\dev\x.toml");
        assert_eq!(strip_position("src/main.rs:42:8"), "src/main.rs");
        assert_eq!(strip_position(r"C:\dev\x.rs:12"), r"C:\dev\x.rs");
        assert_eq!(strip_position("plain"), "plain");
    }

    #[test]
    fn clicking_in_a_file_never_changes_the_mode() {
        // 端末のカーソル位置でモードを切り替えていたので、開いているファイルと
        // 無関係に入力モードへ入っていた。実機で踏んだ回帰。
        assert_eq!(click_intent(true, 0, 999), ClickIntent::MoveOnly);
        assert_eq!(click_intent(true, 999, 0), ClickIntent::MoveOnly);

        // 端末では従来どおり。生きた末尾なら入力、過去の出力なら読むモード。
        assert_eq!(click_intent(false, 999, 10), ClickIntent::ToInsert);
        assert_eq!(click_intent(false, 3, 10), ClickIntent::ToNormal);
    }

    fn buttons(macro_label: &str) -> Vec<(String, StatusTarget)> {
        vec![
            (" ≡ ".to_string(), StatusTarget::Palette),
            (" ? ".to_string(), StatusTarget::Help),
            (macro_label.to_string(), StatusTarget::Macro),
        ]
    }

    #[test]
    fn the_status_bar_has_a_permanent_palette_button() {
        // `mouse-parity.md` §4.7 の最終保証は「常設ボタン 1 クリック」。
        // ここが動くかどうかで、マウスから届かないコマンドが出るかが決まる。
        // 左端 1 桁はウィンドウのリサイズ枠に食われるので、そこだけに頼らない
        let b = buttons(" 録 ");
        assert_eq!(status_hit(&b, 0), StatusTarget::Palette);
        assert_eq!(status_hit(&b, 1), StatusTarget::Palette);
        assert_eq!(status_hit(&b, 200), StatusTarget::Ownership);
    }

    /// ボタンの並びが変わっても、字の出る位置とクリックの当たる位置がずれない。
    /// 実機で 1 度これを外して「見えているのに押せない」を作った。
    #[test]
    fn every_status_button_is_hit_where_it_is_drawn() {
        for label in [" 録 ", " 録a ", " 再a "] {
            let b = buttons(label);
            let mut x = 0usize;
            for (text, target) in &b {
                let w = display_width(text);
                for col in x..x + w {
                    assert_eq!(
                        status_hit(&b, col),
                        *target,
                        "{label} の {col} 桁目が {target:?} に当たらない"
                    );
                }
                x += w;
            }
            assert_eq!(status_hit(&b, x), StatusTarget::Ownership);
        }
    }

    #[test]
    fn every_palette_reachable_command_can_actually_be_invoked() {
        // 宣言だけして実行できない項目があると、パレットが「最終保証」でなくなる。
        let mut t = tsg_term::Terminal::new(40, 8, tsg_term::AmbiguousWidth::Wide);
        t.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07a.txt src/main.rs 42\r\n\x1b]133;D;1\x07");
        let buf = tsg_modal::TermBuffer::new(&t.state.grid, &t.state.marks);

        for spec in tsg_modal::REGISTRY.iter().filter(|s| s.in_palette) {
            let mut e = tsg_modal::Engine::new();
            let fx = e.invoke(spec.id, &buf);
            let unhandled = fx.iter().any(|f| {
                matches!(f, Effect::Message(m) if m.contains("ここからは実行できません"))
            });
            assert!(!unhandled, "{} がパレットから実行できない", spec.id);
        }
    }

    #[test]
    fn the_tab_bar_only_takes_a_row_when_there_is_more_than_one_tab() {
        use tsg_mux::protocol::{Layout, SessionInfo, TabInfo};

        let tab = |id: u32| TabInfo {
            id,
            layout: Layout::leaf(id),
            active_pane: id,
            zoom: None,
        };
        let mut app = App::new(&cli::Cli::default(), Config::default());
        app.rows = 24;

        assert_eq!(app.tab_rows(), 0, "情報が無いうちは行を取らない");
        assert_eq!(app.area().y, 0);
        assert_eq!(app.text_rows(), 23, "ステータス行のぶんだけ");

        app.session.info = Some(SessionInfo {
            name: "t".into(),
            tabs: vec![tab(1)],
            active_tab: 1,
            panes: vec![],
        });
        assert_eq!(app.tab_rows(), 0, "1 枚なら出さない");

        app.session.info.as_mut().unwrap().tabs.push(tab(2));
        assert_eq!(app.tab_rows(), 1);
        assert_eq!(app.area().y, 1, "本文がタブバーのぶん下がる");
        assert_eq!(app.text_rows(), 22);
    }

    #[test]
    fn truncate_width_does_not_split_a_wide_char() {
        assert_eq!(truncate_width("abc", 10), "abc");
        let cut = truncate_width("日本語のタイトル", 6);
        assert!(display_width(&cut) <= 6, "{cut:?} が幅を超えている");
        assert!(cut.ends_with('\u{2026}'));
    }

    #[test]
    fn arrows_are_motions_outside_insert_but_not_inside() {
        let none = ModifiersState::empty();
        assert_eq!(
            to_key_input(&Key::Named(NamedKey::ArrowDown), none, Mode::Normal),
            Some(KeyInput::Char('j'))
        );
        assert_eq!(
            to_key_input(&Key::Named(NamedKey::ArrowDown), none, Mode::Insert),
            None,
            "Insert では素のキーとして PTY へ送るのでここでは拾わない"
        );
    }

    #[test]
    fn control_keys_become_ctrl_inputs() {
        let ctrl = ModifiersState::CONTROL;
        assert_eq!(
            to_key_input(&Key::Character("V".into()), ctrl, Mode::Normal),
            Some(KeyInput::Ctrl('v')),
            "大文字で来ても小文字へ正規化する"
        );
    }

    #[test]
    fn f1_is_help_in_every_mode() {
        let none = ModifiersState::empty();
        for mode in [Mode::Insert, Mode::Normal, Mode::Layout] {
            assert_eq!(
                to_key_input(&Key::Named(NamedKey::F1), none, mode),
                Some(KeyInput::Function(1))
            );
        }
    }
}
