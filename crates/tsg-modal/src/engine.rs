//! モード機械とディスパッチャ。
//!
//! `arch.md` の不変条件 2「`tsg-modal` は純粋」。I/O 依存はゼロで、
//! 入力は `KeyInput` と `&dyn Buffer`、出力は `Effect` の列だけ。
//! したがってキー列 -> 状態のゴールデンテストがヘッドレスで書ける。

use std::collections::BTreeMap;

use tsg_buffer::{
    Buffer, BufferKind, OperatorId, Pos, Range, RangeKind, clamp, clamp_insert, extract,
    first_non_blank, line_width,
};

use crate::command::{
    Command, FileAction, FocusDir, HistoryAction, InsertAt, Mode, MuxRequest, SplitDir, VisualKind,
};
use crate::format;
use crate::motion::{self, Motion, MotionKind, View};
use crate::search::Search;
use crate::t;
use crate::textobj::{self, TextObject};

/// プラットフォーム非依存のキー表現。winit などの型はここへ持ち込まない。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum KeyInput {
    Char(char),
    Esc,
    Enter,
    Backspace,
    Tab,
    Ctrl(char),
    /// F1..F12
    Function(u8),
}

/// エンジンが起こした変化。ホストがこれを見て実際の副作用を行う。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    ModeChanged(Mode),
    CursorMoved(Pos),
    Yanked {
        register: char,
        chars: usize,
        lines: usize,
    },
    SetClipboard(String),
    /// `!` — 範囲のテキストを現在のプロンプトへ挿入する。実行はしない。
    SendToPrompt(String),
    /// `>` — 範囲を外部コマンドへ通す。コマンド名を訊くのはホストの仕事。
    ///
    /// エンジンはプロセスを起こさない（`arch.md` の不変条件 2）。
    /// 渡すのは「何を流し込むか」だけ。
    Pipe {
        input: String,
    },
    /// 範囲を `text` で置き換える。`d` は空文字、`=` は整形結果。
    ///
    /// エンジンはバッファを書き換えない（`&dyn Buffer` しか持たない）。
    /// 実際に変えるのはホストで、相手が端末のグリッドかファイルかを知っているのも
    /// ホストだけ。`arch.md` の不変条件 2「`tsg-modal` は純粋」を保つための形。
    Edit {
        range: Range,
        text: String,
    },
    Scrolled(isize),
    Message(String),
    HelpToggled(bool),
    /// 配色を変える。色を知っているのはホストだけなので、名前だけ渡す。
    SetTheme(String),
    /// 設定ファイルを開く。場所を知っているのはホストだけ。
    OpenConfig,
    /// 検索の入力を開く。窓の作りはホストが持つ。
    OpenSearch {
        back: bool,
    },
    /// mux（別プロセス）への要求。ホストが `tsg-mux` のメッセージへ翻訳する。
    Mux(MuxRequest),
    /// コマンドパレットを開く（`prefix` を入れた状態で）。
    Palette(String),
    /// ファイルバッファへの操作。
    File(FileAction),
    /// 取り消し / やり直し。実際に戻すのはバッファを持つホスト。
    History(HistoryAction),
    /// 長さ 0 の挿入。`Edit`（範囲の置き換え）と分けているのは、
    /// 「消さずに差し込む」を範囲で表そうとすると必ず端が 1 セルずれるため。
    Insert {
        at: Pos,
        text: String,
        /// 差し込んだ後のカーソル。`None` なら差し込んだ末尾。
        ///
        /// 行として貼るときと `O` は、末尾ではなく**先頭**に置きたい。
        /// ホスト側で場合分けすると、貼り方を増やすたびに条件が増える。
        cursor: Option<Pos>,
    },
    /// `ma` で印を置いた。ホストはガターへ出す。
    MarkSet {
        name: char,
        pos: Pos,
    },
    /// マクロの記録状態が変わった。`None` は終了。
    MacroRecording(Option<char>),
    /// 記録したキー列。**流し直すのはホスト**。
    ///
    /// エンジンはバッファを書き換えられない（不変条件 2）ので、
    /// ここで自分で回すと 2 キー目以降が編集前のバッファを見てしまう。
    /// 1 キーごとにバッファを取り直せるのはホストだけ。
    MacroReplay(Vec<KeyInput>),
    Bell,
    Quit,
}

#[derive(Debug, PartialEq, Eq)]
pub enum KeyOutcome {
    Handled(Vec<Effect>),
    /// Insert モード。ホストが PTY へ素通しする。
    PassThrough,
}

// ---- レジスタ -------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisterValue {
    pub text: String,
    pub kind: RangeKind,
}

#[derive(Default, Debug)]
pub struct Registers {
    map: BTreeMap<char, RegisterValue>,
}

impl Registers {
    pub fn get(&self, name: char) -> Option<&RegisterValue> {
        self.map.get(&name)
    }

    pub fn names(&self) -> impl Iterator<Item = &char> {
        self.map.keys()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&char, &RegisterValue)> {
        self.map.iter()
    }

    /// ヤンクの書き込み規則。無名 `"` と `0`、指定があれば名前付きにも入れる。
    /// 大文字指定は追記（`modal-spec.md` §8）。
    fn yank(&mut self, named: Option<char>, value: RegisterValue) {
        self.map.insert('"', value.clone());
        self.map.insert('0', value.clone());
        match named {
            Some(n) if n.is_ascii_uppercase() => {
                let key = n.to_ascii_lowercase();
                let entry = self.map.entry(key).or_insert_with(|| RegisterValue {
                    text: String::new(),
                    kind: value.kind,
                });
                entry.text.push_str(&value.text);
            }
            Some(n) => {
                self.map.insert(n, value);
            }
            None => {}
        }
    }
}

// ---- マーク ---------------------------------------------------------------

/// `ma` で置く印。
///
/// レジスタと同じくウィンドウ全体で 1 組だけ持つ。カーソルもペインをまたいで
/// 1 つしか無い設計なので、印だけペイン別にすると挙動が食い違う。
#[derive(Default, Debug)]
pub struct Marks {
    map: BTreeMap<char, Pos>,
}

impl Marks {
    pub fn get(&self, name: char) -> Option<Pos> {
        self.map.get(&name).copied()
    }

    pub fn set(&mut self, name: char, pos: Pos) {
        self.map.insert(name, pos);
    }

    pub fn iter(&self) -> impl Iterator<Item = (&char, &Pos)> {
        self.map.iter()
    }

    /// その行に置かれている印。ガターに出すのはこれ。
    ///
    /// 飛ぶ前の位置を覚える `` ` `` と `'` は名前が英数字でないので出ない。
    /// 自動で置かれる印がガターを埋めると、自分で置いた印が見えなくなる。
    pub fn at_line(&self, line: usize) -> Option<char> {
        self.map
            .iter()
            .find(|(n, p)| p.line == line && n.is_ascii_alphanumeric())
            .map(|(n, _)| *n)
    }
}

// ---- マクロ ---------------------------------------------------------------

/// `q{a}` … `q` で録って `@{a}` で流す。
///
/// 中身はテキストではなく `KeyInput` の列で持つ。文字列に落とすと
/// `Ctrl` や `Esc` の往復で情報が欠ける。
#[derive(Default, Debug)]
pub struct Macros {
    recording: Option<(char, Vec<KeyInput>)>,
    stored: BTreeMap<char, Vec<KeyInput>>,
    last: Option<char>,
}

impl Macros {
    pub fn recording(&self) -> Option<char> {
        self.recording.as_ref().map(|(n, _)| *n)
    }

    pub fn last(&self) -> Option<char> {
        self.last
    }

    pub fn get(&self, name: char) -> Option<&[KeyInput]> {
        self.stored.get(&name).map(Vec::as_slice)
    }

    pub fn names(&self) -> impl Iterator<Item = &char> {
        self.stored.keys()
    }
}

// ---- 保留状態 -------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Awaiting {
    Register,
    FindChar {
        till: bool,
        backward: bool,
    },
    GPrefix,
    ZPrefix,
    /// `i` / `a` の次に来るオブジェクト文字を待っている
    TextObject {
        around: bool,
    },
    /// `[` / `]` の次を待っている（`modal-spec.md` §5.2）
    Bracket {
        forward: bool,
    },
    /// `m` の次に来る印の名前を待っている
    MarkSet,
    /// `` ` `` / `'` の次に来る印の名前を待っている
    MarkJump {
        exact: bool,
    },
    /// `q` の次に来るマクロ名を待っている
    MacroRecord,
    /// `@` の次に来るマクロ名を待っている
    MacroReplay,
}

#[derive(Default, Debug)]
struct Pending {
    count: Option<usize>,
    operator: Option<OperatorId>,
    operator_key: Option<char>,
    register: Option<char>,
    awaiting: Option<Awaiting>,
}

impl Pending {
    fn clear(&mut self) {
        *self = Pending::default();
    }
}

// ---- エンジン -------------------------------------------------------------

