//! エスケープシーケンス解析とグリッドの意味論。
//!
//! 解析の状態機械は `vte`（Alacritty 由来）に任せ、セルの意味づけと
//! ドキュメント化（スクロールバック連結・セマンティックマーク）はこちらが持つ。
//! `arch.md` §3 の依存選定の通り、`termwiz` を採らないのはセルモデルまで
//! 規定されると設計自由度が消えるため。

pub mod attrs;
pub mod graphics;
pub mod grid;
pub mod semantic;
pub mod sixel;

use base64::prelude::{BASE64_STANDARD, Engine as _};
use vte::{Params, Perform};

pub use attrs::{Attrs, Color};
pub use grid::{
    AmbiguousWidth, Cell, Cursor, Grid, Line, ambiguous, char_width, set_ambiguous, width_of,
};
pub use semantic::{CommandBlock, Mark, MarkKind, SemanticMarks};

/// マウストラッキングの段階。数字は DEC プライベートモード番号。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseTracking {
    #[default]
    Off,
    /// 1000: 押下と解放
    Normal,
    /// 1002: ボタンを押している間の移動も
    ButtonEvent,
    /// 1003: すべての移動
    AnyEvent,
    /// 9: X10 互換（押下のみ）
    X10,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseEncoding {
    #[default]
    Default,
    /// 1005
    Utf8,
    /// 1006
    Sgr,
    /// 1015
    Urxvt,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Modes {
    pub mouse: MouseTracking,
    pub mouse_encoding: MouseEncoding,
    pub bracketed_paste: bool,
    pub app_cursor_keys: bool,
    pub focus_events: bool,
}

/// 入力の所有者。`concept.md` の所有権モデルの実体。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputOwner {
    /// tsumugi 自身（モーダル層が処理する）
    Tsumugi,
    /// 子プロセスへ素通し
    Child,
}

/// `:pin` による所有権の手動固定。
#[derive(Clone, Copy, Debug, Default)]
pub struct Pins {
    pub mouse: Option<InputOwner>,
    pub key: Option<InputOwner>,
}

/// 1ペイン分の端末状態。`vte::Perform` の実装体でもある。
pub struct TermState {
    pub grid: Grid,
    pub marks: SemanticMarks,
    pub modes: Modes,
    pub pins: Pins,
    pub title: String,
    pub cwd: Option<String>,

    /// 受け取った OSC を生のまま記録する（M0 のプローブ用）。
    pub osc_log: Vec<String>,
    pub log_osc: bool,

    /// 端末に出ている絵。**ドキュメント絶対行で置く**ので、
    /// スクロールバックと一緒に動く（`concept.md` の中心命題のまま）。
    pub images: Vec<graphics::Image>,
    /// 組み立て中の絵（分割して届く）。
    pending_image: Option<graphics::Pending>,
    /// 読み取り中の Sixel（DCS `q`）。古い道具はこちらで絵を出す。
    sixel: Option<sixel::Decoder>,
    next_image: u64,

    /// **端末から相手へ返す答え。** ホストが取り出して PTY へ書く。
    ///
    /// これが無いと、相手は「この端末は何ができるのか」を訊いても答えが
    /// 返らない。powerlevel10k はカーソル位置（CPR）を、vim は端末の型
    /// （DA1）を、nvim は背景色（OSC 11）を訊く。無応答だと待たされるか、
    /// 誤った既定に倒れる。**訊かれたら答える**のは端末の義務。
    pub replies: Vec<u8>,
    /// 前景・背景の実際の色（OSC 10 / 11 の答え）。ホストが配色から入れる。
    pub fg_rgb: (u8, u8, u8),
    pub bg_rgb: (u8, u8, u8),
    /// カーソルの形（DECSCUSR `CSI Ps SP q`）。0 = 既定。
    pub cursor_style: u16,
    /// OSC 8 のリンク先。セルは番号だけを持ち、実体はここに 1 つ。
    ///
    /// `eza` `ls --hyperlink` `cargo` `bat` が既に出している。画面の字から
    /// パスらしきものを当てにいくのと違い、**相手が明示した行き先**なので確か。
    pub links: Vec<String>,
    /// OSC 52 で相手が置いたクリップボードの中身。ホストが取り出す。
    ///
    /// **読み出し（`?`）には答えない。** 答えると、画面に字を出しただけで
    /// クリップボードの中身を抜けてしまう。書き込みだけ受ける。
    pub clipboard: Option<String>,
    /// `ESC ( 0` の罫線文字セットが G0 に入っているか。
    line_drawing: bool,
}

/// DEC Special Graphics（`ESC ( 0`）。罫線を使う TUI がこれで枠を描く。
///
/// **無視すると `lqqqk` のような文字化けになる。** 変換表は小さいので持つ。
fn dec_special(c: char) -> Option<char> {
    Some(match c {
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'q' => '─',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'a' => '▒',
        '`' => '◆',
        'f' => '°',
        'g' => '±',
        'o' => '⎺',
        'p' => '⎻',
        'r' => '⎼',
        's' => '⎽',
        '~' => '·',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        '0' => '█',
        _ => return None,
    })
}

