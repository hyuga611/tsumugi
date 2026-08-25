//! コマンド語彙とレジストリ。
//!
//! `arch.md` の不変条件 1「単一コマンドバス」の実体。
//! キーボード・マウス・右クリックメニュー・コマンドパレット・RPC は
//! **すべてここの `Command` を発行するだけ**で、状態を直接触らない。
//!
//! レジストリは `mouse-parity.md` §2.2 のパリティテストの入力でもあり、
//! 右クリックメニューとコマンドパレットの生成元でもある。定義を二重に持たない。

use tsg_buffer::{OperatorId, Pos, Range, RangeKind};

use crate::motion::Motion;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisualKind {
    Char,
    Line,
    Block,
}

impl VisualKind {
    pub fn range_kind(self) -> RangeKind {
        match self {
            VisualKind::Char => RangeKind::Char,
            VisualKind::Line => RangeKind::Line,
            VisualKind::Block => RangeKind::Block,
        }
    }
}

/// `i` `a` `I` `A` `o` `O` の行き先。
///
/// 端末では**どれも同じ**（打てるのは生きているプロンプトだけなので、
/// 位置を選ぶ意味が無い）。ファイルバッファでだけ vim と同じに振る舞う。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertAt {
    /// `i` そのまま
    Here,
    /// `a` 1 つ右
    After,
    /// `I` 行の最初の非空白
    LineStart,
    /// `A` 行末
    LineEnd,
    /// `o` 下に 1 行開ける
    Below,
    /// `O` 上に 1 行開ける
    Above,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Insert,
    Normal,
    Visual(VisualKind),
    OperatorPending,
    /// ペイン・タブ・セッションの操作に専念する持続モード（`modal-spec.md` §9）。
    /// prefix キー方式にしないのは、連続操作で毎回 prefix を打つ苦痛を消すため。
    Layout,
}

impl Mode {
    /// ステータス行に出す短い名前。飾りの `--` は付けない（色帯で見せる）。
    pub fn label(self) -> &'static str {
        match self {
            Mode::Insert => crate::t!("入力", "INSERT"),
            Mode::Normal => crate::t!("通常", "NORMAL"),
            Mode::Visual(VisualKind::Char) => crate::t!("選択", "SELECT"),
            Mode::Visual(VisualKind::Line) => crate::t!("選択・行", "SELECT LINE"),
            Mode::Visual(VisualKind::Block) => crate::t!("選択・矩形", "SELECT BLOCK"),
            Mode::OperatorPending => crate::t!("待機", "PENDING"),
            Mode::Layout => crate::t!("配置", "LAYOUT"),
        }
    }

    /// 今このモードで何ができるかを、記号ではなく言葉で。
    ///
    /// **キーバインドを覚えていない人が読む唯一の場所**なので、
    /// キー名の羅列ではなく「何が起きるか」を書く。
    pub fn hint(self) -> &'static str {
        match self {
            Mode::Insert => crate::t!(
                "そのまま打てます   Esc で読むモードへ",
                "type as usual   Esc to read"
            ),
            Mode::Normal => crate::t!(
                "読むモード   クリックで選ぶ / ダブルクリックで語・パス / i で入力へ",
                "reading   click to select / double-click a word or path / i to type"
            ),
            Mode::Visual(_) => crate::t!(
                "選んでいます   y でコピー / 右クリックでできること / Esc でやめる",
                "selecting   y to copy / right-click for actions / Esc to cancel"
            ),
            Mode::OperatorPending => crate::t!(
                "続きのキー待ち   Esc でやめる",
                "waiting for the rest   Esc to cancel"
            ),
            Mode::Layout => crate::t!(
                "画面の配置   s / v で分割 · hjkl で移動 · z で最大化 · Esc で戻る",
                "layout   s / v split · hjkl move · z zoom · Esc to leave"
            ),
        }
    }

    /// 🔴 `arch.md` §6.2。Normal 系ではキーがコマンドなので IME を切る。
    pub fn ime_allowed(self) -> bool {
        matches!(self, Mode::Insert)
    }
}

/// 分割の向き。`tsg-mux` の `Dir` と同型だが、依存の向きを守るため独立して持つ
/// （`arch.md`: `tsg-modal` は I/O 系クレートに依存しない）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitDir {
    /// 左右に割る
    Horizontal,
    /// 上下に割る
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusDir {
    Left,
    Right,
    Up,
    Down,
}