pub struct Engine {
    mode: Mode,
    cursor: Pos,
    anchor: Option<Pos>,
    pending: Pending,
    last_find: Option<(char, bool, bool)>,
    /// いま探している文字列。`n` `N` と強調が見る。
    ///
    /// **エンジンが持つ。** 画面の側に置くと、ペインを切り替えた瞬間に
    /// `n` が何を指すのか分からなくなる。
    pub search: Option<Search>,
    /// 言語サーバが言ってきた誤りの行。`[e` `]e` が見る。
    ///
    /// **エンジンが持つ。** ペインを切り替えても `]e` が何を指すのかを
    /// 見失わないため（探している文字列と同じ扱い）。
    pub error_lines: Vec<usize>,
    /// 次に開く探し窓を正規表現として読むか（`g/`）。
    ///
    /// **既定は素の文字列。** 端末で探すのはパスやエラー文で、`.` も `(` も
    /// そのままの字として入っている（`search.rs`）。
    pub search_regex: bool,
    view: View,
    help_visible: bool,
    pub registers: Registers,
    pub marks: Marks,
    pub macros: Macros,
    /// Term バッファのヤンクを既定でシステムクリップボードへ入れるか
    /// （`modal-spec.md` §8。`:set clipboard=` で切れるようにする予定）
    pub clipboard_on_yank: bool,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        Self {
            mode: Mode::Insert,
            cursor: Pos::default(),
            anchor: None,
            pending: Pending::default(),
            last_find: None,
            search: None,
            error_lines: Vec::new(),
            search_regex: false,
            view: View::default(),
            help_visible: false,
            registers: Registers::default(),
            marks: Marks::default(),
            macros: Macros::default(),
            clipboard_on_yank: true,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn help_visible(&self) -> bool {
        self.help_visible
    }

    pub fn set_help_visible(&mut self, visible: bool) {
        self.help_visible = visible;
    }

    pub fn cursor(&self) -> Pos {
        self.cursor
    }

    pub fn view(&self) -> View {
        self.view
    }

    pub fn set_view(&mut self, view: View) {
        self.view = view;
    }

    /// ホストからカーソルを合わせる（マウスのクリック、Insert への復帰など）。
    ///
    /// 入力モードだけは行末の 1 つ先を許す（`clamp_insert` の説明を参照）。
    pub fn set_cursor(&mut self, pos: Pos, buf: &dyn Buffer) {
        self.cursor = if self.mode == Mode::Insert {
            clamp_insert(buf, pos)
        } else {
            clamp(buf, pos)
        };
    }

    /// 現在の選択範囲（Visual モードのとき）。
    pub fn selection(&self) -> Option<Range> {
        let Mode::Visual(kind) = self.mode else {
            return None;
        };
        let anchor = self.anchor?;
        Some(Range::new(anchor, self.cursor, kind.range_kind()))
    }

    /// ステータス行に出す保留状態（`2d` まで打った、など）。
    pub fn pending_hint(&self) -> String {
        let mut s = String::new();
        if let Some(r) = self.pending.register {
            s.push('"');
            s.push(r);
        }
        if let Some(c) = self.pending.count {
            s.push_str(&c.to_string());
        }
        if let Some(k) = self.pending.operator_key {
            s.push(k);
        }
        if self.pending.awaiting.is_some() {
            s.push('…');
        }
        s
    }

    // ---- キー解決 ---------------------------------------------------------

    pub fn key(&mut self, key: KeyInput, buf: &dyn Buffer) -> KeyOutcome {
        // F1 はどのモードでも使い方を出す。初見で詰まらないことを最優先する。
        if key == KeyInput::Function(1) {
            return KeyOutcome::Handled(self.execute(Command::ToggleHelp, buf));
        }
        // ヘルプ表示中は、どのキーでも閉じるだけにする（読んでいる最中に
        // 勝手にモードが変わると、何が起きたのか分からなくなる）。
        if self.help_visible {
            return KeyOutcome::Handled(self.execute(Command::ToggleHelp, buf));
        }

        // 記録の停止は、その `q` が記録へ混ざる前に捌く。
        // Insert 中の `q` はただの文字なので、モードで分ける。
        if self.macros.recording.is_some()
            && self.mode != Mode::Insert
            && key == KeyInput::Char('q')
            && self.pending.awaiting.is_none()
        {
            return KeyOutcome::Handled(self.execute(Command::MacroRecord(None), buf));
        }
        if let Some((_, keys)) = self.macros.recording.as_mut() {
            keys.push(key);
        }

        if self.mode == Mode::Insert {
            return match key {
                // Esc をここへ渡すかどうかは所有権の裁定済み（`tsg-term` の `key_owner`）。
                KeyInput::Esc | KeyInput::Ctrl('\\') => {
                    KeyOutcome::Handled(self.execute(Command::EnterNormal, buf))
                }
                _ => KeyOutcome::PassThrough,
            };
        }

        match self.resolve(key, buf) {
            Some(cmd) => KeyOutcome::Handled(self.execute(cmd, buf)),
            None => KeyOutcome::Handled(Vec::new()),
        }
    }

    /// キーを `Command` へ落とす。まだ確定しない（数値の途中など）なら `None`。
    fn resolve(&mut self, key: KeyInput, buf: &dyn Buffer) -> Option<Command> {
        let c = match key {
            KeyInput::Esc => {
                self.pending.clear();
                return Some(Command::EnterNormal);
            }
            KeyInput::Ctrl('\\') => return Some(Command::EnterNormal),
            KeyInput::Ctrl('v') => return Some(Command::EnterVisual(VisualKind::Block)),
            KeyInput::Ctrl('r') => return Some(Command::History(HistoryAction::Redo)),
            KeyInput::Ctrl('d') => return self.motion_command(Motion::HalfPageDown, buf),
            KeyInput::Ctrl('u') => return self.motion_command(Motion::HalfPageUp, buf),
            KeyInput::Ctrl('f') => return self.motion_command(Motion::PageDown, buf),
            KeyInput::Ctrl('b') => return self.motion_command(Motion::PageUp, buf),
            KeyInput::Char(c) => c,
            _ => return None,
        };

        // 配置モードは独立した語彙を持つ（`modal-spec.md` §9）。
        if self.mode == Mode::Layout {
            return self.resolve_layout(c);
        }

        // 1文字を引数に取る待ち状態を先に消化する。
        if let Some(awaiting) = self.pending.awaiting.take() {
            match awaiting {
                Awaiting::Register => {
                    self.pending.register = Some(c);
                    return None;
                }
                Awaiting::FindChar { till, backward } => {
                    self.last_find = Some((c, till, backward));
                    return self.motion_command(Motion::FindChar { c, till, backward }, buf);
                }
                Awaiting::GPrefix => {
                    return match c {
                        // `g/` は正規表現として探す。`/` は打った通り。
                        // **既定を入れ替えない。** 端末で探すのはパスや
                        // エラー文で、`.` も `(` もそのままの字。
                        '/' => {
                            self.pending.clear();
                            self.search_regex = true;
                            Some(Command::OpenSearch { back: false })
                        }
                        // `gd` は定義へ。**vim と同じ字**にしておく。
                        'd' => {
                            self.pending.clear();
                            Some(Command::Mux(MuxRequest::Definition))
                        }
                        'g' => {
                            let n = self.pending.count.take();
                            match n {
                                Some(n) => self.motion_command(Motion::ToLine(n), buf),
                                None => self.motion_command(Motion::DocStart, buf),
                            }
                        }
                        _ => {
                            self.pending.clear();
                            None
                        }
                    };
                }
                Awaiting::TextObject { around } => {
                    return self.text_object(c, around, buf);
                }
                Awaiting::Bracket { forward } => {
                    let motion = match (c, forward) {
                        ('[', false) => Motion::PrevPrompt,
                        (']', true) => Motion::NextPrompt,
                        ('e', false) => Motion::PrevError,
                        ('e', true) => Motion::NextError,
                        ('a', false) => Motion::PrevAgentBlock,
                        ('a', true) => Motion::NextAgentBlock,
                        _ => {
                            self.pending.clear();
                            return None;
                        }
                    };
                    return self.motion_command(motion, buf);
                }
                Awaiting::MarkSet => return Some(Command::SetMark(c)),
                Awaiting::MarkJump { exact } => return self.mark_command(c, exact, buf),
                Awaiting::MacroRecord => return Some(Command::MacroRecord(Some(c))),
                Awaiting::MacroReplay => return Some(Command::MacroReplay(c)),
                Awaiting::ZPrefix => {
                    return match c {
                        'Z' => Some(Command::Quit),
                        _ => {
                            self.pending.clear();
                            None
                        }
                    };
                }
            }
        }

        // カウント。先頭の `0` は行頭モーションなのでカウントにしない。
        if c.is_ascii_digit() && !(c == '0' && self.pending.count.is_none()) {
            let d = c.to_digit(10).unwrap_or(0) as usize;
            self.pending.count = Some(self.pending.count.unwrap_or(0) * 10 + d);
            return None;
        }

        // オペレータの二度押し（`yy`）は行単位。
        if let Some(op) = self.pending.operator
            && self.pending.operator_key == Some(c)
        {
            let count = self.take_count();
            let last = buf.line_count().saturating_sub(1);
            let end = (self.cursor.line + count - 1).min(last);
            let range = Range::new(
                Pos::new(self.cursor.line, 0),
                Pos::new(end, 0),
                RangeKind::Line,
            );
            let register = self.pending.register.take();
            self.pending.clear();
            return Some(Command::Apply {
                op,
                range,
                register,
            });
        }

        // `i` / `a` は、オペレータ待ちと Visual ではテキストオブジェクトの前置になる。
        // Normal の「入力モードへ」より先に見る。
        if matches!(c, 'i' | 'a') && matches!(self.mode, Mode::OperatorPending | Mode::Visual(_)) {
            self.pending.awaiting = Some(Awaiting::TextObject { around: c == 'a' });
            return None;
        }

        match c {
            'i' | 'a' | 'I' | 'A' | 'o' | 'O' if self.mode == Mode::Normal => {
                Some(Command::EnterInsert(match c {
                    'a' => InsertAt::After,
                    'I' => InsertAt::LineStart,
                    'A' => InsertAt::LineEnd,
                    'o' => InsertAt::Below,
                    'O' => InsertAt::Above,
                    _ => InsertAt::Here,
                }))
            }
            ' ' if self.mode == Mode::Normal => Some(Command::EnterLayout),
            'v' => Some(Command::EnterVisual(VisualKind::Char)),
            'V' => Some(Command::EnterVisual(VisualKind::Line)),
            '"' => {
                self.pending.awaiting = Some(Awaiting::Register);
                None
            }
            'g' => {
                self.pending.awaiting = Some(Awaiting::GPrefix);
                None
            }
            '[' | ']' => {
                self.pending.awaiting = Some(Awaiting::Bracket { forward: c == ']' });
                None
            }
            'Z' => {
                self.pending.awaiting = Some(Awaiting::ZPrefix);
                None
            }
            'u' => Some(Command::History(HistoryAction::Undo)),
            'p' => Some(Command::Paste { before: false }),
            'P' => Some(Command::Paste { before: true }),
            'm' => {
                self.pending.awaiting = Some(Awaiting::MarkSet);
                None
            }
            '`' => {
                self.pending.awaiting = Some(Awaiting::MarkJump { exact: true });
                None
            }
            '\'' => {
                self.pending.awaiting = Some(Awaiting::MarkJump { exact: false });
                None
            }
            'q' => {
                self.pending.awaiting = Some(Awaiting::MacroRecord);
                None
            }
            '@' => {
                self.pending.awaiting = Some(Awaiting::MacroReplay);
                None
            }

            'y' | 'd' | 'c' | '!' | '>' | '=' => {
                let op = match c {
                    'y' => OperatorId::Yank,
                    'd' => OperatorId::Delete,
                    'c' => OperatorId::Change,
                    '!' => OperatorId::SendToPrompt,
                    '>' => OperatorId::Pipe,
                    _ => OperatorId::Format,
                };
                self.operator(op, c, buf)
            }
            // `Y` は行単位ヤンク
            'Y' => {
                let count = self.take_count();
                let last = buf.line_count().saturating_sub(1);
                let end = (self.cursor.line + count - 1).min(last);
                let register = self.pending.register.take();
                self.pending.clear();
                Some(Command::Apply {
                    op: OperatorId::Yank,
                    range: Range::new(
                        Pos::new(self.cursor.line, 0),
                        Pos::new(end, 0),
                        RangeKind::Line,
                    ),
                    register,
                })
            }

            'h' => self.motion_command(Motion::Left, buf),
            'l' | ' ' => self.motion_command(Motion::Right, buf), // Visual 中の Space は右移動
            'j' => self.motion_command(Motion::Down, buf),
            'k' => self.motion_command(Motion::Up, buf),
            'w' => self.motion_command(Motion::WordFwd { big: false }, buf),
            'W' => self.motion_command(Motion::WordFwd { big: true }, buf),
            'b' => self.motion_command(Motion::WordBack { big: false }, buf),
            'B' => self.motion_command(Motion::WordBack { big: true }, buf),
            'e' => self.motion_command(Motion::WordEnd { big: false }, buf),
            'E' => self.motion_command(Motion::WordEnd { big: true }, buf),
            '0' => self.motion_command(Motion::LineStart, buf),
            '^' => self.motion_command(Motion::FirstNonBlank, buf),
            '$' => self.motion_command(Motion::LineEnd, buf),
            '{' => self.motion_command(Motion::ParaBack, buf),
            '}' => self.motion_command(Motion::ParaFwd, buf),
            '%' => self.motion_command(Motion::MatchPair, buf),
            '/' => {
                self.search_regex = false;
                Some(Command::OpenSearch { back: false })
            }
            '?' => {
                self.search_regex = false;
                Some(Command::OpenSearch { back: true })
            }
            'n' => self.motion_command(Motion::SearchNext, buf),
            'N' => self.motion_command(Motion::SearchPrev, buf),
            'H' => self.motion_command(Motion::ScreenTop, buf),
            'M' => self.motion_command(Motion::ScreenMiddle, buf),
            'L' => self.motion_command(Motion::ScreenBottom, buf),
            'G' => {
                let n = self.pending.count.take();
                match n {
                    Some(n) => self.motion_command(Motion::ToLine(n), buf),
                    None => self.motion_command(Motion::DocEnd, buf),
                }
            }
            'f' => {
                self.pending.awaiting = Some(Awaiting::FindChar {
                    till: false,
                    backward: false,
                });
                None
            }
            'F' => {
                self.pending.awaiting = Some(Awaiting::FindChar {
                    till: false,
                    backward: true,
                });
                None
            }
            't' => {
                self.pending.awaiting = Some(Awaiting::FindChar {
                    till: true,
                    backward: false,
                });
                None
            }
            'T' => {
                self.pending.awaiting = Some(Awaiting::FindChar {
                    till: true,
                    backward: true,
                });
                None
            }
            ';' => self.motion_command(Motion::RepeatFind { reverse: false }, buf),
            ',' => self.motion_command(Motion::RepeatFind { reverse: true }, buf),

            _ => {
                self.pending.clear();
                None
            }
        }
    }

    /// モーションへ渡す周りの事情。
    fn motion_ctx(&self) -> motion::Ctx<'_> {
        motion::Ctx {
            view: self.view,
            last_find: self.last_find,
            search: self.search.as_ref(),
            error_lines: &self.error_lines,
        }
    }