impl TermState {
    pub fn new(cols: usize, rows: usize, amb: AmbiguousWidth) -> Self {
        Self {
            grid: Grid::new(cols, rows, amb),
            marks: SemanticMarks::default(),
            modes: Modes::default(),
            pins: Pins::default(),
            title: String::new(),
            cwd: None,
            osc_log: Vec::new(),
            log_osc: false,
            images: Vec::new(),
            pending_image: None,
            sixel: None,
            next_image: 1,
            replies: Vec::new(),
            fg_rgb: (0xd8, 0xde, 0xe9),
            bg_rgb: (0x11, 0x13, 0x1a),
            cursor_style: 0,
            links: Vec::new(),
            clipboard: None,
            line_drawing: false,
        }
    }

    /// 相手へ返す。**溜めすぎない** — 読み手が居なければ捨てる。
    fn reply(&mut self, bytes: &[u8]) {
        if self.replies.len() + bytes.len() > 4096 {
            return;
        }
        self.replies.extend_from_slice(bytes);
    }

    /// そのセルのリンク先（OSC 8）。無ければ `None`。
    pub fn link_of(&self, cell: &Cell) -> Option<&str> {
        (cell.link > 0)
            .then(|| self.links.get(cell.link as usize - 1))
            .flatten()
            .map(String::as_str)
    }

    /// APC の中身を 1 つ受け取る。そろったらカーソルの位置に置く。
    ///
    /// **絵のぶんだけ行を送る。** 送らないと、次に出る字が絵の上に重なる。
    /// グリッドは相変わらず文書で、絵はその上に乗るだけ。
    pub fn take_graphics(&mut self, body: &[u8]) {
        let Some(rest) = body.strip_prefix(b"G") else {
            return; // `_G` 以外の APC は知らない
        };
        let Some((rgba, w, h, want_cols, want_rows)) =
            graphics::feed_apc(&mut self.pending_image, rest)
        else {
            return;
        };
        self.place_image(rgba, w, h, want_cols, want_rows);
    }

    /// 受け取った絵をカーソルの位置に置き、そのぶん行を送る。
    ///
    /// **絵のぶんだけ行を送る。** 送らないと、次に出る字が絵の上に重なる。
    /// グリッドは相変わらず文書で、絵はその上に乗るだけ。
    ///
    /// **相手の言うセル数を鵜呑みにしない。** `r=4000000000` と書かれた
    /// 数バイトで、行送りを 40 億回まわしてセッションを止められる。
    fn place_image(&mut self, rgba: Vec<u8>, w: u32, h: u32, want_cols: u32, want_rows: u32) {
        let max_cols = self.grid.cols.max(1);
        let max_rows = self.grid.rows.max(1);
        let cols = if want_cols > 0 {
            (want_cols as usize).min(max_cols)
        } else {
            (w as usize).div_ceil(8).clamp(1, max_cols)
        };
        let rows = if want_rows > 0 {
            (want_rows as usize).min(max_rows)
        } else {
            (h as usize).div_ceil(16).clamp(1, max_rows)
        };
        let line = self.grid.scrollback_len() + self.grid.cursor.row;
        let col = self.grid.cursor.col;
        self.images.push(graphics::Image {
            id: self.next_image,
            line,
            col,
            cols,
            rows,
            width: w,
            height: h,
            rgba,
        });
        self.next_image += 1;
        // **持ちすぎない。** 端末に出た絵は流れて消えるもの。
        if self.images.len() > 32 {
            self.images.remove(0);
        }
        for _ in 0..rows {
            self.grid.line_feed();
        }
        self.grid.carriage_return();
    }

    // ---- 所有権の裁定（唯一の場所） --------------------------------------
    //
    // `arch.md` の不変条件: 所有権の判断はここ以外に書かない。
    // 分散させると「今どっちが持っているか分からない」状態が生まれ、
    // このモデルの生命線が切れる。

    /// マウスの所有者。
    ///
    /// alt screen かつ子プロセスがマウスレポートを要求しているときだけ子へ渡す。
    /// primary screen は常にドキュメントなので、たとえ要求されても渡さない。
    pub fn mouse_owner(&self) -> InputOwner {
        if let Some(pinned) = self.pins.mouse {
            return pinned;
        }
        if self.grid.is_alt() && self.modes.mouse != MouseTracking::Off {
            InputOwner::Child
        } else {
            InputOwner::Tsumugi
        }
    }

    /// `Esc` を含むキーの所有者。alt screen なら子が持つ。
    ///
    /// `C-\` はこの判定を迂回して常に tsumugi が取る（モーダル層の責務）。
    pub fn key_owner(&self) -> InputOwner {
        if let Some(pinned) = self.pins.key {
            return pinned;
        }
        if self.grid.is_alt() {
            InputOwner::Child
        } else {
            InputOwner::Tsumugi
        }
    }

    /// ドキュメント絶対行の範囲を、**印ごと**取り除く。
    ///
    /// `grid` を直に触ると印が置き去りになり、ガターと `[[` `]]` が
    /// 無関係な行を指す。表示から消す道はここ 1 本に絞る。
    pub fn remove_document_lines(&mut self, from: usize, to: usize) {
        let last = to.min(self.grid.document_len().saturating_sub(1));
        if from > last {
            return;
        }
        self.grid.remove_document_lines(from, last);
        self.marks.remove_lines(from, last);
    }