/// mux（別プロセス）へ投げる要求。ホストが `tsg-mux` のメッセージへ翻訳する。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MuxRequest {
    Split(SplitDir),
    ClosePane,
    Focus(FocusDir),
    /// 隣のペインと中身を入れ替える（`Space HJKL`）。
    Swap(FocusDir),
    /// 1 枚だけを画面いっぱいに / 戻す（`Space z`）。
    Zoom,
    /// 分割比をそろえる（`Space =`）。
    Equalize,
    /// 取り分を隣から奪う / 返す（`Space <>+-`）。
    Resize(i32),
    NewTab,
    NextTab,
    PrevTab,
    /// タブを前後へ動かす（`Space <` / `Space >`）。
    MoveTab(isize),
    /// 走っているセッションの一覧を出す（`Space S`）。
    Sessions,
    /// セッションから抜ける。プロセスは生き続ける。
    Detach,
    /// セッションごと終了する。中のシェルも死ぬ。
    Shutdown,
    /// Markdown を読む形にする / 素に戻す（`Space m`）。
    TogglePreview,
    /// この画面に出てきたファイルの一覧（`Space f`）。
    ///
    /// エージェントは触ったファイルを字で言う。**その字はもう画面に在る**ので、
    /// 集めて並べるだけで「何を触られたか」の一覧になる。
    PaneFiles,
    /// 次の「人の番」のエージェントへ飛ぶ（`Space a`）。
    ///
    /// AI エージェントを何本も並べて放っておくと、**どれが止まって返事を
    /// 待っているのかを目で探す時間**が仕事の大半になる。それを消す。
    NextAgent,
}

/// ファイルバッファへの操作。実体を持つのはホスト（`tsg`）だけ。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileAction {
    Save,
    /// 端末へ戻る。未保存なら断る。
    Close,
    /// 未保存でも捨てて戻る（`:q!`）。
    CloseDiscard,
}

/// 取り消し・やり直し。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryAction {
    Undo,
    Redo,
}

/// 実行できる操作の語彙。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    EnterLayout,
    Mux(MuxRequest),

    EnterInsert(InsertAt),
    EnterNormal,
    EnterVisual(VisualKind),

    /// カーソル移動。
    Move { motion: Motion, count: usize },

    /// 範囲が確定したオペレータの適用。
    ///
    /// キーボードは `operator + motion` から、マウスは選択範囲から、
    /// どちらもこの形に合流する。これがマウス等価の合流点。
    Apply {
        op: OperatorId,
        range: Range,
        register: Option<char>,
    },

    /// カーソルを直接置く（マウスのクリック）。
    SetCursor(Pos),

    /// 範囲を選択して Visual に入る。
    ///
    /// テキストオブジェクト（`vif`）とマウス（ドラッグ・ダブルクリック）が
    /// 合流する点。`Apply` が「範囲＋動詞」の合流点なのに対し、
    /// こちらは「範囲だけ」の合流点になる。
    Select { range: Range },

    /// 表示だけを動かす（ホイール）。
    Scroll(isize),

    /// コマンドパレットを開く。`prefix` を入れた状態から始める。
    Palette(&'static str),

    /// ファイルバッファへの操作。
    File(FileAction),

    /// 取り消し / やり直し。
    History(HistoryAction),

    /// `ma` — 今いる場所に印を置く。
    SetMark(char),

    /// `` `a `` / `'a` — 印へ飛ぶ。`exact` が偽なら行頭まで（`'`）。
    JumpMark { name: char, exact: bool },

    /// `p` / `P` — レジスタの中身を貼る。
    Paste { before: bool },

    /// `q{a}` で記録開始、`None` で終了。
    MacroRecord(Option<char>),

    /// `@{a}` — 記録したキー列を流し直す。
    MacroReplay(char),

    /// 使い方の表示。起動しただけでは何をすればいいか分からない、を潰すための一級機能。
    ToggleHelp,

    /// 配色を変える。**名前はこの層にとって不透明**で、実際に色を知っているのは
    /// ホストだけ（`arch.md` の不変条件 2「`tsg-modal` は純粋」）。
    SetTheme(&'static str),

    /// 設定ファイルを開く。**場所を知っているのはホストだけ**なので、
    /// ここは「開け」としか言わない。
    OpenConfig,

    Quit,
}

// ---------------------------------------------------------------------------
// レジストリ
// ---------------------------------------------------------------------------

/// そのコマンドへマウスから到達する経路。`mouse-parity.md` §2.2。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MousePath {
    /// 専用のジェスチャがある
    Direct(&'static str),
    /// 右クリックメニューの該当セクションに出る
    Menu(&'static str),
    /// コマンドパレット経由（2アクション以内の最終保証）
    Palette,
    /// マウス経路を持たない。**理由の宣言を必須にする。**
    KeyboardOnly(&'static str),
}

pub struct CommandSpec {
    pub id: &'static str,
    /// 日本語の題名。**英語と同じ場所に置く**ので、片方だけ直す事故が起きにくい。
    pub title: &'static str,
    pub title_en: &'static str,
    /// 既定のキーバインド。空なら palette からのみ。
    pub keys: &'static [&'static str],
    pub mouse: MousePath,
    pub in_palette: bool,
}

impl MousePath {
    /// 人に見せる説明。
    pub fn describe(&self) -> &'static str {
        match self {
            MousePath::Direct(s) | MousePath::Menu(s) => s,
            MousePath::Palette => "コマンドパレットから",
            MousePath::KeyboardOnly(_) => "—",
        }
    }
}

impl CommandSpec {
    /// 今の言語での題名。
    pub fn label(&self) -> &'static str {
        crate::t!(self.title, self.title_en)
    }

    pub fn mouse_reachable(&self) -> bool {
        !matches!(self.mouse, MousePath::KeyboardOnly(_))
    }

    pub fn keyboard_reachable(&self) -> bool {
        !self.keys.is_empty() || self.in_palette
    }
}