    fn resolve_layout(&mut self, c: char) -> Option<Command> {
        let req = match c {
            's' => MuxRequest::Split(SplitDir::Vertical),
            'v' => MuxRequest::Split(SplitDir::Horizontal),
            'h' => MuxRequest::Focus(FocusDir::Left),
            'j' => MuxRequest::Focus(FocusDir::Down),
            'k' => MuxRequest::Focus(FocusDir::Up),
            'l' => MuxRequest::Focus(FocusDir::Right),
            'x' => MuxRequest::ClosePane,
            'H' => MuxRequest::Swap(FocusDir::Left),
            'J' => MuxRequest::Swap(FocusDir::Down),
            'K' => MuxRequest::Swap(FocusDir::Up),
            'L' => MuxRequest::Swap(FocusDir::Right),
            'z' => MuxRequest::Zoom,
            '=' => MuxRequest::Equalize,
            '<' | '-' => MuxRequest::Resize(-10),
            '>' | '+' => MuxRequest::Resize(10),
            '[' => MuxRequest::MoveTab(-1),
            ']' => MuxRequest::MoveTab(1),
            'S' => MuxRequest::Sessions,
            'c' => MuxRequest::NewTab,
            'n' => MuxRequest::NextTab,
            'p' => MuxRequest::PrevTab,
            'd' => MuxRequest::Detach,
            'Q' => MuxRequest::Shutdown,
            // 画面を読むための道具。**一覧に載っている以上、押せば効く。**
            'g' => MuxRequest::GitDiff,
            'G' => MuxRequest::ApplyHunk { stage: true },
            'R' => MuxRequest::ApplyHunk { stage: false },
            'b' => MuxRequest::Broadcast,
            'o' => MuxRequest::ToggleFold,
            'O' => MuxRequest::FoldAll(true),
            'U' => MuxRequest::FoldAll(false),
            'm' => MuxRequest::TogglePreview,
            'f' => MuxRequest::PaneFiles,
            'a' => MuxRequest::NextAgent,
            't' => MuxRequest::Hints,
            // 配置モードから直接エディタを開く（`modal-spec.md` §9）
            'e' => return Some(Command::Palette("e ")),
            _ => return Some(Command::EnterNormal),
        };
        Some(Command::Mux(req))
    }

    /// オペレータキーを受けたときの処理。
    ///
    /// Visual モードなら選択に即適用、Normal なら operator-pending へ入る。
    fn operator(&mut self, op: OperatorId, key: char, buf: &dyn Buffer) -> Option<Command> {
        if let Some(range) = self.selection() {
            let register = self.pending.register.take();
            self.pending.clear();
            return Some(Command::Apply {
                op,
                range,
                register,
            });
        }
        self.pending.operator = Some(op);
        self.pending.operator_key = Some(key);
        self.mode = Mode::OperatorPending;
        let _ = buf;
        None
    }

    /// レジストリの id を実行する。**メニューとコマンドパレットの入口。**
    ///
    /// `mouse-parity.md` §2.1 の合流点をここでも使う。メニュー項目が独自の
    /// 副作用を持たないので、「キーボードでは効くのにメニューからは効かない」
    /// が構造的に起きない。範囲が要るものは今の選択を使う。
    pub fn invoke(&mut self, id: &str, buf: &dyn Buffer) -> Vec<Effect> {
        let selection = self.selection();
        let cmd = match id {
            "ui.help" => Command::ToggleHelp,
            "ui.config" => Command::OpenConfig,
            "ui.theme.yogiri" => Command::SetTheme("yogiri"),
            "ui.theme.sumi" => Command::SetTheme("sumi"),
            "ui.theme.hakuji" => Command::SetTheme("hakuji"),
            "mode.insert" => Command::EnterInsert(InsertAt::Here),
            "mode.normal" => Command::EnterNormal,
            "mode.visual.char" => Command::EnterVisual(VisualKind::Char),
            "mode.visual.line" => Command::EnterVisual(VisualKind::Line),
            "mode.visual.block" => Command::EnterVisual(VisualKind::Block),
            "layout.mode" => Command::EnterLayout,
            "layout.split" => Command::Mux(MuxRequest::Split(SplitDir::Horizontal)),
            "layout.close" => Command::Mux(MuxRequest::ClosePane),
            "layout.tab" => Command::Mux(MuxRequest::NewTab),
            "layout.zoom" => Command::Mux(MuxRequest::Zoom),
            "layout.equalize" => Command::Mux(MuxRequest::Equalize),
            "layout.sessions" => Command::Mux(MuxRequest::Sessions),
            "agent.next" => Command::Mux(MuxRequest::NextAgent),
            "search.open" => {
                self.search_regex = false;
                Command::OpenSearch { back: false }
            }
            "lsp.definition" => Command::Mux(MuxRequest::Definition),
            "lsp.complete" => Command::Mux(MuxRequest::Complete),
            "search.regex" => {
                self.search_regex = true;
                Command::OpenSearch { back: false }
            }
            "hints" => Command::Mux(MuxRequest::Hints),
            "fold.toggle" => Command::Mux(MuxRequest::ToggleFold),
            "agent.broadcast" => Command::Mux(MuxRequest::Broadcast),
            "git.diff" => Command::Mux(MuxRequest::GitDiff),
            "git.stage_hunk" => Command::Mux(MuxRequest::ApplyHunk { stage: true }),
            "git.revert_hunk" => Command::Mux(MuxRequest::ApplyHunk { stage: false }),
            "fold.all" => Command::Mux(MuxRequest::FoldAll(true)),
            "search.next" => Command::Move {
                motion: Motion::SearchNext,
                count: 1,
            },
            "agent.files" => Command::Mux(MuxRequest::PaneFiles),
            "file.preview" => Command::Mux(MuxRequest::TogglePreview),
            "motion.agent" => Command::Move {
                motion: Motion::NextAgentBlock,
                count: 1,
            },
            // 向きが要るものは、キーで打ったのと同じ待ち状態にはできない
            // （配置モードの語彙なので）。既定の向きで 1 回動かす。
            "layout.swap" => Command::Mux(MuxRequest::Swap(FocusDir::Right)),
            "layout.resize" => Command::Mux(MuxRequest::Resize(10)),
            "layout.tabmove" => Command::Mux(MuxRequest::MoveTab(1)),
            "layout.detach" => Command::Mux(MuxRequest::Detach),
            "layout.shutdown" => Command::Mux(MuxRequest::Shutdown),
            "app.quit" => Command::Quit,
            "edit.undo" => Command::History(HistoryAction::Undo),
            "edit.redo" => Command::History(HistoryAction::Redo),
            "file.open" => Command::Palette("e "),
            // どれも入力欄へ書き足す形。**入り口を 2 つ作らない**
            // （`:e` と同じ欄で受ける）。
            "layout.tabname" => Command::Palette("tabname "),
            "edit.substitute" => Command::Palette("s/"),
            "motion.goto_line" => Command::Palette(""),
            "file.save" => Command::File(FileAction::Save),
            "file.close" => Command::File(FileAction::Close),
            "op.delete" | "op.change" | "op.format" | "op.pipe" => {
                let op = match id {
                    "op.delete" => OperatorId::Delete,
                    "op.change" => OperatorId::Change,
                    "op.pipe" => OperatorId::Pipe,
                    _ => OperatorId::Format,
                };
                let Some(range) = selection else {
                    return vec![
                        Effect::Message(
                            t!("先に範囲を選んでください", "select something first").into(),
                        ),
                        Effect::Bell,
                    ];
                };
                Command::Apply {
                    op,
                    range,
                    register: None,
                }
            }
            // 続きの 1 文字が要るものは、キーで打ったのと同じ待ち状態に入れる。
            // 「メニューから選んだら何も起きない」を作らないための扱い。
            "edit.paste" => Command::Paste { before: false },
            "mark.set" => {
                self.pending.awaiting = Some(Awaiting::MarkSet);
                return vec![Effect::Message(
                    t!(
                        "印の名前を押してください（英数字）",
                        "press a letter or digit for the mark"
                    )
                    .into(),
                )];
            }
            "mark.jump" => {
                self.pending.awaiting = Some(Awaiting::MarkJump { exact: true });
                return vec![Effect::Message(
                    t!("飛ぶ印の名前を押してください", "press the mark to jump to").into(),
                )];
            }
            "macro.record" => {
                if self.macros.recording().is_some() {
                    Command::MacroRecord(None)
                } else {
                    self.pending.awaiting = Some(Awaiting::MacroRecord);
                    return vec![Effect::Message(
                        t!(
                            "マクロの名前を押してください（q で終了）",
                            "press a name for the macro (q to stop)"
                        )
                        .into(),
                    )];
                }
            }
            "macro.replay" => {
                if self.macros.last().is_some() {
                    Command::MacroReplay('@')
                } else {
                    self.pending.awaiting = Some(Awaiting::MacroReplay);
                    return vec![Effect::Message(
                        t!(
                            "再生するマクロの名前を押してください",
                            "press the macro to play"
                        )
                        .into(),
                    )];
                }
            }
            "register.list" => {
                let mut out: Vec<String> = self
                    .registers
                    .iter()
                    .map(|(n, v)| {
                        let head: String = v
                            .text
                            .lines()
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(16)
                            .collect();
                        format!("{}{n} {head}", '"')
                    })
                    .collect();
                for n in self.macros.names() {
                    out.push(format!("@{n}"));
                }
                return vec![Effect::Message(if out.is_empty() {
                    t!("レジスタは空です", "no registers yet").into()
                } else {
                    out.join("  ")
                })];
            }
            "register.select" => {
                self.pending.awaiting = Some(Awaiting::Register);
                return vec![Effect::Message(
                    t!(
                        "レジスタの文字を押してください（大文字で追記）",
                        "press a register letter (uppercase appends)"
                    )
                    .into(),
                )];
            }
            "motion.page" => Command::Move {
                motion: Motion::HalfPageDown,
                count: 1,
            },
            "motion.match" => Command::Move {
                motion: Motion::MatchPair,
                count: 1,
            },
            "motion.prompt" => Command::Move {
                motion: Motion::NextPrompt,
                count: 1,
            },
            "motion.error" => Command::Move {
                motion: Motion::NextError,
                count: 1,
            },
            "op.yank" | "op.send_to_prompt" => {
                let op = if id == "op.yank" {
                    OperatorId::Yank
                } else {
                    OperatorId::SendToPrompt
                };
                let Some(range) = selection else {
                    return vec![
                        Effect::Message(
                            t!("先に範囲を選んでください", "select something first").into(),
                        ),
                        Effect::Bell,
                    ];
                };
                Command::Apply {
                    op,
                    range,
                    register: None,
                }
            }
            // テキストオブジェクトはカーソル位置から範囲を作る。
            _ if id.starts_with("textobj.") => {
                let obj = match id {
                    "textobj.command" => TextObject::CommandBlock,
                    "textobj.output" => TextObject::OutputBlock,
                    "textobj.path" => TextObject::Path,
                    "textobj.url" => TextObject::Url,
                    "textobj.hash" => TextObject::Hash,
                    "textobj.number" => TextObject::Number,
                    "textobj.error" => TextObject::ErrorBlock,
                    _ => TextObject::Word { big: false },
                };
                let around = matches!(obj, TextObject::CommandBlock | TextObject::ErrorBlock);
                let Some(range) = textobj::range_of(buf, self.cursor, obj, around) else {
                    return vec![
                        Effect::Message(
                            t!(
                                "カーソルの下に見つかりません",
                                "nothing like that under the cursor"
                            )
                            .into(),
                        ),
                        Effect::Bell,
                    ];
                };
                Command::Select { range }
            }
            _ => {
                return vec![Effect::Message(t!(
                    format!("{id} はここからは実行できません（キーで使ってください）"),
                    format!("{id} cannot be run from here (use its key)")
                ))];
            }
        };
        self.execute(cmd, buf)
    }