    fn mark(&mut self, kind: MarkKind) {
        // alt screen 上のセマンティックマークは捨てる。
        //
        // OSC 133 はシェルのプロンプト構造を表すもので、それが在るのは primary。
        // alt screen では履歴が伸びないため絶対行番号が意味を持たず、記録すると
        // `]e` が実在しない行へ飛ぶ。M0 プローブが実際にこれを踏んだ。
        if self.grid.is_alt() {
            return;
        }
        let line = self.grid.cursor_absolute();
        let col = self.grid.cursor.col;
        self.marks.push(kind, line, col);
    }

    /// SGR。`grid.pen` を動かす唯一の場所。
    fn set_sgr(&mut self, params: &Params) {
        if params.is_empty() {
            self.grid.pen = Attrs::default();
            return;
        }
        let mut iter = params.iter();
        while let Some(sub) = iter.next() {
            let Some(&code) = sub.first() else { continue };
            let pen = &mut self.grid.pen;
            match code {
                0 => *pen = Attrs::default(),
                1 => pen.set(Attrs::BOLD),
                2 => pen.set(Attrs::DIM),
                3 => pen.set(Attrs::ITALIC),
                4 => pen.set(Attrs::UNDERLINE),
                5 | 6 => pen.set(Attrs::BLINK),
                7 => pen.set(Attrs::REVERSE),
                8 => pen.set(Attrs::HIDDEN),
                9 => pen.set(Attrs::STRIKE),
                21 | 22 => pen.unset(Attrs::BOLD | Attrs::DIM),
                23 => pen.unset(Attrs::ITALIC),
                24 => pen.unset(Attrs::UNDERLINE),
                25 => pen.unset(Attrs::BLINK),
                27 => pen.unset(Attrs::REVERSE),
                28 => pen.unset(Attrs::HIDDEN),
                29 => pen.unset(Attrs::STRIKE),
                30..=37 => pen.fg = Color::Indexed((code - 30) as u8),
                38 => {
                    if let Some(c) = extended_color(sub, &mut iter) {
                        self.grid.pen.fg = c;
                    }
                }
                39 => pen.fg = Color::Default,
                40..=47 => pen.bg = Color::Indexed((code - 40) as u8),
                48 => {
                    if let Some(c) = extended_color(sub, &mut iter) {
                        self.grid.pen.bg = c;
                    }
                }
                49 => pen.bg = Color::Default,
                90..=97 => pen.fg = Color::Indexed((code - 90 + 8) as u8),
                100..=107 => pen.bg = Color::Indexed((code - 100 + 8) as u8),
                _ => {}
            }
        }
    }

    fn set_dec_mode(&mut self, mode: u16, on: bool) {
        match mode {
            1 => self.modes.app_cursor_keys = on,
            9 => {
                self.modes.mouse = if on {
                    MouseTracking::X10
                } else {
                    MouseTracking::Off
                }
            }
            1000 => {
                self.modes.mouse = if on {
                    MouseTracking::Normal
                } else {
                    MouseTracking::Off
                }
            }
            1002 => {
                self.modes.mouse = if on {
                    MouseTracking::ButtonEvent
                } else {
                    MouseTracking::Off
                }
            }
            1003 => {
                self.modes.mouse = if on {
                    MouseTracking::AnyEvent
                } else {
                    MouseTracking::Off
                }
            }
            1004 => self.modes.focus_events = on,
            1005 => {
                self.modes.mouse_encoding = if on {
                    MouseEncoding::Utf8
                } else {
                    MouseEncoding::Default
                }
            }
            1006 => {
                self.modes.mouse_encoding = if on {
                    MouseEncoding::Sgr
                } else {
                    MouseEncoding::Default
                }
            }
            1015 => {
                self.modes.mouse_encoding = if on {
                    MouseEncoding::Urxvt
                } else {
                    MouseEncoding::Default
                }
            }
            47 | 1047 => {
                if on {
                    self.grid.enter_alt()
                } else {
                    self.grid.leave_alt()
                }
            }
            1048 => {
                if on {
                    self.grid.save_cursor()
                } else {
                    self.grid.restore_cursor()
                }
            }
            1049 => {
                if on {
                    self.grid.save_cursor();
                    self.grid.enter_alt();
                } else {
                    self.grid.leave_alt();
                    self.grid.restore_cursor();
                }
            }
            2004 => self.modes.bracketed_paste = on,
            _ => {}
        }
    }
}

/// 数値パラメータの取り出し。`0` を既定値に読み替える版（座標・カウント用）。
fn p1(params: &Params, idx: usize) -> usize {
    params
        .iter()
        .nth(idx)
        .and_then(|sub| sub.first().copied())
        .filter(|&v| v != 0)
        .unwrap_or(1) as usize
}