/// 現時点で存在するコマンド。増やすたびにマウス経路の宣言が強制される。
///
/// ⚠️ `mouse` の宣言は**設計上の約束**であり、実配線とは別物。
/// 配線済み: クリック / ダブル・トリプルクリック / ドラッグ / Alt＋ドラッグ /
/// ホイール / タブバー / ペイン境界のドラッグ / ステータス行。
/// **未配線: 左ガター・右クリックメニュー・コマンドパレット。**
/// これらを宣言している行は、まだキーボードからしか届かない。
pub const REGISTRY: &[CommandSpec] = &[
    CommandSpec {
        id: "ui.config",
        title: "設定ファイルを開く",
        title_en: "Open the config file",
        keys: &[],
        mouse: MousePath::Palette,
        in_palette: true,
    },
    CommandSpec {
        id: "ui.theme.yogiri",
        title: "配色: 夜霧（暗い / 既定）",
        title_en: "Theme: Yogiri (dark, default)",
        keys: &[],
        mouse: MousePath::Palette,
        in_palette: true,
    },
    CommandSpec {
        id: "ui.theme.sumi",
        title: "配色: 墨（暗い / 高コントラスト）",
        title_en: "Theme: Sumi (dark, high contrast)",
        keys: &[],
        mouse: MousePath::Palette,
        in_palette: true,
    },
    CommandSpec {
        id: "ui.theme.hakuji",
        title: "配色: 白磁（明るい）",
        title_en: "Theme: Hakuji (light)",
        keys: &[],
        mouse: MousePath::Palette,
        in_palette: true,
    },
    CommandSpec {
        id: "ui.help",
        title: "使い方を表示",
        title_en: "Show help",
        keys: &["F1"],
        mouse: MousePath::Direct("ステータス行の [F1 ヘルプ] をクリック"),
        in_palette: true,
    },
    CommandSpec {
        id: "mode.insert",
        title: "入力モードへ",
        title_en: "Type (insert mode)",
        keys: &["i", "a", "I", "A", "o", "O"],
        mouse: MousePath::Direct("末尾のプロンプト行をクリック / ステータス行の入力バッジ"),
        in_palette: true,
    },
    CommandSpec {
        id: "mode.normal",
        title: "通常モードへ",
        title_en: "Read (normal mode)",
        keys: &["Esc", "C-\\"],
        mouse: MousePath::Direct("スクロールバック上をクリック / 上方向へホイール"),
        in_palette: true,
    },
    CommandSpec {
        id: "mode.visual.char",
        title: "選択（文字）",
        title_en: "Select by character",
        keys: &["v"],
        mouse: MousePath::Direct("左ドラッグ"),
        in_palette: true,
    },
    CommandSpec {
        id: "mode.visual.line",
        title: "選択（行）",
        title_en: "Select by line",
        keys: &["V"],
        mouse: MousePath::Direct("トリプルクリック"),
        in_palette: true,
    },
    CommandSpec {
        id: "mode.visual.block",
        title: "選択（矩形）",
        title_en: "Select a block",
        keys: &["C-v"],
        mouse: MousePath::Direct("Alt＋ドラッグ"),
        in_palette: true,
    },
    CommandSpec {
        id: "motion.basic",
        title: "カーソル移動",
        title_en: "Move the cursor",
        keys: &["h", "j", "k", "l", "w", "b", "e", "0", "^", "$", "gg", "G", "{", "}"],
        mouse: MousePath::Direct("クリック"),
        in_palette: false,
    },
    CommandSpec {
        id: "motion.find",
        title: "行内文字検索",
        title_en: "Find a character in the line",
        keys: &["f", "F", "t", "T"],
        mouse: MousePath::KeyboardOnly(
            "移動先を直接クリックすれば済む。純粋な速記であり機能の追加ではない",
        ),
        in_palette: false,
    },
    CommandSpec {
        id: "motion.find.repeat",
        title: "行内検索の繰り返し",
        title_en: "Repeat the last find",
        keys: &[";", ","],
        mouse: MousePath::KeyboardOnly("motion.find と同じ理由"),
        in_palette: false,
    },
    CommandSpec {
        id: "motion.screen",
        title: "画面内の上/中/下へ",
        title_en: "Top / middle / bottom of the screen",
        keys: &["H", "M", "L"],
        mouse: MousePath::Direct("クリック"),
        in_palette: false,
    },
    CommandSpec {
        id: "motion.page",
        title: "ページ送り",
        title_en: "Page up / down",
        keys: &["C-d", "C-u", "C-f", "C-b"],
        mouse: MousePath::Direct("ホイール / スクロールバー"),
        in_palette: true,
    },
    CommandSpec {
        id: "motion.prompt",
        title: "前 / 次のプロンプトへ",
        title_en: "Previous / next prompt",
        keys: &["[[", "]]"],
        mouse: MousePath::Direct("左ガターのプロンプトマーカーをクリック"),
        in_palette: true,
    },
    CommandSpec {
        id: "motion.error",
        title: "前 / 次のエラーへ",
        title_en: "Previous / next failed command",
        keys: &["[e", "]e"],
        mouse: MousePath::Direct("左ガターの赤マーカーをクリック"),
        in_palette: true,
    },
    CommandSpec {
        id: "motion.agent",
        title: "前 / 次のエージェントの発話へ",
        title_en: "Previous / next agent message",
        keys: &["[a", "]a"],
        mouse: MousePath::Palette,
        in_palette: true,
    },
    CommandSpec {
        id: "motion.match",
        title: "対応括弧へ",
        title_en: "Jump to the matching bracket",
        keys: &["%"],
        mouse: MousePath::Direct("括弧の上をダブルクリック"),
        in_palette: true,
    },
    CommandSpec {
        id: "textobj.generic",
        title: "テキストオブジェクト（単語・引用符・括弧・段落・文）",
        title_en: "Text objects (word, quotes, brackets, paragraph, sentence)",
        keys: &["iw", "aw", "i\"", "i(", "ip", "is"],
        mouse: MousePath::Direct("ダブルクリック / 引用符・括弧の上をダブルクリック"),
        in_palette: true,
    },
    CommandSpec {
        id: "textobj.command",
        title: "コマンドブロック（ic / ac）",
        title_en: "Command block (ic / ac)",
        keys: &["ic", "ac"],
        mouse: MousePath::Direct("左ガターのプロンプトマーカーをクリック"),
        in_palette: true,
    },
    CommandSpec {
        id: "textobj.output",
        title: "出力ブロック（io / ao）",
        title_en: "Output block (io / ao)",
        keys: &["io", "ao"],
        mouse: MousePath::Direct("左ガターのプロンプトマーカーをダブルクリック"),
        in_palette: true,
    },
    CommandSpec {
        id: "textobj.path",
        title: "ファイルパス（if / af）",
        title_en: "File path (if / af)",
        keys: &["if", "af"],
        mouse: MousePath::Direct("パスの上をダブルクリック"),
        in_palette: true,
    },
    CommandSpec {
        id: "textobj.url",
        title: "URL（iu / au）",
        title_en: "URL (iu / au)",
        keys: &["iu", "au"],
        mouse: MousePath::Direct("URL の上をダブルクリック"),
        in_palette: true,
    },
    CommandSpec {
        id: "textobj.hash",
        title: "ハッシュ・ID（ih）",
        title_en: "Hash or id (ih)",
        keys: &["ih"],
        mouse: MousePath::Direct("SHA の上をダブルクリック"),
        in_palette: true,
    },
    CommandSpec {
        id: "textobj.number",
        title: "数値（in / an）",
        title_en: "Number (in / an)",
        keys: &["in", "an"],
        mouse: MousePath::Direct("数値の上をダブルクリック"),
        in_palette: true,
    },
    CommandSpec {
        id: "textobj.error",
        title: "エラーブロック（ie / ae）",
        title_en: "Error block (ie / ae)",
        keys: &["ie", "ae"],
        mouse: MousePath::Direct("左ガターの赤マーカーをクリック"),
        in_palette: true,
    },
    CommandSpec {
        id: "op.send_to_prompt",
        title: "プロンプトへ送る",
        title_en: "Send to the prompt",
        keys: &["!"],
        mouse: MousePath::Menu("編集"),
        in_palette: true,
    },
    CommandSpec {
        id: "op.yank",
        title: "ヤンク（コピー）",
        title_en: "Copy (yank)",
        keys: &["y", "yy", "Y"],
        mouse: MousePath::Menu("編集"),
        in_palette: true,
    },
    CommandSpec {
        id: "register.select",
        title: "レジスタを指定",
        title_en: "Choose a register",
        keys: &["\""],
        mouse: MousePath::Menu("編集"),
        in_palette: true,
    },
    CommandSpec {
        id: "layout.mode",
        title: "配置モードへ",
        title_en: "Layout mode",
        keys: &["Space", "C-w"],
        mouse: MousePath::Menu("配置"),
        in_palette: true,
    },
    CommandSpec {
        id: "layout.split",
        title: "ペインを分割",
        title_en: "Split the pane",
        keys: &["Space s", "Space v"],
        mouse: MousePath::Menu("配置"),
        in_palette: true,
    },
    CommandSpec {
        id: "layout.focus",
        title: "ペインを移動",
        title_en: "Move between panes",
        keys: &["Space h", "Space j", "Space k", "Space l"],
        mouse: MousePath::Direct("ペインをクリック"),
        in_palette: false,
    },
    CommandSpec {
        id: "layout.close",
        title: "ペインを閉じる",
        title_en: "Close the pane",
        keys: &["Space x"],
        mouse: MousePath::Menu("配置"),
        in_palette: true,
    },
    CommandSpec {
        id: "layout.tab",
        title: "タブ（新規・前後）",
        title_en: "Tabs (new, previous, next)",
        keys: &["Space c", "Space n", "Space p"],
        mouse: MousePath::Direct("タブバーをクリック / + ボタン"),
        in_palette: true,
    },
    CommandSpec {
        id: "layout.swap",
        title: "ペインを入れ替える",
        title_en: "Swap panes",
        keys: &["Space H", "Space J", "Space K", "Space L"],
        mouse: MousePath::Menu("配置"),
        in_palette: true,
    },
    CommandSpec {
        id: "layout.zoom",
        title: "ペインを画面いっぱいに（もう一度で戻す）",
        title_en: "Zoom the pane (again to restore)",
        keys: &["Space z"],
        mouse: MousePath::Menu("配置"),
        in_palette: true,
    },
    CommandSpec {
        id: "layout.equalize",
        title: "分割比をそろえる",
        title_en: "Even out the splits",
        keys: &["Space ="],
        mouse: MousePath::Direct("ペイン境界をダブルクリック"),
        in_palette: true,
    },
    CommandSpec {
        id: "layout.resize",
        title: "分割比を変える",
        title_en: "Resize the split",
        keys: &["Space <", "Space >", "Space +", "Space -"],
        mouse: MousePath::Direct("ペイン境界をドラッグ"),
        in_palette: true,
    },
    CommandSpec {
        id: "layout.tabmove",
        title: "タブを前後へ動かす",
        title_en: "Move the tab left / right",
        keys: &["Space [", "Space ]"],
        mouse: MousePath::Direct("タブをドラッグ"),
        in_palette: true,
    },
    CommandSpec {
        id: "layout.sessions",
        title: "セッションの一覧（切り替え）",
        title_en: "Sessions (switch)",
        keys: &["Space S"],
        mouse: MousePath::Menu("セッション"),
        in_palette: true,
    },
    CommandSpec {
        id: "file.preview",
        title: "Markdown を読む形にする（もう一度で戻す）",
        title_en: "Render Markdown (press again to go back)",
        keys: &["Space m"],
        mouse: MousePath::Menu("ファイル"),
        in_palette: true,
    },
    CommandSpec {
        id: "agent.files",
        title: "この画面に出てきたファイルの一覧",
        title_en: "Files mentioned on this screen",
        keys: &["Space f"],
        mouse: MousePath::Menu("ファイル"),
        in_palette: true,
    },
    CommandSpec {
        id: "agent.next",
        title: "返事を待っているエージェントへ飛ぶ",
        title_en: "Jump to the agent waiting for you",
        keys: &["Space a"],
        mouse: MousePath::Menu("セッション"),
        in_palette: true,
    },
    CommandSpec {
        id: "layout.detach",
        title: "デタッチ（プロセスは生かしたまま抜ける）",
        title_en: "Detach (leave the shells running)",
        keys: &["Space d"],
        mouse: MousePath::Menu("セッション"),
        in_palette: true,
    },
    CommandSpec {
        id: "layout.shutdown",
        title: "セッションごと終了（中のシェルも終わる）",
        title_en: "End the session (kills the shells)",
        keys: &["Space Q"],
        mouse: MousePath::Menu("セッション"),
        in_palette: true,
    },
    CommandSpec {
        id: "edit.undo",
        title: "取り消す",
        title_en: "Undo",
        keys: &["u"],
        mouse: MousePath::Menu("編集"),
        in_palette: true,
    },
    CommandSpec {
        id: "edit.redo",
        title: "やり直す",
        title_en: "Redo",
        keys: &["C-r"],
        mouse: MousePath::Menu("編集"),
        in_palette: true,
    },
    CommandSpec {
        id: "file.open",
        title: "ファイルを開く（このペインをエディタにする）",
        title_en: "Open a file (turns this pane into an editor)",
        keys: &["Space e", ":e"],
        mouse: MousePath::Menu("ファイル"),
        in_palette: true,
    },
    CommandSpec {
        id: "file.save",
        title: "保存",
        title_en: "Save",
        keys: &[":w"],
        mouse: MousePath::Menu("ファイル"),
        in_palette: true,
    },
    CommandSpec {
        id: "file.close",
        title: "端末へ戻る（エディタを閉じる）",
        title_en: "Back to the terminal (close the editor)",
        keys: &[":q"],
        mouse: MousePath::Menu("ファイル"),
        in_palette: true,
    },
    CommandSpec {
        id: "op.delete",
        title: "削除（端末では表示から消すだけ）",
        title_en: "Delete (in a terminal, only from the view)",
        keys: &["d", "dd"],
        mouse: MousePath::Menu("編集"),
        in_palette: true,
    },
    CommandSpec {
        id: "op.change",
        title: "変更（消して入力モードへ・File のみ）",
        title_en: "Change (delete then type; files only)",
        keys: &["c", "cc"],
        mouse: MousePath::Menu("編集"),
        in_palette: true,
    },
    CommandSpec {
        id: "op.pipe",
        title: "外部コマンドへ通す（結果を新しいペインで開く）",
        title_en: "Pipe through a command (result opens in a new pane)",
        keys: &[">"],
        mouse: MousePath::Menu("編集"),
        in_palette: true,
    },
    CommandSpec {
        id: "op.format",
        title: "整形（JSON・表の桁揃え）",
        title_en: "Format (JSON, align columns)",
        keys: &["=", "=="],
        mouse: MousePath::Menu("編集"),
        in_palette: true,
    },
    CommandSpec {
        id: "mark.set",
        title: "マークを置く",
        title_en: "Set a mark",
        keys: &["m{a}"],
        mouse: MousePath::Menu("編集"),
        in_palette: true,
    },
    CommandSpec {
        id: "mark.jump",
        title: "マークへ飛ぶ",
        title_en: "Jump to a mark",
        keys: &["`{a}", "'{a}"],
        mouse: MousePath::Direct("左ガターのマーク印をクリック"),
        in_palette: true,
    },
    CommandSpec {
        id: "macro.record",
        title: "マクロを記録 / 終了",
        title_en: "Record a macro / stop",
        keys: &["q{a}", "q"],
        mouse: MousePath::Direct("ステータス行の記録ボタンをクリック"),
        in_palette: true,
    },
    CommandSpec {
        id: "macro.replay",
        title: "マクロを再生",
        title_en: "Play the macro",
        keys: &["@{a}", "@@"],
        mouse: MousePath::Direct("ステータス行の再生ボタンをクリック"),
        in_palette: true,
    },
    CommandSpec {
        id: "edit.paste",
        title: "貼り付け（端末ではプロンプトへ入る）",
        title_en: "Paste (in a terminal it goes to the prompt)",
        keys: &["p", "P"],
        mouse: MousePath::Menu("編集"),
        in_palette: true,
    },
    CommandSpec {
        id: "register.list",
        title: "レジスタの中身を見る",
        title_en: "Show the registers",
        keys: &[":reg"],
        mouse: MousePath::Menu("編集"),
        in_palette: true,
    },
    CommandSpec {
        id: "app.quit",
        title: "終了",
        title_en: "Quit",
        keys: &["ZZ"],
        mouse: MousePath::Direct("ウィンドウの×"),
        in_palette: true,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// `mouse-parity.md` §6 が許すキーボード専用の除外。
    ///
    /// ここに載っているのは「移動先をクリックすれば済む純粋な速記」だけ。
    /// 新しいコマンドをキーボード専用にするには**この配列を編集する必要があり**、
    /// それは PR の差分に必ず現れる。黙って等価が壊れることを防ぐのが目的。
    const ALLOWED_KEYBOARD_ONLY: &[&str] = &["motion.find", "motion.find.repeat"];

    /// `mouse-parity.md` §2.2。新しいコマンドを足した人は、
    /// **マウス経路を書くか、除外リストに載せて理由を宣言するかを強制される。**
    #[test]
    fn every_command_is_mouse_reachable() {
        for spec in REGISTRY {
            if spec.mouse_reachable() {
                continue;
            }
            assert!(
                ALLOWED_KEYBOARD_ONLY.contains(&spec.id),
                "{} にマウス経路がありません。mouse-parity.md §6 の除外リストにも無いので、\n\
                 マウス経路を足すか、除外の妥当性をレビューしてリストに追加してください",
                spec.id
            );
        }
    }

    /// 除外リストが実体と食い違って腐るのを防ぐ。
    #[test]
    fn exclusion_list_has_no_stale_entries() {
        for id in ALLOWED_KEYBOARD_ONLY {
            let spec = REGISTRY
                .iter()
                .find(|s| s.id == *id)
                .unwrap_or_else(|| panic!("除外リストの {id} がレジストリに存在しません"));
            assert!(
                !spec.mouse_reachable(),
                "{id} はマウス経路を得たので、除外リストから外してください"
            );
        }
    }

    #[test]
    fn every_command_is_keyboard_reachable() {
        for spec in REGISTRY {
            assert!(
                spec.keyboard_reachable(),
                "{} はキーボードから到達できません",
                spec.id
            );
        }
    }

    /// 除外は「速記にすぎない」型だけを許す。理由が空文字の逃げを塞ぐ。
    #[test]
    fn keyboard_only_exclusions_state_a_reason() {
        for spec in REGISTRY {
            if let MousePath::KeyboardOnly(reason) = spec.mouse {
                assert!(
                    reason.len() > 10,
                    "{} の除外理由が実質空です",
                    spec.id
                );
            }
        }
    }

    /// 片方の言語だけ足して、もう片方が空のまま出るのを防ぐ。
    #[test]
    fn every_command_has_both_languages() {
        for spec in REGISTRY {
            assert!(!spec.title.is_empty(), "{} に日本語の題名が無い", spec.id);
            assert!(!spec.title_en.is_empty(), "{} に英語の題名が無い", spec.id);
            assert!(
                spec.title_en.is_ascii(),
                "{} の英語の題名に非 ASCII が混ざっている",
                spec.id
            );
        }
    }

    #[test]
    fn ids_are_unique() {
        let mut ids: Vec<&str> = REGISTRY.iter().map(|s| s.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "コマンド ID が重複しています");
    }

    #[test]
    fn ime_is_disabled_outside_insert() {
        assert!(Mode::Insert.ime_allowed());
        assert!(!Mode::Normal.ime_allowed());
        assert!(!Mode::Visual(VisualKind::Char).ime_allowed());
        assert!(!Mode::OperatorPending.ime_allowed());
        assert!(!Mode::Layout.ime_allowed());
    }
}