    /// 印へ飛ぶキーを `Command` にする。オペレータ待ちなら範囲へ畳む。
    ///
    /// バッククォートは exclusive、`'` は linewise。vim と同じ区別で、
    /// `d'a` が行ごと消えるのに対し桁で切る方も残る。
    fn mark_command(&mut self, name: char, exact: bool, buf: &dyn Buffer) -> Option<Command> {
        let Some(target) = self.marks.get(name) else {
            self.pending.clear();
            // 実行時に「ありません」を出させる。ここで黙って捨てない。
            return Some(Command::JumpMark { name, exact });
        };
        if let Some(op) = self.pending.operator.take() {
            let kind = if exact {
                MotionKind::Exclusive
            } else {
                MotionKind::Linewise
            };
            let range = range_for(self.cursor, clamp(buf, target), kind, buf);
            let register = self.pending.register.take();
            self.pending.clear();
            return Some(Command::Apply {
                op,
                range,
                register,
            });
        }
        self.pending.clear();
        Some(Command::JumpMark { name, exact })
    }

    /// `i` / `a` に続くオブジェクト文字を範囲へ落とす。
    ///
    /// オペレータ待ちなら `Apply`、Visual なら `Select` になる。
    /// マウスのダブルクリックも同じ `textobj` を通って同じ `Command` を出す
    /// （`mouse-parity.md` §2.1 の単一コマンドバス）。
    fn text_object(&mut self, c: char, around: bool, buf: &dyn Buffer) -> Option<Command> {
        let obj = TextObject::from_key(c, buf.kind());
        let range = obj.and_then(|o| textobj::range_of(buf, self.cursor, o, around));

        let Some(range) = range else {
            // 何も取れなかったことは黙って捨てない（`modal-spec.md` の方針）
            self.pending.clear();
            self.mode = Mode::Normal;
            return None;
        };

        if let Some(op) = self.pending.operator.take() {
            let register = self.pending.register.take();
            self.pending.clear();
            return Some(Command::Apply {
                op,
                range,
                register,
            });
        }
        self.pending.clear();
        Some(Command::Select { range })
    }

    /// モーションを `Command` にする。operator-pending なら範囲へ畳む。
    fn motion_command(&mut self, m: Motion, buf: &dyn Buffer) -> Option<Command> {
        let count = self.take_count();
        let target = motion::apply(m, self.cursor, count, buf, &self.motion_ctx());

        if let Some(op) = self.pending.operator.take() {
            let range = range_for(self.cursor, target, m.kind(), buf);
            let register = self.pending.register.take();
            self.pending.clear();
            return Some(Command::Apply {
                op,
                range,
                register,
            });
        }
        Some(Command::Move { motion: m, count })
    }

    fn take_count(&mut self) -> usize {
        self.pending.count.take().unwrap_or(1).max(1)
    }

    // ---- 唯一のディスパッチャ ---------------------------------------------

    /// すべての入力フロントエンド（キーボード・マウス・パレット・RPC）が
    /// ここを通る。`arch.md` の不変条件 1。
    pub fn execute(&mut self, cmd: Command, buf: &dyn Buffer) -> Vec<Effect> {
        match cmd {
            Command::EnterInsert(at) => self.enter_insert(at, buf),
            Command::EnterNormal => {
                let changed = self.mode != Mode::Normal;
                self.mode = Mode::Normal;
                self.anchor = None;
                self.pending.clear();
                self.cursor = clamp(buf, self.cursor);
                if changed {
                    vec![Effect::ModeChanged(Mode::Normal)]
                } else {
                    Vec::new()
                }
            }
            Command::EnterVisual(kind) => {
                // 同じ種別をもう一度押したら解除（vim と同じ）
                if self.mode == Mode::Visual(kind) {
                    return self.execute(Command::EnterNormal, buf);
                }
                if self.anchor.is_none() {
                    self.anchor = Some(self.cursor);
                }
                self.mode = Mode::Visual(kind);
                vec![Effect::ModeChanged(self.mode)]
            }
            Command::Move { motion, count } => {
                let target = motion::apply(motion, self.cursor, count, buf, &self.motion_ctx());
                self.cursor = target;
                if self.mode == Mode::OperatorPending {
                    self.mode = Mode::Normal;
                }
                vec![Effect::CursorMoved(target)]
            }
            Command::SetCursor(pos) => {
                self.cursor = clamp(buf, pos);
                vec![Effect::CursorMoved(self.cursor)]
            }
            Command::Select { range } => {
                let kind = match range.kind {
                    RangeKind::Line => VisualKind::Line,
                    RangeKind::Block => VisualKind::Block,
                    RangeKind::Char => VisualKind::Char,
                };
                self.anchor = Some(clamp(buf, range.start));
                self.cursor = clamp(buf, range.end);
                self.mode = Mode::Visual(kind);
                self.pending.clear();
                vec![
                    Effect::ModeChanged(self.mode),
                    Effect::CursorMoved(self.cursor),
                ]
            }
            Command::Scroll(delta) => vec![Effect::Scrolled(delta)],
            Command::OpenSearch { back } => {
                self.mode = Mode::Normal;
                self.pending.clear();
                vec![Effect::OpenSearch { back }]
            }
            Command::SetSearch(q) => {
                self.search = (!q.is_empty()).then(|| Search::new(&q, self.search_regex));
                Vec::new()
            }
            Command::Palette(prefix) => {
                self.mode = Mode::Normal;
                self.pending.clear();
                vec![Effect::Palette(prefix.to_string())]
            }
            Command::File(action) => vec![Effect::File(action)],
            Command::History(action) => {
                self.mode = Mode::Normal;
                self.anchor = None;
                self.pending.clear();
                vec![Effect::History(action)]
            }
            Command::EnterLayout => {
                self.mode = Mode::Layout;
                self.pending.clear();
                vec![Effect::ModeChanged(Mode::Layout)]
            }
            Command::Mux(req) => {
                // 配置モードは持続する（連続で分割・移動できる）。
                // ただしデタッチは抜けるので通常モードへ戻す。
                if matches!(req, MuxRequest::Detach | MuxRequest::Shutdown) {
                    self.mode = Mode::Normal;
                }
                vec![Effect::Mux(req)]
            }
            Command::ToggleHelp => {
                self.help_visible = !self.help_visible;
                vec![Effect::HelpToggled(self.help_visible)]
            }
            Command::Apply {
                op,
                range,
                register,
            } => self.apply_operator(op, range, register, buf),
            Command::SetMark(name) => {
                self.pending.clear();
                if !name.is_ascii_alphanumeric() {
                    return vec![
                        Effect::Message(
                            t!(
                                "マークの名前は英数字です",
                                "marks are named with letters or digits"
                            )
                            .into(),
                        ),
                        Effect::Bell,
                    ];
                }
                self.marks.set(name, self.cursor);
                vec![
                    Effect::MarkSet {
                        name,
                        pos: self.cursor,
                    },
                    Effect::Message(t!(
                        format!("マーク {name} を置きました"),
                        format!("mark {name} set")
                    )),
                ]
            }
            Command::JumpMark { name, exact } => {
                self.pending.clear();
                let Some(target) = self.marks.get(name) else {
                    return vec![
                        Effect::Message(t!(
                            format!("マーク {name} はありません"),
                            format!("no mark {name}")
                        )),
                        Effect::Bell,
                    ];
                };
                // 飛ぶ前の位置を覚えておく。同じキーの二度押しで戻れる。
                let back = self.cursor;
                self.marks.set('`', back);
                self.marks.set('\'', back);
                let target = if exact {
                    clamp(buf, target)
                } else {
                    clamp(
                        buf,
                        Pos::new(target.line, first_non_blank(buf, target.line)),
                    )
                };
                self.cursor = target;
                if self.mode == Mode::OperatorPending {
                    self.mode = Mode::Normal;
                }
                vec![Effect::CursorMoved(target)]
            }
            Command::Paste { before } => self.paste(before, buf),
            Command::MacroRecord(Some(name)) => {
                self.pending.clear();
                if !name.is_ascii_alphanumeric() {
                    return vec![
                        Effect::Message(
                            t!(
                                "マクロの名前は英数字です",
                                "macros are named with letters or digits"
                            )
                            .into(),
                        ),
                        Effect::Bell,
                    ];
                }
                self.macros.recording = Some((name, Vec::new()));
                vec![
                    Effect::MacroRecording(Some(name)),
                    Effect::Message(t!(
                        format!("マクロ {name} を記録中（q で終了）"),
                        format!("recording macro {name} (q to stop)")
                    )),
                ]
            }
            Command::MacroRecord(None) => {
                let Some((name, keys)) = self.macros.recording.take() else {
                    return vec![Effect::Message(
                        t!("記録していません", "not recording").into(),
                    )];
                };
                let n = keys.len();
                self.macros.stored.insert(name, keys);
                self.macros.last = Some(name);
                vec![
                    Effect::MacroRecording(None),
                    Effect::Message(t!(
                        format!("マクロ {name} を記録しました（{n} キー）"),
                        format!("macro {name} recorded ({n} keys)")
                    )),
                ]
            }
            Command::MacroReplay(name) => {
                self.pending.clear();
                // `@@` は直前のものをもう一度。
                let name = if name == '@' {
                    match self.macros.last {
                        Some(n) => n,
                        None => {
                            return vec![
                                Effect::Message(
                                    t!("再生できるマクロがありません", "no macro to play").into(),
                                ),
                                Effect::Bell,
                            ];
                        }
                    }
                } else {
                    name
                };
                let Some(keys) = self.macros.get(name).map(<[KeyInput]>::to_vec) else {
                    return vec![
                        Effect::Message(t!(
                            format!("マクロ {name} はありません"),
                            format!("no macro {name}")
                        )),
                        Effect::Bell,
                    ];
                };
                self.macros.last = Some(name);
                vec![Effect::MacroReplay(keys)]
            }
            Command::SetTheme(name) => vec![Effect::SetTheme(name.to_string())],
            Command::OpenConfig => vec![Effect::OpenConfig],
            Command::Quit => vec![Effect::Quit],
        }
    }