/// `38` / `48` の後続を読んで色を作る。
///
/// セミコロン形（`38;5;n` / `38;2;r;g;b`）とコロン形（`38:5:n` / `38:2::r:g:b`）の
/// 両方が現実に飛んでくる。vte はコロン形を 1 つのサブパラメータにまとめて渡す。
fn extended_color<'a>(sub: &[u16], rest: &mut impl Iterator<Item = &'a [u16]>) -> Option<Color> {
    if sub.len() > 1 {
        return color_from_parts(&sub[1..]);
    }
    match rest.next()?.first().copied()? {
        5 => Some(Color::Indexed(*rest.next()?.first()? as u8)),
        2 => {
            let r = *rest.next()?.first()? as u8;
            let g = *rest.next()?.first()? as u8;
            let b = *rest.next()?.first()? as u8;
            Some(Color::Rgb(r, g, b))
        }
        _ => None,
    }
}

fn color_from_parts(parts: &[u16]) -> Option<Color> {
    match parts.first()? {
        5 => parts.get(1).map(|n| Color::Indexed(*n as u8)),
        2 => {
            // `38:2::r:g:b` は色空間 ID が 1 つ挟まる。長さで見分ける。
            let v = if parts.len() >= 5 {
                &parts[2..]
            } else {
                &parts[1..]
            };
            Some(Color::Rgb(
                *v.first()? as u8,
                *v.get(1)? as u8,
                *v.get(2)? as u8,
            ))
        }
        _ => None,
    }
}

/// 数値パラメータの取り出し。既定 `0` 版（モード番号用）。
fn p0(params: &Params, idx: usize) -> u16 {
    params
        .iter()
        .nth(idx)
        .and_then(|sub| sub.first().copied())
        .unwrap_or(0)
}

impl Perform for TermState {
    fn print(&mut self, c: char) {
        // `ESC ( 0` の間は罫線文字に読み替える。無視すると、枠を描く
        // TUI が `lqqqk` のような字の並びに化ける。
        let c = if self.line_drawing {
            dec_special(c).unwrap_or(c)
        } else {
            c
        };
        self.grid.print(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => {}                     // BEL
            0x08 => self.grid.backspace(), // BS
            0x09 => self.grid.tab(),       // HT
            0x0A..=0x0C => self.grid.line_feed(),
            0x0D => self.grid.carriage_return(),
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let private = intermediates.first() == Some(&b'?');

        if private {
            match action {
                'h' => {
                    for sub in params.iter() {
                        if let Some(&m) = sub.first() {
                            self.set_dec_mode(m, true);
                        }
                    }
                }
                'l' => {
                    for sub in params.iter() {
                        if let Some(&m) = sub.first() {
                            self.set_dec_mode(m, false);
                        }
                    }
                }
                _ => {}
            }
            return;
        }

        // **訊かれたら答える。** 相手は「この端末は何ができるか」を
        // 問い合わせて振る舞いを決める。無応答だと、待たされるか
        // 誤った既定へ倒れる（powerlevel10k は CPR、vim は DA1 を使う）。
        match action {
            // Device Attributes。VT220 相当＋色を名乗る。
            'c' if intermediates.first() == Some(&b'>') => {
                self.reply(b"[>1;10;0c");
            }
            'c' => {
                self.reply(b"[?62;4;22c");
            }
            // Device Status Report
            'n' => {
                match p0(params, 0) {
                    5 => self.reply(b"[0n"), // 元気です
                    6 => {
                        // カーソル位置（1 始まり）
                        let (r, c) = (self.grid.cursor.row + 1, self.grid.cursor.col + 1);
                        self.reply(format!("[{r};{c}R").as_bytes());
                    }
                    _ => {}
                }
            }
            // カーソルの形（DECSCUSR）。`CSI Ps SP q`
            'q' if intermediates.first() == Some(&b' ') => {
                self.cursor_style = p0(params, 0);
            }
            'A' => self.grid.move_up(p1(params, 0)),
            'B' | 'e' => self.grid.move_down(p1(params, 0)),
            'C' | 'a' => self.grid.move_right(p1(params, 0)),
            'D' => self.grid.move_left(p1(params, 0)),
            'E' => {
                self.grid.move_down(p1(params, 0));
                self.grid.carriage_return();
            }
            'F' => {
                self.grid.move_up(p1(params, 0));
                self.grid.carriage_return();
            }
            'G' | '`' => self.grid.move_to_col(p1(params, 0) - 1),
            'd' => self.grid.move_to_row(p1(params, 0) - 1),
            'H' | 'f' => self.grid.move_to(p1(params, 0) - 1, p1(params, 1) - 1),
            'J' => self.grid.erase_display(p0(params, 0)),
            'K' => self.grid.erase_line(p0(params, 0)),
            'L' => self.grid.insert_lines(p1(params, 0)),
            'M' => self.grid.delete_lines(p1(params, 0)),
            'P' => self.grid.delete_chars(p1(params, 0)),
            'X' => self.grid.erase_chars(p1(params, 0)),
            '@' => self.grid.insert_chars(p1(params, 0)),
            'S' => self.grid.scroll_up(p1(params, 0)),
            'T' => self.grid.scroll_down(p1(params, 0)),
            'r' => {
                let top = p1(params, 0) - 1;
                let bot = params
                    .iter()
                    .nth(1)
                    .and_then(|s| s.first().copied())
                    .filter(|&v| v != 0)
                    .map(|v| v as usize - 1)
                    .unwrap_or(self.grid.rows - 1);
                self.grid.set_scroll_region(top, bot);
            }
            'm' => self.set_sgr(params),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        // `ESC ( 0` / `ESC ( B`。G0 に何を入れるか。
        if intermediates.first() == Some(&b'(') {
            self.line_drawing = byte == b'0';
            return;
        }
        match byte {
            b'7' => self.grid.save_cursor(),
            b'8' => self.grid.restore_cursor(),
            b'M' => self.grid.reverse_index(),
            b'D' => self.grid.line_feed(),
            b'E' => {
                self.grid.line_feed();
                self.grid.carriage_return();
            }
            b'c' => {
                self.grid.reset();
                self.marks.clear();
            }
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, action: char) {
        // Sixel（`DCS ... q`）。ほかの DCS は知らないので受け取らない。
        if action == 'q' {
            self.sixel = Some(sixel::Decoder::new());
        }
    }

    fn put(&mut self, byte: u8) {
        if let Some(d) = self.sixel.as_mut() {
            d.put(byte);
        }
    }

    fn unhook(&mut self) {
        let Some(d) = self.sixel.take() else {
            return;
        };
        let Some((rgba, w, h)) = d.finish() else {
            return;
        };
        self.place_image(rgba, w, h, 0, 0);
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if self.log_osc {
            let raw: Vec<String> = params
                .iter()
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect();
            self.osc_log.push(raw.join(";"));
        }

        let Some(&kind) = params.first() else { return };

        match kind {
            b"0" | b"2" => {
                if let Some(t) = params.get(1) {
                    self.title = sanitize_osc_text(t, 256);
                }
            }
            b"7" => {
                if let Some(u) = params.get(1) {
                    self.cwd = Some(sanitize_osc_text(u, 4096)).filter(|s| !s.is_empty());
                }
            }
            // ハイパーリンク。`OSC 8 ; params ; URI ST` … `OSC 8 ; ; ST` で閉じる。
            b"8" => {
                let uri = params.get(2).map(|u| sanitize_osc_text(u, 2048));
                self.grid.pen_link = match uri.filter(|u| !u.is_empty()) {
                    None => 0,
                    Some(u) => {
                        // **同じ行き先は 1 つにまとめる。** ls の出力 1 画面で
                        // 何百回も同じ URI が来る。
                        match self.links.iter().position(|x| *x == u) {
                            Some(i) => i as u16 + 1,
                            None if self.links.len() < 4096 => {
                                self.links.push(u);
                                self.links.len() as u16
                            }
                            // 溜まりすぎたら、それ以上は覚えない（印も付けない）
                            None => 0,
                        }
                    }
                };
            }
            // 前景 / 背景の問い合わせ。nvim はこれで明暗を決める。
            // **答えないと、暗い背景に暗い字を置かれる。**
            b"10" | b"11" => {
                if params.get(1).is_some_and(|v| v.starts_with(b"?")) {
                    let (r, g, b) = if kind == b"10" {
                        self.fg_rgb
                    } else {
                        self.bg_rgb
                    };
                    let n = if kind == b"10" { 10 } else { 11 };
                    // xterm と同じ 16 bit 表記（各成分を 2 回繰り返す）。
                    self.reply(
                        format!("]{n};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\\")
                            .as_bytes(),
                    );
                }
            }
            // クリップボード（OSC 52）。**書き込みだけ受ける。**
            // 読み出しに答えると、画面に字を出しただけで中身を抜かれる。
            b"52" => {
                if let Some(data) = params.get(2)
                    && data != b"?"
                    && let Ok(text) = BASE64_STANDARD.decode(data)
                    && let Ok(text) = String::from_utf8(text)
                    && text.len() <= 1024 * 1024
                {
                    self.clipboard = Some(text);
                }
            }
            // 133 が本命。633 は VSCode / PSReadLine 系の同義シーケンス。
            b"133" | b"633" => {
                if let Some(k) = semantic::parse_mark(&params[1..]) {
                    self.mark(k);
                }
            }
            _ => {}
        }
    }
}

/// OSC が持ち込む文字列を、そのまま画面や OS へ渡さない形にする。
///
/// ここへ来る中身は**子プロセスが自由に決められる**（`printf` 1 行で好きな
/// タイトルを付けられる）。制御文字が混ざるとステータス行やタブの見た目が壊れ、
/// 長さに上限が無ければメモリがそのぶん伸びる。
/// 表示に使うものは、表示できる字だけ・決めた長さまでに切る。
fn sanitize_osc_text(raw: &[u8], max_chars: usize) -> String {
    String::from_utf8_lossy(raw)
        .chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect()
}

/// パーサと状態を束ねたもの。
pub struct Terminal {
    parser: vte::Parser,
    apc: graphics::ApcSplitter,
    /// 履歴の先頭から捨てた行数の累計。**外にも配る**（印と絵はここで
    /// 寄せているが、畳みや表示位置はホストが持っているため）。
    dropped: usize,
    /// 直近の組み直しで動いた行番号の対応表。
    reflow_map: Option<Vec<usize>>,
    pub state: TermState,
}

impl Terminal {
    pub fn new(cols: usize, rows: usize, amb: AmbiguousWidth) -> Self {
        Self {
            parser: vte::Parser::new(),
            apc: graphics::ApcSplitter::default(),
            dropped: 0,
            reflow_map: None,
            state: TermState::new(cols, rows, amb),
        }
    }

    /// 捨てた行数を受け取って 0 に戻す。**ホストが持っている行番号**
    /// （表示位置・畳み）を同じだけ寄せるために要る。取りこぼすと、
    /// 畳んだ範囲が別の行を指す。
    pub fn take_dropped(&mut self) -> usize {
        std::mem::take(&mut self.dropped)
    }

    /// 幅を変える。**印と絵の行番号も一緒に寄せる。**
    ///
    /// 組み直すと行番号が動く。寄せ忘れると、ガターの印と絵が別の行を指す。
    /// 畳みはホストが持っているので、`take_reflow_map` で受け取って寄せる。
    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.state.grid.resize(cols, rows);
        let Some(map) = self.state.grid.take_reflow_map() else {
            return;
        };
        let at = |old: usize| map.get(old).copied().unwrap_or(old);
        self.state.marks.remap(&|l| at(l));
        for img in &mut self.state.images {
            img.line = at(img.line);
        }
        self.reflow_map = Some(map);
    }

    /// 直近の組み直しの対応表。ホスト側の行番号（表示位置・畳み）を寄せる。
    pub fn take_reflow_map(&mut self) -> Option<Vec<usize>> {
        self.reflow_map.take()
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        // 絵（Kitty graphics）は APC で来る。`vte` は APC を捨てるので、
        // 解析器へ渡す前に抜き出す。**残りはこれまでどおり**流す。
        let (rest, apcs) = self.apc.split(bytes);
        for body in apcs {
            self.state.take_graphics(&body);
        }
        self.parser.advance(&mut self.state, &rest);
        // 上限を超えて履歴の先頭が捨てられたら、印を同じだけ寄せる。
        // ここが**唯一の合わせ場所**。取りこぼすと印が別の行を指す。
        let dropped = self.state.grid.take_dropped();
        self.dropped += dropped;
        self.state.marks.shift_up(dropped);
        // 絵も同じだけ寄せる。**印と同じ理由**で、忘れると別の行に出る。
        if dropped > 0 {
            self.state
                .images
                .retain_mut(|img| match img.line.checked_sub(dropped) {
                    Some(l) => {
                        img.line = l;
                        true
                    }
                    None => false,
                });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term() -> Terminal {
        Terminal::new(40, 6, AmbiguousWidth::Wide)
    }

    /// **訊かれたら答える。** 無応答だと、相手は待たされるか誤った既定へ倒れる。
    #[test]
    fn the_terminal_answers_when_it_is_asked() {
        let mut t = term();
        t.feed(b"[c"); // DA1
        assert!(!t.state.replies.is_empty(), "DA1 に答えていない");
        t.state.replies.clear();

        t.feed(b"[5n"); // DSR
        assert_eq!(t.state.replies, b"[0n");
        t.state.replies.clear();

        t.feed(b"hi[6n"); // CPR。1 始まりで返す
        assert_eq!(t.state.replies, b"[1;3R");
        t.state.replies.clear();

        t.state.bg_rgb = (0x11, 0x22, 0x33);
        t.feed(b"]11;?\\");
        let got = String::from_utf8_lossy(&t.state.replies).to_string();
        assert!(
            got.contains("rgb:1111/2222/3333"),
            "OSC 11 の答えが違う: {got}"
        );
    }

    /// クリップボードは**書き込みだけ**受ける。読み出しに答えると、
    /// 画面に字を出しただけで中身を抜かれる。
    #[test]
    fn the_clipboard_can_be_written_but_never_read() {
        let mut t = term();
        t.feed(b"]52;c;aGVsbG8=\\");
        assert_eq!(t.state.clipboard.as_deref(), Some("hello"));
        t.state.clipboard = None;
        t.feed(b"]52;c;?\\");
        assert!(t.state.replies.is_empty(), "読み出しに答えてしまった");
        assert!(t.state.clipboard.is_none());
    }

    /// 罫線の文字セット。無視すると枠が `lqqqk` に化ける。
    #[test]
    fn the_line_drawing_charset_draws_lines() {
        let mut t = term();
        t.feed(b"(0lqqk(Bx");
        assert_eq!(t.state.grid.document_line(0).unwrap().text(), "┌──┐x");
    }

    #[test]
    fn plain_text_lands_on_the_grid() {
        let mut t = term();
        t.feed(b"hello");
        assert_eq!(t.state.grid.document_line(0).unwrap().text(), "hello");
    }

    #[test]
    fn cursor_position_and_erase() {
        let mut t = term();
        t.feed(b"abcdef\x1b[1;1H\x1b[K");
        assert_eq!(t.state.grid.document_line(0).unwrap().text(), "");
    }

    #[test]
    fn osc133_produces_marks() {
        let mut t = term();
        // プロンプト -> コマンド -> 出力 -> 終了(コード 1)
        t.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07cargo build\r\n");
        t.feed(b"\x1b]133;C\x07error: boom\r\n");
        t.feed(b"\x1b]133;D;1\x07");

        let blocks = t.state.marks.blocks();
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].is_error(), "終了コード 1 を拾えていない");
        assert_eq!(blocks[0].prompt_line, 0);
    }

    #[test]
    fn osc633_is_accepted_as_an_alias() {
        let mut t = term();
        t.feed(b"\x1b]633;A\x07");
        assert_eq!(t.state.marks.all().len(), 1);
    }

    #[test]
    fn st_terminated_osc_is_accepted() {
        // BEL 終端ではなく ESC \ 終端の形
        let mut t = term();
        t.feed(b"\x1b]133;A\x1b\\");
        assert_eq!(t.state.marks.all().len(), 1);
    }

    #[test]
    fn alt_screen_transfers_ownership() {
        let mut t = term();
        assert_eq!(t.state.key_owner(), InputOwner::Tsumugi);
        assert_eq!(t.state.mouse_owner(), InputOwner::Tsumugi);

        // 全画面 TUI が起動: alt screen + SGR マウスレポート
        t.feed(b"\x1b[?1049h\x1b[?1002h\x1b[?1006h");
        assert!(t.state.grid.is_alt());
        assert_eq!(t.state.modes.mouse, MouseTracking::ButtonEvent);
        assert_eq!(t.state.modes.mouse_encoding, MouseEncoding::Sgr);
        assert_eq!(t.state.key_owner(), InputOwner::Child);
        assert_eq!(t.state.mouse_owner(), InputOwner::Child);

        // 抜けたら戻る
        t.feed(b"\x1b[?1002l\x1b[?1049l");
        assert!(!t.state.grid.is_alt());
        assert_eq!(t.state.key_owner(), InputOwner::Tsumugi);
        assert_eq!(t.state.mouse_owner(), InputOwner::Tsumugi);
    }

    #[test]
    fn primary_screen_never_yields_the_mouse() {
        // primary でマウスレポートを要求されても、そこはドキュメントなので渡さない
        let mut t = term();
        t.feed(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(t.state.modes.mouse, MouseTracking::Normal);
        assert_eq!(t.state.mouse_owner(), InputOwner::Tsumugi);
    }

    #[test]
    fn pin_overrides_arbitration() {
        let mut t = term();
        t.feed(b"\x1b[?1049h\x1b[?1002h");
        assert_eq!(t.state.mouse_owner(), InputOwner::Child);
        t.state.pins.mouse = Some(InputOwner::Tsumugi);
        assert_eq!(t.state.mouse_owner(), InputOwner::Tsumugi);
    }

    #[test]
    fn alt_screen_tui_does_not_pollute_scrollback() {
        let mut t = term();
        t.feed(b"before\r\n");
        let before = t.state.grid.scrollback_len();
        t.feed(b"\x1b[?1049h");
        for _ in 0..20 {
            t.feed(b"noise\r\n");
        }
        t.feed(b"\x1b[?1049l");
        assert_eq!(
            t.state.grid.scrollback_len(),
            before,
            "alt screen の出力が履歴を汚している"
        );
    }

    #[test]
    fn marks_emitted_on_alt_screen_are_discarded() {
        // M0 プローブが踏んだ実バグの回帰テスト。
        // alt screen 上の OSC 133 を記録すると、`]e` が実在しない行へ飛ぶ。
        let mut t = term();
        t.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07cmd\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07");
        let before = t.state.marks.all().len();

        t.feed(b"\x1b[?1049h");
        t.feed(b"\x1b]133;A\x07"); // 全画面 TUI が出したもの: 無視すべき
        t.feed(b"\x1b]133;D;9\x07");
        t.feed(b"\x1b[?1049l");

        assert_eq!(
            t.state.marks.all().len(),
            before,
            "alt screen 上のマークが記録されている"
        );
        assert_eq!(t.state.marks.blocks().len(), 1);
    }

    fn attrs_at(t: &Terminal, row: usize, col: usize) -> Attrs {
        t.state.grid.document_line(row).unwrap().cells[col].attrs
    }

    #[test]
    fn sgr_colors_land_on_the_cells() {
        let mut t = term();
        t.feed(b"[31mR[1;32mG[0mn");
        assert_eq!(attrs_at(&t, 0, 0).fg, Color::Indexed(1));
        assert_eq!(attrs_at(&t, 0, 1).fg, Color::Indexed(2));
        assert!(attrs_at(&t, 0, 1).has(Attrs::BOLD));
        assert_eq!(attrs_at(&t, 0, 2), Attrs::default(), "0 で戻らない");
    }

    #[test]
    fn extended_colors_accept_both_semicolon_and_colon_forms() {
        let mut t = term();
        t.feed(b"[38;5;196ma");
        t.feed(b"[38;2;10;20;30mb");
        t.feed(b"[38:5:44mc");
        t.feed(b"[38:2::1:2:3md");
        assert_eq!(attrs_at(&t, 0, 0).fg, Color::Indexed(196));
        assert_eq!(attrs_at(&t, 0, 1).fg, Color::Rgb(10, 20, 30));
        assert_eq!(attrs_at(&t, 0, 2).fg, Color::Indexed(44));
        assert_eq!(attrs_at(&t, 0, 3).fg, Color::Rgb(1, 2, 3));
    }

    #[test]
    fn bright_codes_map_to_the_upper_eight() {
        let mut t = term();
        t.feed(b"[91;104mx");
        assert_eq!(attrs_at(&t, 0, 0).fg, Color::Indexed(9));
        assert_eq!(attrs_at(&t, 0, 0).bg, Color::Indexed(12));
    }

    #[test]
    fn erasing_paints_with_the_current_background() {
        // 色付きの `clear` が抜けて見えないこと（xterm 以来の「背景で消す」）
        let mut t = term();
        t.feed(b"[44m[2J");
        assert_eq!(attrs_at(&t, 0, 5).bg, Color::Indexed(4));
        assert_eq!(
            attrs_at(&t, 0, 5).fg,
            Color::Default,
            "前景までは持ち越さない"
        );
    }

    #[test]
    fn a_wide_char_spacer_carries_the_same_attrs() {
        // 背景色が全角文字の右半分で途切れると縞になる
        let mut t = term();
        t.feed("[41m日".as_bytes());
        assert_eq!(attrs_at(&t, 0, 0).bg, Color::Indexed(1));
        assert_eq!(attrs_at(&t, 0, 1).bg, Color::Indexed(1));
    }

    #[test]
    fn a_line_round_trips_through_its_own_sgr() {
        // スナップショットの往復。サーバが吐いた ANSI をクライアントが解析して同じ絵になる。
        let mut src = term();
        src.feed("[1;38;5;208;44m警告[0m: [4mfile.rs[0m".as_bytes());
        let ansi = src.state.grid.line_ansi(0).unwrap();

        let mut dst = term();
        dst.feed(ansi.as_bytes());

        let a = src.state.grid.document_line(0).unwrap();
        let b = dst.state.grid.document_line(0).unwrap();
        assert_eq!(a.text(), b.text());
        for (i, (x, y)) in a.cells.iter().zip(b.cells.iter()).enumerate() {
            assert_eq!(x.attrs, y.attrs, "{i} 列目の属性が復元できていない");
        }
    }

    #[test]
    fn a_trailing_colored_run_survives_the_snapshot() {
        // パワーライン風の帯が行末まで伸びている場合、trim で消してはいけない
        let mut t = term();
        t.feed(b"[45m[K");
        let ansi = t.state.grid.line_ansi(0).unwrap();
        assert!(
            ansi.contains("45m") || ansi.contains("48;5;5"),
            "背景色が落ちた: {ansi:?}"
        );
        assert!(
            ansi.trim_end_matches("[0m").ends_with(' '),
            "空白が残っていない"
        );
    }

    /// タイトルは**子プロセスが自由に決められる**。制御文字と長さを
    /// 素通しにすると、ステータス行の見た目が壊れ、メモリがそのぶん伸びる。
    #[test]
    fn a_hostile_title_cannot_carry_control_characters_or_grow_without_end() {
        let mut t = term();
        t.feed(b"]0;ab[31mc");
        assert!(
            !t.state.title.chars().any(char::is_control),
            "制御文字が残っている: {:?}",
            t.state.title
        );

        let long = format!("]0;{}", "x".repeat(10_000));
        t.feed(long.as_bytes());
        assert!(
            t.state.title.chars().count() <= 256,
            "長さの上限が効いていない: {}",
            t.state.title.chars().count()
        );
    }

    /// 履歴の先頭が捨てられたら、印も同じだけ寄る。
    /// ここがずれると、ガターの印と `[[` `]]` が無関係な行を指す。
    #[test]
    fn marks_follow_the_lines_when_old_scrollback_is_dropped() {
        let mut t = Terminal::new(20, 2, AmbiguousWidth::Wide);
        t.state.grid.set_max_scrollback(4);
        t.feed(
            b"]133;Aprompt
",
        );
        let before = t.state.marks.all()[0].line;
        for _ in 0..20 {
            t.feed(
                b"x
",
            );
        }
        assert_eq!(t.state.grid.scrollback_len(), 4, "履歴が上限を超えている");
        if let Some(m) = t.state.marks.all().first() {
            assert!(
                m.line < t.state.grid.document_len(),
                "印が実在しない行を指している: {} / {}",
                m.line,
                t.state.grid.document_len()
            );
            assert!(m.line < before, "行が捨てられたのに印が動いていない");
        }
    }

    #[test]
    fn title_and_cwd_are_captured() {
        let mut t = term();
        t.feed(b"\x1b]0;my title\x07");
        t.feed(b"\x1b]7;file://host/c/dev\x07");
        assert_eq!(t.state.title, "my title");
        assert_eq!(t.state.cwd.as_deref(), Some("file://host/c/dev"));
    }
}