    /// `i` `a` `I` `A` `o` `O`。
    ///
    /// 端末では位置に意味が無いので、どれもそのまま入力モードへ入る
    /// （ホストが生きている末尾へカーソルを寄せる）。
    fn enter_insert(&mut self, at: InsertAt, buf: &dyn Buffer) -> Vec<Effect> {
        self.mode = Mode::Insert;
        self.anchor = None;
        self.pending.clear();
        let entered = vec![Effect::ModeChanged(Mode::Insert)];
        if buf.kind() != BufferKind::File {
            return entered;
        }

        let line = self.cursor.line.min(buf.line_count().saturating_sub(1));
        let width = line_width(buf, line);
        match at {
            InsertAt::Here => entered,
            InsertAt::After => {
                let w = buf
                    .cells(line)
                    .and_then(|c| c.get(self.cursor.col))
                    .map_or(1, |c| usize::from(c.width.max(1)));
                self.cursor = Pos::new(line, (self.cursor.col + w).min(width));
                let mut fx = entered;
                fx.push(Effect::CursorMoved(self.cursor));
                fx
            }
            InsertAt::LineStart => {
                self.cursor = Pos::new(line, first_non_blank(buf, line));
                let mut fx = entered;
                fx.push(Effect::CursorMoved(self.cursor));
                fx
            }
            InsertAt::LineEnd => {
                self.cursor = Pos::new(line, width);
                let mut fx = entered;
                fx.push(Effect::CursorMoved(self.cursor));
                fx
            }
            InsertAt::Below => {
                self.cursor = Pos::new(line + 1, 0);
                let mut fx = entered;
                fx.push(Effect::Insert {
                    at: Pos::new(line, width),
                    text: "\n".into(),
                    cursor: Some(self.cursor),
                });
                fx
            }
            InsertAt::Above => {
                // 行 0 の上は「先頭に改行を差す」。それ以外は「前の行の下に開ける」
                // に落とすと、同じ 1 本の経路で済む。
                let (at, cursor) = if line == 0 {
                    (Pos::new(0, 0), Pos::new(0, 0))
                } else {
                    (
                        Pos::new(line - 1, line_width(buf, line - 1)),
                        Pos::new(line, 0),
                    )
                };
                self.cursor = cursor;
                let mut fx = entered;
                fx.push(Effect::Insert {
                    at,
                    text: "\n".into(),
                    cursor: Some(cursor),
                });
                fx
            }
        }
    }

    /// `p` / `P`。
    ///
    /// 端末では「表示に文字を差し込む」ことに意味が無い（次の出力で流れる）。
    /// **プロンプトへ入れるのが端末での自然な等価**なので、そう振る舞う。
    /// ファイルでは普通に差し込む。
    fn paste(&mut self, before: bool, buf: &dyn Buffer) -> Vec<Effect> {
        let name = self.pending.register.take().unwrap_or('"');
        self.pending.clear();
        let Some(value) = self.registers.get(name).cloned() else {
            return vec![
                Effect::Message(t!(
                    format!("レジスタ {name} は空です"),
                    format!("register {name} is empty")
                )),
                Effect::Bell,
            ];
        };

        if buf.kind() != BufferKind::File {
            let text = value.text.trim_end_matches(['\r', '\n']).to_string();
            if text.is_empty() {
                return vec![Effect::Bell];
            }
            self.mode = Mode::Insert;
            return vec![
                Effect::SendToPrompt(text),
                Effect::ModeChanged(Mode::Insert),
            ];
        }

        self.mode = Mode::Normal;
        self.anchor = None;
        if value.kind == RangeKind::Line {
            let mut text = value.text.clone();
            if !text.ends_with('\n') {
                text.push('\n');
            }
            let line = if before {
                self.cursor.line
            } else {
                self.cursor.line + 1
            };
            // 末尾より下へ貼るときは、最終行の後ろに改行から足す
            // （「最終行の次の行」はまだ無いので、差し込み先として指せない）。
            if line >= buf.line_count() {
                let last = buf.line_count().saturating_sub(1);
                return vec![Effect::Insert {
                    at: Pos::new(last, line_width(buf, last)),
                    text: format!("{}{}", '\n', text.trim_end_matches('\n')),
                    cursor: Some(Pos::new(last + 1, 0)),
                }];
            }
            return vec![Effect::Insert {
                at: Pos::new(line, 0),
                text,
                cursor: Some(Pos::new(line, 0)),
            }];
        }

        // 全角の上で `p` を押したら、その 2 セル先へ入れる。
        let width = buf
            .cells(self.cursor.line)
            .and_then(|cells| cells.get(self.cursor.col))
            .map_or(1, |c| usize::from(c.width.max(1)));
        let col = if before {
            self.cursor.col
        } else {
            self.cursor.col + width
        };
        vec![Effect::Insert {
            at: Pos::new(self.cursor.line, col),
            text: value.text,
            cursor: None,
        }]
    }

    fn apply_operator(
        &mut self,
        op: OperatorId,
        range: Range,
        register: Option<char>,
        buf: &dyn Buffer,
    ) -> Vec<Effect> {
        self.mode = Mode::Normal;
        self.anchor = None;
        self.pending.clear();

        if !buf.allows(op) {
            return vec![
                Effect::Message(t!(
                    format!("このバッファでは {op:?} は使えません（{:?}）", buf.kind()),
                    format!("{op:?} does not apply to this buffer ({:?})", buf.kind())
                )),
                Effect::Bell,
            ];
        }

        match op {
            OperatorId::Yank => {
                let text = extract(buf, &range);
                let lines = if range.kind == RangeKind::Char && !text.contains('\n') {
                    1
                } else {
                    text.lines().count()
                };
                let chars = text.chars().count();
                self.registers.yank(
                    register,
                    RegisterValue {
                        text: text.clone(),
                        kind: range.kind,
                    },
                );
                self.cursor = clamp(buf, range.start);

                let mut effects = vec![
                    Effect::Yanked {
                        register: register.unwrap_or('"'),
                        chars,
                        lines,
                    },
                    Effect::CursorMoved(self.cursor),
                ];
                if self.clipboard_on_yank {
                    effects.push(Effect::SetClipboard(text));
                }
                effects
            }
            // `!` — 範囲を現在のプロンプトへ挿入する。Enter は押さない
            // （`modal-spec.md` §7「実行はしない。Enter はユーザーが押す」）。
            OperatorId::SendToPrompt => {
                let text = extract(buf, &range);
                let text = text.trim_end_matches(['\r', '\n']).to_string();
                if text.is_empty() {
                    return vec![Effect::Bell];
                }
                self.mode = Mode::Insert;
                vec![
                    Effect::SendToPrompt(text),
                    Effect::ModeChanged(Mode::Insert),
                ]
            }
            // `d` / `c` — 消す。消した分はレジスタへ入れる（vim と同じ）。
            OperatorId::Delete | OperatorId::Change => {
                let text = extract(buf, &range);
                self.registers.yank(
                    register,
                    RegisterValue {
                        text: text.clone(),
                        kind: range.kind,
                    },
                );
                self.cursor = clamp(buf, range.start);
                let mut effects = vec![
                    Effect::Edit {
                        range,
                        text: String::new(),
                    },
                    Effect::CursorMoved(self.cursor),
                ];
                if op == OperatorId::Change {
                    self.mode = Mode::Insert;
                    effects.push(Effect::ModeChanged(Mode::Insert));
                }
                effects
            }
            // `=` — 整形して置き換える。判定できないものはそのまま返るので実害が無い。
            OperatorId::Format => {
                let text = extract(buf, &range);
                let formatted = format::format(&text);
                if formatted == text {
                    return vec![Effect::Message(
                        t!(
                            "整形できる形ではありませんでした",
                            "nothing here looks formattable"
                        )
                        .into(),
                    )];
                }
                self.cursor = clamp(buf, range.start);
                vec![
                    Effect::Edit {
                        range,
                        text: formatted,
                    },
                    Effect::CursorMoved(self.cursor),
                ]
            }
            // `>` はプロセスを起こす層へ降ろす。ここでは中身を渡すだけ。
            OperatorId::Pipe => {
                let input = extract(buf, &range);
                if input.trim().is_empty() {
                    return vec![
                        Effect::Message(t!("流し込むものがありません", "nothing to pipe").into()),
                        Effect::Bell,
                    ];
                }
                vec![Effect::Pipe { input }]
            }
        }
    }
}

/// オペレータの範囲を組む。vim の exclusive / inclusive / linewise に従う。
fn range_for(from: Pos, to: Pos, kind: MotionKind, buf: &dyn Buffer) -> Range {
    match kind {
        MotionKind::Linewise => Range::new(
            Pos::new(from.line, 0),
            Pos::new(to.line, 0),
            RangeKind::Line,
        ),
        MotionKind::Inclusive => Range::new(from, to, RangeKind::Char),
        MotionKind::Exclusive => {
            let (start, end) = if from <= to { (from, to) } else { (to, from) };
            // exclusive は行き先を含まないので、末尾を1セル手前へ寄せる。
            let end = motion::step_back(buf, end).unwrap_or(start).max(start);
            Range::new(start, end, RangeKind::Char)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{FocusDir, InsertAt, MuxRequest, SplitDir};
    use tsg_buffer::TermBuffer;
    use tsg_term::{AmbiguousWidth, Terminal};

    /// キー列を流し込み、最後の状態と副作用を得るゴールデンテスト用のハーネス。
    struct Harness {
        term: Terminal,
        engine: Engine,
    }

    impl Harness {
        fn new(text: &str) -> Self {
            let mut term = Terminal::new(40, 8, AmbiguousWidth::Wide);
            term.feed(text.as_bytes());
            let mut engine = Engine::new();
            engine.set_view(View { top: 0, height: 8 });
            engine.mode = Mode::Normal;
            Self { term, engine }
        }

        fn keys(&mut self, s: &str) -> Vec<Effect> {
            let buf = TermBuffer::new(&self.term.state.grid, &self.term.state.marks);
            let mut out = Vec::new();
            for c in s.chars() {
                if let KeyOutcome::Handled(fx) = self.engine.key(KeyInput::Char(c), &buf) {
                    out.extend(fx);
                }
            }
            out
        }

        fn ctrl(&mut self, c: char) -> Vec<Effect> {
            let buf = TermBuffer::new(&self.term.state.grid, &self.term.state.marks);
            match self.engine.key(KeyInput::Ctrl(c), &buf) {
                KeyOutcome::Handled(fx) => fx,
                KeyOutcome::PassThrough => Vec::new(),
            }
        }

        fn yanked(&self) -> &str {
            self.engine
                .registers
                .get('"')
                .map_or("", |v| v.text.as_str())
        }
    }

    #[test]
    fn motions_move_the_cursor() {
        let mut h = Harness::new("hello world\r\nsecond line\r\n");
        h.keys("w");
        assert_eq!(h.engine.cursor(), Pos::new(0, 6));
        h.keys("j");
        assert_eq!(h.engine.cursor().line, 1);
        h.keys("0");
        assert_eq!(h.engine.cursor(), Pos::new(1, 0));
        h.keys("$");
        assert_eq!(h.engine.cursor(), Pos::new(1, 10));
    }

    #[test]
    fn counts_apply_to_motions() {
        let mut h = Harness::new("abcdefghij");
        h.keys("3l");
        assert_eq!(h.engine.cursor(), Pos::new(0, 3));
        h.keys("12l");
        assert_eq!(h.engine.cursor().col, 9, "行末で止まる");
    }

    #[test]
    fn yank_with_motion_is_exclusive() {
        let mut h = Harness::new("hello world");
        h.keys("yw");
        assert_eq!(
            h.yanked(),
            "hello ",
            "w は exclusive なので行き先を含まない"
        );
    }

    #[test]
    fn yank_with_inclusive_motion_includes_target() {
        let mut h = Harness::new("hello world");
        h.keys("ye");
        assert_eq!(h.yanked(), "hello", "e は inclusive");
    }

    #[test]
    fn doubled_operator_is_linewise() {
        let mut h = Harness::new("one\r\ntwo\r\nthree\r\n");
        h.keys("yy");
        assert_eq!(h.yanked(), "one\n");
        h.keys("2yy");
        assert_eq!(h.yanked(), "one\ntwo\n");
    }

    #[test]
    fn yank_to_named_register() {
        let mut h = Harness::new("hello world");
        h.keys("\"ayw");
        assert_eq!(h.engine.registers.get('a').unwrap().text, "hello ");
        assert_eq!(h.yanked(), "hello ", "無名レジスタにも入る");
    }

    #[test]
    fn uppercase_register_appends() {
        let mut h = Harness::new("one\r\ntwo\r\n");
        h.keys("\"ayy");
        h.keys("j\"Ayy");
        assert_eq!(h.engine.registers.get('a').unwrap().text, "one\ntwo\n");
    }

    #[test]
    fn visual_selection_then_yank() {
        let mut h = Harness::new("hello world");
        h.keys("vee");
        let sel = h.engine.selection().expect("選択中のはず");
        assert_eq!(sel.start, Pos::new(0, 0));
        h.keys("y");
        assert_eq!(h.yanked(), "hello world");
        assert_eq!(h.engine.mode(), Mode::Normal, "ヤンク後は Normal へ戻る");
    }

    #[test]
    fn visual_line_yanks_whole_lines() {
        let mut h = Harness::new("one\r\ntwo\r\nthree\r\n");
        h.keys("Vj");
        h.keys("y");
        assert_eq!(h.yanked(), "one\ntwo\n");
    }

    #[test]
    fn visual_block_yanks_a_column() {
        let mut h = Harness::new("abcdef\r\nghijkl\r\nmnopqr\r\n");
        h.ctrl('v');
        h.keys("jjll");
        h.keys("y");
        assert_eq!(h.yanked(), "abc\nghi\nmno\n");
    }

    #[test]
    fn yank_puts_text_on_the_clipboard() {
        let mut h = Harness::new("hello");
        let fx = h.keys("yy");
        assert!(
            fx.iter()
                .any(|e| matches!(e, Effect::SetClipboard(s) if s == "hello\n")),
            "Term のヤンクは既定でクリップボードへ入る"
        );
    }

    #[test]
    fn find_char_takes_the_next_key_as_an_argument() {
        let mut h = Harness::new("hello world");
        h.keys("fo");
        assert_eq!(h.engine.cursor(), Pos::new(0, 4));
        h.keys(";");
        assert_eq!(h.engine.cursor(), Pos::new(0, 7), "; で繰り返す");
    }

    #[test]
    fn gg_and_g_move_between_ends() {
        let mut h = Harness::new("one\r\ntwo\r\nthree\r\n");
        h.keys("G");
        assert_eq!(h.engine.cursor().line, 7, "グリッド末尾");
        h.keys("gg");
        assert_eq!(h.engine.cursor().line, 0);
        h.keys("2gg");
        assert_eq!(h.engine.cursor().line, 1);
    }

    #[test]
    fn esc_clears_pending_state() {
        let mut h = Harness::new("hello");
        h.keys("2y");
        assert!(!h.engine.pending_hint().is_empty());
        let buf = TermBuffer::new(&h.term.state.grid, &h.term.state.marks);
        h.engine.key(KeyInput::Esc, &buf);
        assert_eq!(h.engine.pending_hint(), "");
        assert_eq!(h.engine.mode(), Mode::Normal);
    }

    // ---- オペレータ `d` `c` `=`（`modal-spec.md` §7） ----

    /// `Effect::Edit` を取り出す。
    fn edit_of(fx: &[Effect]) -> Option<(&Range, &str)> {
        fx.iter().find_map(|e| match e {
            Effect::Edit { range, text } => Some((range, text.as_str())),
            _ => None,
        })
    }

    #[test]
    fn delete_yanks_what_it_removes_and_asks_the_host_to_edit() {
        let mut h = Harness::new("one\r\ntwo\r\nthree\r\n");
        h.keys("gg");
        let fx = h.keys("dd");

        let (range, text) = edit_of(&fx).expect("Edit が出ていない");
        assert_eq!(range.kind, RangeKind::Line);
        assert_eq!(range.start.line, 0);
        assert_eq!(text, "", "d は空で置き換える");
        assert_eq!(h.yanked(), "one\n", "消した分がレジスタに入っていない");
        assert_eq!(h.engine.mode(), Mode::Normal);
    }

    #[test]
    fn change_deletes_then_enters_insert_on_a_file() {
        // `c` は File バッファでのみ許される（§7）。モーションもオブジェクトも
        // 端末と同じものがそのまま効く、というのがここでの見どころ。
        let file = tsg_buffer::FileBuffer::from_text("hello world", AmbiguousWidth::Wide);
        let mut e = Engine::new();
        e.set_view(View { top: 0, height: 8 });
        e.mode = Mode::Normal;

        let fx = ["c", "w"]
            .iter()
            .flat_map(
                |k| match e.key(KeyInput::Char(k.chars().next().unwrap()), &file) {
                    KeyOutcome::Handled(fx) => fx,
                    KeyOutcome::PassThrough => Vec::new(),
                },
            )
            .collect::<Vec<_>>();

        let (range, text) = edit_of(&fx).expect("Edit が出ていない");
        assert_eq!(range.start, Pos::new(0, 0));
        assert_eq!(text, "");
        assert_eq!(e.mode(), Mode::Insert, "c の後に入力モードへ入らない");
    }

    #[test]
    fn delete_is_refused_on_the_alt_screen() {
        // `modal-spec.md` §7 の可否表。alt では表示を書き換えない。
        let mut term = Terminal::new(40, 8, AmbiguousWidth::Wide);
        term.feed(b"\x1b[?1049h");
        term.feed(b"tui output\r\n");
        let mut engine = Engine::new();
        engine.set_view(View { top: 0, height: 8 });
        engine.mode = Mode::Normal;
        let mut h = Harness { term, engine };

        let fx = h.keys("dd");
        assert!(edit_of(&fx).is_none(), "alt で削除が通っている");
        assert!(fx.iter().any(|e| matches!(e, Effect::Bell)));
    }

    #[test]
    fn equals_formats_json_in_place() {
        let mut h = Harness::new("{\"a\":1,\"b\":2}\r\n");
        h.keys("gg");
        let fx = h.keys("==");
        let (_, text) = edit_of(&fx).expect("Edit が出ていない");
        assert!(
            text.starts_with("{\n  \"a\": 1"),
            "整形されていない: {text:?}"
        );
    }

    #[test]
    fn equals_says_so_instead_of_mangling_prose() {
        let mut h = Harness::new("ただの文章です\r\n");
        h.keys("gg");
        let fx = h.keys("==");
        assert!(edit_of(&fx).is_none(), "整形できないものを書き換えている");
        assert!(fx.iter().any(|e| matches!(e, Effect::Message(_))));
    }

    #[test]
    fn u_and_ctrl_r_ask_the_host_for_undo_and_redo() {
        let mut h = Harness::new("hello");
        let fx = h.keys("u");
        assert!(
            fx.contains(&Effect::History(HistoryAction::Undo)),
            "u が取り消しになっていない: {fx:?}"
        );
        let fx = h.ctrl('r');
        assert!(fx.contains(&Effect::History(HistoryAction::Redo)));
    }

    #[test]
    fn undo_leaves_visual_mode_first() {
        // 選択したまま戻すと、範囲が消えた行を指したままになる
        let mut h = Harness::new("one\r\ntwo\r\n");
        h.keys("gg");
        h.keys("V");
        assert!(matches!(h.engine.mode(), Mode::Visual(_)));
        h.keys("u");
        assert_eq!(h.engine.mode(), Mode::Normal);
        assert!(h.engine.selection().is_none());
    }

    // ---- ターミナル固有モーション（`modal-spec.md` §5.2） ----

    /// プロンプト2つ・2つ目が失敗、という履歴を作る。
    fn history() -> Harness {
        let mut term = Terminal::new(40, 12, AmbiguousWidth::Wide);
        term.feed(
            b"\x1b]133;A\x07$ \x1b]133;B\x07ok-cmd\r\n\x1b]133;C\x07fine\r\n\x1b]133;D;0\x07",
        );
        term.feed(
            b"\x1b]133;A\x07$ \x1b]133;B\x07bad-cmd\r\n\x1b]133;C\x07boom\r\n\x1b]133;D;2\x07",
        );
        term.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07last\r\n");
        let mut engine = Engine::new();
        engine.set_view(View { top: 0, height: 12 });
        engine.mode = Mode::Normal;
        Harness { term, engine }
    }

    #[test]
    fn bracket_motions_walk_between_prompts() {
        let mut h = history();
        h.keys("G");
        let start = h.engine.cursor().line;

        h.keys("[[");
        let first_back = h.engine.cursor().line;
        assert!(first_back < start, "前のプロンプトへ戻っていない");

        h.keys("[[");
        assert!(
            h.engine.cursor().line < first_back,
            "2 回目が効いていない（同じ行に留まっている）"
        );

        h.keys("]]");
        assert_eq!(
            h.engine.cursor().line,
            first_back,
            "次のプロンプトへ戻れない"
        );
    }

    #[test]
    fn bracket_e_only_stops_at_failures() {
        let mut h = history();
        h.keys("gg");
        h.keys("]e");
        let err = h.engine.cursor().line;

        // 失敗したのは 2 番目のコマンドだけ。その行から先に候補は無い。
        h.keys("]e");
        assert_eq!(
            h.engine.cursor().line,
            err,
            "成功したコマンドまで拾っている"
        );

        let text = {
            let buf = TermBuffer::new(&h.term.state.grid, &h.term.state.marks);
            tsg_buffer::line_text(&buf, err)
        };
        assert!(
            text.contains("bad-cmd"),
            "止まった先が失敗行ではない: {text:?}"
        );
    }

    #[test]
    fn a_count_applies_to_prompt_motion() {
        let mut h = history();
        h.keys("G");
        h.keys("2[[");
        let two = h.engine.cursor().line;

        let mut g = history();
        g.keys("G");
        g.keys("[[");
        g.keys("[[");
        assert_eq!(
            two,
            g.engine.cursor().line,
            "count が 1 回ぶんしか効いていない"
        );
    }

    // ---- テキストオブジェクト（`modal-spec.md` §6） ----

    /// 行内の目印の列へカーソルを置く。
    fn at(h: &mut Harness, needle: &str) {
        let buf = TermBuffer::new(&h.term.state.grid, &h.term.state.marks);
        let text = tsg_buffer::line_text(&buf, 0);
        let byte = text.find(needle).expect("目印が行に無い");
        let col: usize = text[..byte]
            .chars()
            .map(|c| tsg_term::char_width(c, AmbiguousWidth::Wide))
            .sum();
        h.engine.execute(Command::SetCursor(Pos::new(0, col)), &buf);
    }

    #[test]
    fn yif_yanks_the_whole_path_not_just_a_word() {
        let mut h = Harness::new("error at src/main.rs:42:8 here");
        at(&mut h, "main");
        h.keys("yif");
        assert_eq!(h.yanked(), "src/main.rs");

        let mut h = Harness::new("error at src/main.rs:42:8 here");
        at(&mut h, "main");
        h.keys("yaf");
        assert_eq!(h.yanked(), "src/main.rs:42:8");
    }

    #[test]
    fn vif_selects_the_same_range_that_yif_yanks() {
        // 「範囲を作る」と「範囲に何かする」が分離していることの確認。
        // マウス（ドラッグ -> 右クリック -> コピー）が通るのと同じ道。
        let mut h = Harness::new("see src/main.rs here");
        at(&mut h, "main");
        h.keys("vif");
        assert_eq!(h.engine.mode(), Mode::Visual(VisualKind::Char));
        h.keys("y");
        assert_eq!(h.yanked(), "src/main.rs");
    }

    #[test]
    fn bang_sends_the_object_to_the_prompt_without_running_it() {
        let mut h = Harness::new("built target/debug/tsg.exe ok");
        at(&mut h, "debug");
        let fx = h.keys("!if");

        assert!(
            fx.contains(&Effect::SendToPrompt("target/debug/tsg.exe".into())),
            "プロンプトへ送られていない: {fx:?}"
        );
        assert_eq!(h.engine.mode(), Mode::Insert, "送った後は入力モードへ入る");
        assert!(
            !fx.iter().any(|e| matches!(e, Effect::Message(_))),
            "未実装扱いのままになっている: {fx:?}"
        );
    }

    #[test]
    fn yac_takes_the_whole_command_block() {
        let mut term = Terminal::new(40, 8, AmbiguousWidth::Wide);
        term.feed(
            b"\x1b]133;A\x07$ \x1b]133;B\x07ls -la\r\n\x1b]133;C\x07a.txt\r\n\x1b]133;D;0\x07",
        );
        let mut engine = Engine::new();
        engine.set_view(View { top: 0, height: 8 });
        engine.mode = Mode::Normal;
        let mut h = Harness { term, engine };

        {
            let buf = TermBuffer::new(&h.term.state.grid, &h.term.state.marks);
            h.engine.execute(Command::SetCursor(Pos::new(1, 0)), &buf);
        }
        h.keys("yac");
        let got = h.yanked();
        assert!(
            got.contains("$ ls -la"),
            "プロンプト行が入っていない: {got:?}"
        );
        assert!(got.contains("a.txt"), "出力が入っていない: {got:?}");

        h.keys("yic");
        assert_eq!(h.yanked(), "ls -la", "ic はコマンド行だけを取る");
    }

    #[test]
    fn an_object_that_matches_nothing_does_not_wedge_the_engine() {
        let mut h = Harness::new("plain words only");
        at(&mut h, "words");
        h.keys("yif"); // パスは無い
        assert_eq!(
            h.engine.mode(),
            Mode::Normal,
            "待機モードに取り残されている"
        );
        h.keys("yiw");
        assert_eq!(h.yanked(), "words", "その後の操作が効かなくなっている");
    }

    #[test]
    fn insert_mode_passes_keys_through() {
        let mut h = Harness::new("hello");
        let buf = TermBuffer::new(&h.term.state.grid, &h.term.state.marks);
        h.engine.execute(Command::EnterInsert(InsertAt::Here), &buf);
        assert_eq!(h.engine.mode(), Mode::Insert);
        assert_eq!(
            h.engine.key(KeyInput::Char('x'), &buf),
            KeyOutcome::PassThrough
        );
    }

    #[test]
    fn alt_buffer_refuses_delete_but_allows_yank() {
        let mut h = Harness::new("hello");
        h.term.feed(b"\x1b[?1049h");
        h.term.feed(b"alt content");
        let fx = h.keys("dd");
        assert!(
            fx.iter().any(|e| matches!(e, Effect::Bell)),
            "alt バッファでの d は拒否されるべき"
        );
        let fx = h.keys("yy");
        assert!(fx.iter().any(|e| matches!(e, Effect::Yanked { .. })));
    }

    #[test]
    fn change_is_refused_on_a_terminal_but_delete_is_not() {
        // `modal-spec.md` §7 の可否表。Term(primary) は `d` 可 / `c` 不可。
        let mut h = Harness::new("hello world");
        assert!(edit_of(&h.keys("dw")).is_some(), "端末で d が効かない");

        let mut h = Harness::new("hello world");
        let fx = h.keys("cw");
        assert!(edit_of(&fx).is_none(), "端末で c が通っている");
        assert!(fx.iter().any(|e| matches!(e, Effect::Message(_))));
    }

    #[test]
    fn wide_chars_do_not_break_yank() {
        let mut h = Harness::new("日本語のログ");
        h.keys("y$");
        assert_eq!(h.yanked(), "日本語のログ");
    }

    #[test]
    fn help_opens_with_f1_and_closes_with_any_key() {
        let mut h = Harness::new("hello");
        let buf = TermBuffer::new(&h.term.state.grid, &h.term.state.marks);
        assert!(!h.engine.help_visible());

        h.engine.key(KeyInput::Function(1), &buf);
        assert!(h.engine.help_visible());

        // 読んでいる最中にモードが変わらないこと。`j` はカーソルを動かさず閉じるだけ。
        let before = h.engine.cursor();
        h.engine.key(KeyInput::Char('j'), &buf);
        assert!(!h.engine.help_visible());
        assert_eq!(
            h.engine.cursor(),
            before,
            "ヘルプを閉じるキーが操作に化けない"
        );
    }

    #[test]
    fn f1_opens_help_even_from_insert_mode() {
        let mut h = Harness::new("hello");
        let buf = TermBuffer::new(&h.term.state.grid, &h.term.state.marks);
        h.engine.execute(Command::EnterInsert(InsertAt::Here), &buf);
        h.engine.key(KeyInput::Function(1), &buf);
        assert!(h.engine.help_visible(), "入力モードからでもヘルプは出る");
    }

    #[test]
    fn space_enters_layout_mode_and_keys_become_mux_requests() {
        let mut h = Harness::new("hello");
        h.keys(" ");
        assert_eq!(h.engine.mode(), Mode::Layout);

        let fx = h.keys("s");
        assert!(
            fx.contains(&Effect::Mux(MuxRequest::Split(SplitDir::Vertical))),
            "s が分割要求にならない: {fx:?}"
        );
        assert_eq!(
            h.engine.mode(),
            Mode::Layout,
            "配置モードは持続する（連続で操作できる）"
        );

        let fx = h.keys("l");
        assert!(fx.contains(&Effect::Mux(MuxRequest::Focus(FocusDir::Right))));
    }

    #[test]
    fn detach_leaves_layout_mode() {
        let mut h = Harness::new("hello");
        h.keys(" ");
        let fx = h.keys("d");
        assert!(fx.contains(&Effect::Mux(MuxRequest::Detach)));
        assert_eq!(h.engine.mode(), Mode::Normal);
    }

    #[test]
    fn unknown_key_leaves_layout_mode() {
        let mut h = Harness::new("hello");
        h.keys(" ");
        h.keys("Z");
        assert_eq!(
            h.engine.mode(),
            Mode::Normal,
            "知らないキーで閉じ込められない"
        );
    }

    #[test]
    fn space_is_a_motion_inside_visual_mode() {
        // 配置モードは Normal からのみ。Visual 中の Space は右移動のまま。
        let mut h = Harness::new("abcdef");
        h.keys("v ");
        assert_eq!(h.engine.mode(), Mode::Visual(VisualKind::Char));
        assert_eq!(h.engine.cursor(), Pos::new(0, 1));
    }

    // ---- マーク・マクロ・貼り付け（`mouse-parity.md` §4.6）----------------

    /// ファイルバッファへキーを流すテスト用のハーネス。
    ///
    /// 端末と違い、`Effect::Edit` / `Effect::Insert` は**ホストが**適用する。
    /// ここではそれを手で当てて、マクロの 2 周目が編集後を見ることまで確かめる。
    struct FileHarness {
        file: tsg_buffer::FileBuffer,
        engine: Engine,
    }

    impl FileHarness {
        fn new(text: &str) -> Self {
            let mut engine = Engine::new();
            engine.set_view(View { top: 0, height: 8 });
            engine.mode = Mode::Normal;
            Self {
                file: tsg_buffer::FileBuffer::from_text(text, AmbiguousWidth::Wide),
                engine,
            }
        }

        fn keys(&mut self, s: &str) -> Vec<Effect> {
            let mut out = Vec::new();
            for c in s.chars() {
                let fx = match self.engine.key(KeyInput::Char(c), &self.file) {
                    KeyOutcome::Handled(fx) => fx,
                    // 入力モードの素通しはホストがファイルへ入れる。
                    // ここで捨てると「打った文字が入らない」テストになる。
                    KeyOutcome::PassThrough => {
                        let at = self.engine.cursor();
                        let end = self.file.insert(at, &c.to_string());
                        self.engine.set_cursor(end, &self.file);
                        Vec::new()
                    }
                };
                self.apply(&fx);
                out.extend(fx);
            }
            out
        }

        /// ホストの代わり。編集を実際にバッファへ当て、カーソルを追わせる。
        fn apply(&mut self, effects: &[Effect]) {
            for fx in effects {
                match fx {
                    Effect::Edit { range, text } => {
                        self.file.replace(range, text);
                        let at = self.file.clamp(range.start);
                        self.engine.set_cursor(at, &self.file);
                    }
                    Effect::Insert { at, text, cursor } => {
                        let end = self.file.insert(*at, text);
                        let next = self.file.clamp(cursor.unwrap_or(end));
                        self.engine.set_cursor(next, &self.file);
                    }
                    Effect::MacroReplay(keys) => {
                        let keys = keys.clone();
                        for k in keys {
                            let fx = match self.engine.key(k, &self.file) {
                                KeyOutcome::Handled(fx) => fx,
                                KeyOutcome::PassThrough => Vec::new(),
                            };
                            self.apply(&fx);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// `A` は行末、`o` は下に 1 行。**ファイルでだけ**効く
    /// （端末では打てるのが生きたプロンプトだけなので、位置を選ぶ意味が無い）。
    #[test]
    fn insert_keys_land_where_vim_puts_them_in_a_file() {
        let mut h = FileHarness::new("alpha\nbravo\n");
        h.keys("A");
        assert_eq!(h.engine.cursor(), Pos::new(0, 5), "A が行末へ行っていない");
        h.keys("-EDIT");
        assert_eq!(h.file.line(0), "alpha-EDIT", "行末に打つと順序が入れ替わる");

        h.engine.mode = Mode::Normal;
        h.engine.set_cursor(Pos::new(0, 0), &h.file);
        h.keys("I");
        assert_eq!(h.engine.cursor(), Pos::new(0, 0));

        h.engine.mode = Mode::Normal;
        h.keys("o");
        assert_eq!(
            h.file.text(),
            "alpha-EDIT\n\nbravo\n",
            "o が下に 1 行開けていない"
        );
        assert_eq!(h.engine.cursor(), Pos::new(1, 0));

        h.engine.mode = Mode::Normal;
        h.engine.set_cursor(Pos::new(0, 0), &h.file);
        h.keys("O");
        assert_eq!(
            h.file.text(),
            "\nalpha-EDIT\n\nbravo\n",
            "O が上に 1 行開けていない"
        );
        assert_eq!(
            h.engine.cursor(),
            Pos::new(0, 0),
            "O の後は開いた行に居るべき"
        );
    }

    #[test]
    fn insert_keys_are_all_the_same_in_a_terminal() {
        // 端末で `A` が行末へ飛ぶと、打った文字が過去の出力の後ろへ入るように見える。
        let mut h = Harness::new("hello world\r\n");
        h.keys("A");
        assert_eq!(h.engine.mode(), Mode::Insert);
        assert_eq!(
            h.engine.cursor(),
            Pos::new(0, 0),
            "端末で位置を動かしている"
        );
    }

    #[test]
    fn a_mark_remembers_where_you_were() {
        let mut h = Harness::new("one\r\ntwo\r\nthree\r\nfour\r\n");
        h.keys("jjma");
        assert_eq!(h.engine.marks.get('a'), Some(Pos::new(2, 0)));
        h.keys("gg");
        assert_eq!(h.engine.cursor().line, 0);
        h.keys("`a");
        assert_eq!(h.engine.cursor().line, 2, "印へ戻れない");
    }

    #[test]
    fn jumping_leaves_a_way_back() {
        let mut h = Harness::new("one\r\ntwo\r\nthree\r\nfour\r\n");
        h.keys("jjjma");
        h.keys("gg");
        h.keys("`a");
        assert_eq!(h.engine.cursor().line, 3);
        // 飛ぶ前の位置が自動で覚えられているので、同じキーで戻れる
        h.keys("``");
        assert_eq!(h.engine.cursor().line, 0, "飛ぶ前へ戻れない");
    }

    #[test]
    fn an_operator_can_take_a_mark_as_its_range() {
        // `d'a` は行単位。`modal-spec.md` の exclusive / linewise の区別が
        // 印にもそのまま効く。
        let mut h = Harness::new("one\r\ntwo\r\nthree\r\nfour\r\n");
        h.keys("jjma");
        h.keys("gg");
        h.keys("d'a");
        assert_eq!(
            h.yanked(),
            "one\ntwo\nthree\n",
            "印までを行ごと取れていない"
        );
    }

    #[test]
    fn a_missing_mark_says_so_instead_of_going_nowhere() {
        let mut h = Harness::new("one\r\ntwo\r\n");
        let fx = h.keys("`z");
        assert!(
            fx.iter()
                .any(|f| matches!(f, Effect::Message(m) if m.contains("ありません"))),
            "黙って何も起きないのが一番困る"
        );
    }

    #[test]
    fn only_the_marks_you_placed_show_up_in_the_gutter() {
        let mut h = Harness::new("one\r\ntwo\r\nthree\r\n");
        h.keys("jma");
        h.keys("gg");
        h.keys("`a");
        // 飛ぶ前の位置は行 0 に自動で置かれるが、ガターには出さない
        assert_eq!(h.engine.marks.at_line(0), None);
        assert_eq!(h.engine.marks.at_line(1), Some('a'));
    }

    #[test]
    fn a_macro_records_keys_and_replays_them() {
        let mut h = FileHarness::new("b\na\nc\n");
        // `qq` で記録、`dd` を録って `q` で終了
        h.keys("qqddq");
        assert_eq!(h.engine.macros.recording(), None, "q で止まっていない");
        assert_eq!(h.engine.macros.get('q').map(<[KeyInput]>::len), Some(2));
        assert_eq!(h.file.text(), "a\nc\n");

        h.keys("@q");
        assert_eq!(h.file.text(), "c\n", "再生が 1 行も消していない");
    }

    #[test]
    fn replaying_sees_the_buffer_as_it_is_now() {
        // マクロ 1 回ごとにバッファを取り直さないと、2 周目が編集前を見る。
        let mut h = FileHarness::new("1\n2\n3\n4\n");
        h.keys("qxddq");
        h.keys("@x");
        h.keys("@@");
        assert_eq!(h.file.text(), "4\n", "@@ が直前のマクロを繰り返していない");
    }

    #[test]
    fn recording_does_not_swallow_the_terminating_key() {
        let mut h = FileHarness::new("hello\n");
        h.keys("qaq");
        assert_eq!(
            h.engine.macros.get('a').map(<[KeyInput]>::len),
            Some(0),
            "終了の q が記録へ混ざっている"
        );
    }

    #[test]
    fn replaying_an_unknown_macro_complains() {
        let mut h = Harness::new("hello");
        let fx = h.keys("@z");
        assert!(fx.iter().any(|f| matches!(f, Effect::Message(_))));
    }

    #[test]
    fn paste_into_a_file_inserts_without_eating_anything() {
        let mut h = FileHarness::new("one\ntwo\n");
        h.keys("yy");
        assert_eq!(
            h.engine.registers.get('"').map(|v| v.kind),
            Some(RangeKind::Line)
        );
        h.keys("p");
        assert_eq!(h.file.text(), "one\none\ntwo\n", "行として下へ貼れていない");
        assert_eq!(h.engine.cursor().line, 1, "貼った行の頭にいない");
    }

    #[test]
    fn paste_before_puts_it_above() {
        let mut h = FileHarness::new("one\ntwo\n");
        h.keys("jyyggP");
        assert_eq!(h.file.text(), "two\none\ntwo\n");
    }

    #[test]
    fn paste_at_the_end_of_the_file_still_lands_on_a_new_line() {
        // 「最終行の次の行」はまだ無いので、差し込み先として指せない。
        let mut h = FileHarness::new("one\ntwo");
        h.keys("yyjp");
        assert_eq!(h.file.text(), "one\ntwo\none");
    }

    #[test]
    fn charwise_paste_lands_after_the_cursor() {
        let mut h = FileHarness::new("abcd\n");
        h.keys("vly");
        assert_eq!(
            h.engine.registers.get('"').map(|v| v.text.clone()),
            Some("ab".into())
        );
        h.keys("p");
        assert_eq!(h.file.text(), "aabbcd\n");
    }

    #[test]
    fn pasting_in_a_terminal_goes_to_the_prompt() {
        // 端末の表示へ文字を差し込んでも次の出力で流れる。プロンプトへ入れるのが等価。
        let mut h = Harness::new("hello world\r\n");
        h.keys("yy");
        let fx = h.keys("p");
        assert!(
            fx.iter()
                .any(|f| matches!(f, Effect::SendToPrompt(t) if t.contains("hello"))),
            "端末での貼り付けがプロンプトへ行っていない"
        );
        assert_eq!(h.engine.mode(), Mode::Insert);
    }

    #[test]
    fn pasting_an_empty_register_says_so() {
        let mut h = FileHarness::new("abc\n");
        let fx = h.keys("p");
        assert!(
            fx.iter()
                .any(|f| matches!(f, Effect::Message(m) if m.contains("空")))
        );
        assert_eq!(h.file.text(), "abc\n", "空なのに何か入った");
    }

    #[test]
    fn mouse_and_keyboard_produce_the_same_command() {
        // mouse-parity.md の合流点。ドラッグ由来の Apply と yw 由来の Apply が
        // 同じディスパッチャを通り、同じ結果になる。
        let mut a = Harness::new("hello world");
        a.keys("yw");

        let mut b = Harness::new("hello world");
        let buf = TermBuffer::new(&b.term.state.grid, &b.term.state.marks);
        b.engine.execute(
            Command::Apply {
                op: OperatorId::Yank,
                range: Range::new(Pos::new(0, 0), Pos::new(0, 5), RangeKind::Char),
                register: None,
            },
            &buf,
        );

        assert_eq!(a.yanked(), b.yanked());
    }
}

#[cfg(test)]
mod space_keys {
    use super::*;

    /// `Space` のあとに続くキーが、一覧に書いてある通りに効くか。
    ///
    /// **一覧に載っているのに効かないキーは、無いのと同じではなく質が悪い。**
    /// 押した人は自分が間違えたと思う。
    #[test]
    fn every_space_key_in_the_list_actually_does_something() {
        let missing: Vec<&str> = crate::command::REGISTRY
            .iter()
            .filter(|c| {
                // **1 つでも効かないキーがあれば挙げる。** 「だいたい効く」は
                // 押した人には効かないのと同じ。
                c.keys
                    .iter()
                    .filter_map(|k| k.strip_prefix("Space ")?.chars().next())
                    .any(|ch| {
                        let mut e = Engine::new();
                        e.mode = Mode::Layout;
                        // 何も起きない = 通常モードへ戻るだけ
                        matches!(e.resolve_layout(ch), Some(Command::EnterNormal))
                    })
            })
            .map(|c| c.id)
            .collect();
        assert!(
            missing.is_empty(),
            "Space のキーが効いていない: {missing:?}"
        );
    }

    /// 同じキーを 2 つのコマンドが名乗っていないか。
    ///
    /// **これが今回の穴の作られ方だった。** `Space l` をペイン移動と
    /// ラベルの両方が名乗っていて、先に読まれるほうしか効かず、
    /// 一覧には両方載っていた。
    #[test]
    fn no_two_commands_claim_the_same_key() {
        let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
        let mut clashes: Vec<String> = Vec::new();
        for c in crate::command::REGISTRY {
            for k in c.keys {
                if let Some(other) = seen.insert(k, c.id) {
                    clashes.push(format!("{k}: {other} と {}", c.id));
                }
            }
        }
        assert!(clashes.is_empty(), "同じキーを名乗っている: {clashes:?}");
    }
}
