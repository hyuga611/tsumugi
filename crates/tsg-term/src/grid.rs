//! セルグリッドとスクロールバック。
//!
//! `concept.md` の中心命題「グリッドは末尾を持つドキュメントである」を実装する層。
//! スクロールバックと現在画面は連続した1つのアドレス空間として `document_*` で見える。

use unicode_width::UnicodeWidthChar;

use crate::attrs::Attrs;

/// East Asian Ambiguous 幅の扱い。`arch.md` §6.1。
///
/// **既定は `Narrow`（1 幅）。** 設計時は日本語環境の慣習に合わせて 2 幅を
/// 既定にしていたが、罫線素片（`─` `│` `╭`）・`●` `•` `·` `←` `█` はどれも
/// Ambiguous なので、2 幅にすると**TUI が軒並み崩れる**。枠線が破線になり、
/// 消去とカーソル移動の桁がずれて古い文字が残る（Claude Code で踏んだ）。
/// かな・漢字は Wide クラスなのでこの設定に左右されない。
/// 2 幅が要るなら `[font] ambiguous_width = "wide"`。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AmbiguousWidth {
    #[default]
    Narrow,
    Wide,
}

/// 1文字の表示幅を返す。制御文字・結合文字は 0。
pub fn char_width(c: char, amb: AmbiguousWidth) -> usize {
    match amb {
        AmbiguousWidth::Wide => c.width_cjk().unwrap_or(0),
        AmbiguousWidth::Narrow => c.width().unwrap_or(0),
    }
}

/// プロセス全体の Ambiguous 幅。
///
/// **1 プロセスに 1 つと決める。** 画面ごとに違うと、同じ文字列が
/// ペインをまたいだ瞬間に幅を変え、桁の勘定がどこかで必ず食い違う。
/// 起動時に設定から一度だけ決める（`Lang` と同じ扱い）。
static AMBIGUOUS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub fn set_ambiguous(a: AmbiguousWidth) {
    let v = u8::from(a == AmbiguousWidth::Wide);
    AMBIGUOUS.store(v, std::sync::atomic::Ordering::Relaxed);
}

pub fn ambiguous() -> AmbiguousWidth {
    if AMBIGUOUS.load(std::sync::atomic::Ordering::Relaxed) == 1 {
        AmbiguousWidth::Wide
    } else {
        AmbiguousWidth::Narrow
    }
}

/// いまの設定での表示幅。桁を数えるところは全部これを通す。
pub fn width_of(c: char) -> usize {
    char_width(c, ambiguous())
}

/// 1セル。
///
/// TODO(M4): `String` はセルあたりのコストが高い。書記素クラスタの
/// インターン、あるいは `char` + 稀な結合列だけ別表、に置き換える。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    /// 書記素クラスタ。結合文字・異体字セレクタはここに連結される。
    pub text: String,
    /// 1 = 半角、2 = 全角の先頭、0 = 全角の右半分（スペーサ）
    pub width: u8,
    /// SGR。スペーサは直前のセルと同じものを持つ（背景色が途切れないように）
    pub attrs: Attrs,
}

impl Default for Cell {
    fn default() -> Self {
        Self::blank(Attrs::default())
    }
}

impl Cell {
    /// 空白セル。`attrs` は消去時の背景色を運ぶためのもの。
    pub fn blank(attrs: Attrs) -> Self {
        Self {
            text: " ".to_string(),
            width: 1,
            attrs,
        }
    }

    /// 全角の右半分（それ自体は文字を持たない）。
    ///
    /// **全角文字の直後には必ずこれを置く。** 置かないと、セルの添字と列が
    /// 1 つずつずれて文字が重なって描かれる（ファイルバッファで実際に踏んだ）。
    pub fn spacer(attrs: Attrs) -> Self {
        Self {
            text: String::new(),
            width: 0,
            attrs,
        }
    }

    pub fn is_spacer(&self) -> bool {
        self.width == 0
    }
}

#[derive(Clone, Debug, Default)]
pub struct Line {
    pub cells: Vec<Cell>,
    /// 自動折り返しで次の行へ続いているか（論理行の途中）
    pub wrapped: bool,
}

impl Line {
    fn new(cols: usize) -> Self {
        Self::blank(cols, Attrs::default())
    }

    fn blank(cols: usize, attrs: Attrs) -> Self {
        Self {
            cells: vec![Cell::blank(attrs); cols],
            wrapped: false,
        }
    }

    /// 行末の空白を落とした表示文字列。
    pub fn text(&self) -> String {
        let mut s = String::new();
        for cell in &self.cells {
            if !cell.is_spacer() {
                s.push_str(&cell.text);
            }
        }
        s.trim_end().to_string()
    }

    /// 中身が無い行か。リサイズでどの行を捨ててよいかの判断に使う。
    pub fn is_blank(&self) -> bool {
        self.cells
            .iter()
            .all(|c| c.is_spacer() || c.text.trim().is_empty())
    }

    /// SGR 付きの行。再アタッチのスナップショットで使う。
    ///
    /// 行末は落とすが、**既定でない背景色を持つセルは落とさない**。
    /// 色付きの帯が行末まで伸びている表示（プロンプトのパワーライン等）が
    /// 切れてしまうため。
    pub fn ansi(&self) -> String {
        let last = self
            .cells
            .iter()
            .rposition(|c| c.text.trim() != "" || c.attrs != Attrs::default());
        let Some(last) = last else {
            return String::new();
        };

        let mut out = String::new();
        let mut pen = Attrs::default();
        for cell in self.cells.iter().take(last + 1) {
            if cell.is_spacer() {
                continue;
            }
            if cell.attrs != pen {
                out.push_str(&cell.attrs.sgr());
                pen = cell.attrs;
            }
            out.push_str(&cell.text);
        }
        if pen != Attrs::default() {
            out.push_str("[0m");
        }
        out
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
}

/// スクロールバックの既定の上限（行）。
///
/// **上限が無いと、出しっぱなしのプロセス 1 つでメモリが尽きる**（`yes` を
/// 走らせたまま忘れる、巨大なログを `cat` する）。端末は「他人が出した
/// バイト列」を無制限に受け取る立場なので、ここは必ず閉じておく。
/// 1 万行はおよそ 400 画面分。実用で足りて、80 桁なら数十 MB に収まる。
pub const DEFAULT_MAX_SCROLLBACK: usize = 10_000;

pub struct Grid {
    pub cols: usize,
    pub rows: usize,
    pub amb: AmbiguousWidth,

    /// 現在アクティブな画面（primary か alt のどちらか）
    screen: Vec<Line>,
    /// alt screen に入っている間、primary の内容をここへ退避する
    saved_primary: Option<Vec<Line>>,
    /// primary から押し出された行。alt screen では増えない
    scrollback: Vec<Line>,

    pub cursor: Cursor,
    saved_cursor: Cursor,

    /// いま書き込みに使う SGR。CSI m がここを動かす。
    pub pen: Attrs,

    /// スクロール領域（0-origin, 両端含む）
    scroll_top: usize,
    scroll_bot: usize,

    /// スクロールバックに残す最大行数。
    max_scrollback: usize,
    /// 上限を超えて先頭から捨てた行数。**まだ印へ反映していない分。**
    dropped: usize,
}

impl Grid {
    pub fn new(cols: usize, rows: usize, amb: AmbiguousWidth) -> Self {
        Self {
            cols,
            rows,
            amb,
            screen: (0..rows).map(|_| Line::new(cols)).collect(),
            saved_primary: None,
            scrollback: Vec::new(),
            cursor: Cursor::default(),
            saved_cursor: Cursor::default(),
            pen: Attrs::default(),
            scroll_top: 0,
            scroll_bot: rows.saturating_sub(1),
            max_scrollback: DEFAULT_MAX_SCROLLBACK,
            dropped: 0,
        }
    }

    /// スクロールバックの上限を変える（設定から）。
    ///
    /// 0 は「履歴を持たない」ではなく既定に戻す扱いにする。履歴が完全に無い
    /// ターミナルは、この製品では**モーションの行き先が消える**ことを意味する。
    pub fn set_max_scrollback(&mut self, n: usize) {
        self.max_scrollback = if n == 0 { DEFAULT_MAX_SCROLLBACK } else { n };
        self.trim_scrollback();
    }

    pub fn max_scrollback(&self) -> usize {
        self.max_scrollback
    }

    /// 上限を超えた分を先頭から捨てる。
    fn trim_scrollback(&mut self) {
        let Some(excess) = self.scrollback.len().checked_sub(self.max_scrollback) else {
            return;
        };
        if excess == 0 {
            return;
        }
        self.scrollback.drain(..excess);
        self.dropped += excess;
    }

    /// 先頭から捨てた行数を受け取り、数え直しに戻す。
    ///
    /// 呼んだ側は**セマンティックマークを同じだけ寄せる責任を負う**
    /// （`TermState::feed` がやる）。取りっぱなしにすると印が別の行を指す。
    #[must_use]
    pub fn take_dropped(&mut self) -> usize {
        std::mem::take(&mut self.dropped)
    }

    pub fn is_alt(&self) -> bool {
        self.saved_primary.is_some()
    }

    // ---- ドキュメントとしての見え方 -------------------------------------

    /// スクロールバック + 現在画面の総行数。
    pub fn document_len(&self) -> usize {
        self.scrollback.len() + self.rows
    }

    /// ドキュメント絶対行番号での行取得。
    pub fn document_line(&self, index: usize) -> Option<&Line> {
        if index < self.scrollback.len() {
            self.scrollback.get(index)
        } else {
            self.screen.get(index - self.scrollback.len())
        }
    }

    /// 画面上の行番号 -> ドキュメント絶対行番号。
    pub fn absolute(&self, screen_row: usize) -> usize {
        self.scrollback.len() + screen_row
    }

    /// カーソルのドキュメント絶対行番号。
    pub fn cursor_absolute(&self) -> usize {
        self.absolute(self.cursor.row)
    }

    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    /// ドキュメント絶対行番号での SGR 付き行。
    pub fn line_ansi(&self, index: usize) -> Option<String> {
        self.document_line(index).map(Line::ansi)
    }

    /// ドキュメント全体のテキスト。末尾の空行は落とす。
    pub fn document_text(&self) -> String {
        let mut lines: Vec<String> = (0..self.document_len())
            .filter_map(|i| self.document_line(i).map(Line::text))
            .collect();
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    // ---- 消去に使う空白（背景色を引き継ぐ） -------------------------------

    fn blank_cell(&self) -> Cell {
        Cell::blank(self.pen.erased())
    }

    fn blank_line(&self) -> Line {
        Line::blank(self.cols, self.pen.erased())
    }

    // ---- 表示からの削除（`modal-spec.md` §7 の `d`） ---------------------
    //
    // プロセスには何もしない。**見えている文書から消すだけ**。
    // 再アタッチするとサーバの控えから戻るので、消えたままにはならない。

    /// ドキュメント絶対行の範囲を丸ごと取り除く。
    pub fn remove_document_lines(&mut self, from: usize, to: usize) {
        let last = to.min(self.document_len().saturating_sub(1));
        if from > last {
            return;
        }
        for line in (from..=last).rev() {
            if line < self.scrollback.len() {
                self.scrollback.remove(line);
            } else {
                let row = line - self.scrollback.len();
                if row < self.screen.len() {
                    self.screen.remove(row);
                    self.screen.push(Line::new(self.cols));
                }
            }
        }
    }

    /// ドキュメント絶対行の一部を空白にする。
    pub fn blank_document_cells(&mut self, line: usize, from: usize, to: usize) {
        let blank = self.blank_cell();
        let cols = self.cols;
        let Some(target) = self.document_line_mut(line) else {
            return;
        };
        for c in from..=to.min(cols.saturating_sub(1)) {
            if let Some(cell) = target.cells.get_mut(c) {
                *cell = blank.clone();
            }
        }
    }

    fn document_line_mut(&mut self, index: usize) -> Option<&mut Line> {
        if index < self.scrollback.len() {
            self.scrollback.get_mut(index)
        } else {
            self.screen.get_mut(index - self.scrollback.len())
        }
    }

    // ---- 文字の書き込み --------------------------------------------------

    pub fn print(&mut self, c: char) {
        let w = char_width(c, self.amb);

        // 結合文字・異体字セレクタ・ZWJ: 直前のセルへ連結する。
        // これを分けて置くと日本語のログでカーソル位置が恒常的にずれる。
        if w == 0 {
            self.attach_to_previous(c);
            return;
        }

        if self.cursor.col + w > self.cols {
            self.screen[self.cursor.row].wrapped = true;
            self.cursor.col = 0;
            self.line_feed();
        }

        let (row, col) = (self.cursor.row, self.cursor.col);
        let attrs = self.pen;
        self.screen[row].cells[col] = Cell {
            text: c.to_string(),
            width: w as u8,
            attrs,
        };
        if w == 2 && col + 1 < self.cols {
            self.screen[row].cells[col + 1] = Cell::spacer(attrs);
        }
        self.cursor.col += w;
    }

    fn attach_to_previous(&mut self, c: char) {
        let row = self.cursor.row;
        let mut col = self.cursor.col;
        // 直前の非スペーサセルまで戻る
        while col > 0 {
            col -= 1;
            if !self.screen[row].cells[col].is_spacer() {
                self.screen[row].cells[col].text.push(c);
                return;
            }
        }
        // 行頭で結合文字が来た場合は捨てる（前提となる基底文字が無い）
    }

    // ---- カーソル移動 ----------------------------------------------------

    pub fn carriage_return(&mut self) {
        self.cursor.col = 0;
    }

    pub fn line_feed(&mut self) {
        if self.cursor.row == self.scroll_bot {
            self.scroll_up(1);
        } else if self.cursor.row + 1 < self.rows {
            self.cursor.row += 1;
        }
    }

    /// 逆改行（ESC M）
    pub fn reverse_index(&mut self) {
        if self.cursor.row == self.scroll_top {
            self.scroll_down(1);
        } else {
            self.cursor.row = self.cursor.row.saturating_sub(1);
        }
    }

    pub fn backspace(&mut self) {
        self.cursor.col = self.cursor.col.saturating_sub(1);
    }

    pub fn tab(&mut self) {
        let next = ((self.cursor.col / 8) + 1) * 8;
        self.cursor.col = next.min(self.cols.saturating_sub(1));
    }

    pub fn move_to(&mut self, row: usize, col: usize) {
        self.cursor.row = row.min(self.rows.saturating_sub(1));
        self.cursor.col = col.min(self.cols.saturating_sub(1));
    }

    pub fn move_to_row(&mut self, row: usize) {
        self.cursor.row = row.min(self.rows.saturating_sub(1));
    }

    pub fn move_to_col(&mut self, col: usize) {
        self.cursor.col = col.min(self.cols.saturating_sub(1));
    }

    pub fn move_up(&mut self, n: usize) {
        self.cursor.row = self.cursor.row.saturating_sub(n);
    }

    pub fn move_down(&mut self, n: usize) {
        self.cursor.row = (self.cursor.row + n).min(self.rows.saturating_sub(1));
    }

    pub fn move_left(&mut self, n: usize) {
        self.cursor.col = self.cursor.col.saturating_sub(n);
    }

    pub fn move_right(&mut self, n: usize) {
        self.cursor.col = (self.cursor.col + n).min(self.cols.saturating_sub(1));
    }

    pub fn save_cursor(&mut self) {
        self.saved_cursor = self.cursor;
    }

    pub fn restore_cursor(&mut self) {
        self.cursor = self.saved_cursor;
        self.cursor.row = self.cursor.row.min(self.rows.saturating_sub(1));
        self.cursor.col = self.cursor.col.min(self.cols.saturating_sub(1));
    }

    pub fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        if top < bottom && bottom < self.rows {
            self.scroll_top = top;
            self.scroll_bot = bottom;
        } else {
            self.scroll_top = 0;
            self.scroll_bot = self.rows.saturating_sub(1);
        }
        self.cursor = Cursor::default();
    }

    // ---- スクロール ------------------------------------------------------

    pub fn scroll_up(&mut self, n: usize) {
        for _ in 0..n {
            let line = self.screen.remove(self.scroll_top);
            // スクロール領域が画面全体で、かつ primary のときだけ履歴に残す。
            // TUI アプリが領域を絞って回している行を履歴に混ぜてはいけない。
            let full_screen = self.scroll_top == 0 && self.scroll_bot + 1 == self.rows;
            if full_screen && !self.is_alt() {
                self.scrollback.push(line);
                self.trim_scrollback();
            }
            self.screen.insert(self.scroll_bot, self.blank_line());
        }
    }

    pub fn scroll_down(&mut self, n: usize) {
        for _ in 0..n {
            self.screen.remove(self.scroll_bot);
            self.screen.insert(self.scroll_top, self.blank_line());
        }
    }

    pub fn insert_lines(&mut self, n: usize) {
        if self.cursor.row < self.scroll_top || self.cursor.row > self.scroll_bot {
            return;
        }
        for _ in 0..n {
            self.screen.remove(self.scroll_bot);
            self.screen.insert(self.cursor.row, self.blank_line());
        }
    }

    pub fn delete_lines(&mut self, n: usize) {
        if self.cursor.row < self.scroll_top || self.cursor.row > self.scroll_bot {
            return;
        }
        for _ in 0..n {
            self.screen.remove(self.cursor.row);
            self.screen.insert(self.scroll_bot, self.blank_line());
        }
    }

    // ---- 消去 ------------------------------------------------------------

    /// ED: 0 = カーソルから末尾, 1 = 先頭からカーソル, 2 = 画面全体, 3 = スクロールバック
    pub fn erase_display(&mut self, mode: u16) {
        let (row, col) = (self.cursor.row, self.cursor.col);
        let (blank, blank_line) = (self.blank_cell(), self.blank_line());
        match mode {
            0 => {
                self.erase_line(0);
                for r in row + 1..self.rows {
                    self.screen[r] = blank_line.clone();
                }
            }
            1 => {
                for r in 0..row {
                    self.screen[r] = blank_line.clone();
                }
                for c in 0..=col.min(self.cols - 1) {
                    self.screen[row].cells[c] = blank.clone();
                }
            }
            2 => {
                for r in 0..self.rows {
                    self.screen[r] = blank_line.clone();
                }
            }
            3 => self.scrollback.clear(),
            _ => {}
        }
    }

    /// EL: 0 = カーソルから行末, 1 = 行頭からカーソル, 2 = 行全体
    pub fn erase_line(&mut self, mode: u16) {
        let (row, col) = (self.cursor.row, self.cursor.col);
        let range = match mode {
            0 => col..self.cols,
            1 => 0..(col + 1).min(self.cols),
            2 => 0..self.cols,
            _ => return,
        };
        let blank = self.blank_cell();
        for c in range {
            self.screen[row].cells[c] = blank.clone();
        }
    }

    pub fn erase_chars(&mut self, n: usize) {
        let (row, col) = (self.cursor.row, self.cursor.col);
        let blank = self.blank_cell();
        for c in col..(col + n).min(self.cols) {
            self.screen[row].cells[c] = blank.clone();
        }
    }

    pub fn delete_chars(&mut self, n: usize) {
        let (row, col) = (self.cursor.row, self.cursor.col);
        let blank = self.blank_cell();
        for _ in 0..n {
            if col < self.screen[row].cells.len() {
                self.screen[row].cells.remove(col);
                self.screen[row].cells.push(blank.clone());
            }
        }
    }

    pub fn insert_chars(&mut self, n: usize) {
        let (row, col) = (self.cursor.row, self.cursor.col);
        let blank = self.blank_cell();
        for _ in 0..n {
            if col < self.screen[row].cells.len() {
                self.screen[row].cells.insert(col, blank.clone());
                self.screen[row].cells.truncate(self.cols);
            }
        }
    }

    // ---- alt screen ------------------------------------------------------

    pub fn enter_alt(&mut self) {
        if self.is_alt() {
            return;
        }
        let blank = self.blank_line();
        let primary = std::mem::replace(
            &mut self.screen,
            (0..self.rows).map(|_| blank.clone()).collect(),
        );
        self.saved_primary = Some(primary);
        self.cursor = Cursor::default();
    }

    pub fn leave_alt(&mut self) {
        if let Some(primary) = self.saved_primary.take() {
            self.screen = primary;
        }
    }

    pub fn reset(&mut self) {
        *self = Grid::new(self.cols, self.rows, self.amb);
    }

    /// ウィンドウのリサイズに追従する。
    ///
    /// 論理行の折り返し再計算（reflow）は M1 の担当。ここでは行の伸縮と、
    /// 行数が減ったぶんを履歴へ送る処理だけを行う。
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if cols == self.cols && rows == self.rows {
            return;
        }

        for line in &mut self.screen {
            line.cells.resize(cols, Cell::default());
        }
        for line in &mut self.scrollback {
            line.cells.resize(cols, Cell::default());
        }
        if let Some(primary) = &mut self.saved_primary {
            for line in primary.iter_mut() {
                line.cells.resize(cols, Cell::default());
            }
            primary.resize_with(rows, || Line::new(cols));
        }

        // 行数の増減をどちらの端で吸収するかは、見た目を左右する。
        //
        // ConPTY は**カーソルより下の空行から先に捨てる**。ここで上端から捨てると、
        // 画面の中身が上へずれてスクロールバックへ流れ、直後に来る `CSI row;col H`
        // が空行を指す。実機では「プロンプトが消えて、打った文字だけが宙に浮く」
        // という形で出た。逆に広げるときは履歴から引き戻す（端末の慣例）。
        while self.screen.len() < rows {
            if !self.is_alt()
                && let Some(line) = self.scrollback.pop()
            {
                self.screen.insert(0, line);
                self.cursor.row += 1;
            } else {
                self.screen.push(Line::new(cols));
            }
        }
        while self.screen.len() > rows {
            let last = self.screen.len() - 1;
            if last > self.cursor.row && self.screen[last].is_blank() {
                self.screen.pop();
                continue;
            }
            let line = self.screen.remove(0);
            if !self.is_alt() {
                self.scrollback.push(line);
            }
            self.cursor.row = self.cursor.row.saturating_sub(1);
        }

        self.cols = cols;
        self.rows = rows;
        self.scroll_top = 0;
        self.scroll_bot = rows - 1;
        self.cursor.row = self.cursor.row.min(rows - 1);
        self.cursor.col = self.cursor.col.min(cols - 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TUI が枠を描く字は East Asian **Ambiguous**。ここを 2 幅で数えると、
    /// 枠線が破線になり、消去とカーソル移動の桁がずれて古い文字が残る。
    /// Claude Code を tsumugi の中で動かして実際に踏んだ。
    #[test]
    fn the_characters_tuis_draw_boxes_with_are_one_cell_wide() {
        for cp in [0x2500u32, 0x2502, 0x256d, 0x2588, 0x25cf, 0x2022, 0x00b7, 0x2190] {
            let c = char::from_u32(cp).unwrap();
            assert_eq!(char_width(c, AmbiguousWidth::Narrow), 1, "U+{cp:04X}");
            assert_eq!(char_width(c, AmbiguousWidth::Wide), 2, "U+{cp:04X}");
        }
        assert_eq!(ambiguous(), AmbiguousWidth::Narrow, "既定は 1 幅");
    }

    /// かな・漢字・全角英字は Wide クラスなので、この設定に左右されない。
    #[test]
    fn kana_and_kanji_stay_two_cells_either_way() {
        for cp in [0x3042u32, 0x6f22, 0xff21] {
            let c = char::from_u32(cp).unwrap();
            assert_eq!(char_width(c, AmbiguousWidth::Narrow), 2, "U+{cp:04X}");
            assert_eq!(char_width(c, AmbiguousWidth::Wide), 2, "U+{cp:04X}");
        }
    }

    #[test]
    fn ascii_is_one_cell() {
        let mut g = Grid::new(10, 3, AmbiguousWidth::Wide);
        for c in "abc".chars() {
            g.print(c);
        }
        assert_eq!(g.cursor.col, 3);
        assert_eq!(g.document_line(0).unwrap().text(), "abc");
    }

    #[test]
    fn cjk_takes_two_cells() {
        let mut g = Grid::new(10, 3, AmbiguousWidth::Wide);
        for c in "日本語".chars() {
            g.print(c);
        }
        assert_eq!(g.cursor.col, 6, "全角3文字は6セル");
        assert_eq!(g.document_line(0).unwrap().text(), "日本語");
    }

    #[test]
    fn ambiguous_width_follows_config() {
        // 「※」は East Asian Ambiguous
        let mut wide = Grid::new(10, 3, AmbiguousWidth::Wide);
        wide.print('※');
        assert_eq!(wide.cursor.col, 2);

        let mut narrow = Grid::new(10, 3, AmbiguousWidth::Narrow);
        narrow.print('※');
        assert_eq!(narrow.cursor.col, 1);
    }

    #[test]
    fn variation_selector_does_not_take_a_cell() {
        // 葛 + IVS(U+E0100)
        let mut g = Grid::new(10, 3, AmbiguousWidth::Wide);
        g.print('葛');
        g.print('\u{E0100}');
        assert_eq!(g.cursor.col, 2, "異体字セレクタはセルを消費しない");
        assert_eq!(g.document_line(0).unwrap().text(), "葛\u{E0100}");
    }

    #[test]
    fn combining_mark_attaches() {
        let mut g = Grid::new(10, 3, AmbiguousWidth::Wide);
        g.print('か');
        g.print('\u{3099}'); // 濁点（結合）
        assert_eq!(g.cursor.col, 2);
        assert_eq!(g.document_line(0).unwrap().text(), "か\u{3099}");
    }

    #[test]
    fn wide_char_wraps_instead_of_splitting() {
        let mut g = Grid::new(3, 3, AmbiguousWidth::Wide);
        g.print('a');
        g.print('日'); // 残り2セルだが col=1 なので 1+2 = 3 <= 3 で収まる
        assert_eq!(g.cursor.col, 3);
        g.print('本'); // 収まらない -> 折り返す
        assert_eq!(g.cursor.row, 1);
        assert_eq!(g.cursor.col, 2);
        assert!(g.document_line(0).unwrap().wrapped);
    }

    #[test]
    /// **上限が無いとメモリが尽きる。** 端末は他人が出したバイト列を
    /// 無制限に受け取る立場なので、ここは閉じておく。
    fn scrollback_stops_growing_at_the_limit() {
        let mut g = Grid::new(10, 2, AmbiguousWidth::Wide);
        g.set_max_scrollback(5);
        for _ in 0..100 {
            g.scroll_up(1);
        }
        assert_eq!(g.scrollback_len(), 5, "上限を超えて履歴が伸びている");
        assert_eq!(g.take_dropped(), 95, "捨てた行数が数えられていない");
        assert_eq!(g.take_dropped(), 0, "同じ分を二度数えている");
    }

    #[test]
    fn lowering_the_limit_trims_what_is_already_there() {
        let mut g = Grid::new(10, 2, AmbiguousWidth::Wide);
        for _ in 0..50 {
            g.scroll_up(1);
        }
        g.set_max_scrollback(10);
        assert_eq!(g.scrollback_len(), 10);
    }

    #[test]
    fn scrolling_pushes_into_scrollback() {
        let mut g = Grid::new(10, 2, AmbiguousWidth::Wide);
        for c in "one".chars() {
            g.print(c);
        }
        g.carriage_return();
        g.line_feed();
        for c in "two".chars() {
            g.print(c);
        }
        g.carriage_return();
        g.line_feed(); // ここでスクロール
        assert_eq!(g.scrollback_len(), 1);
        assert_eq!(g.document_line(0).unwrap().text(), "one");
        assert_eq!(g.document_len(), 3);
    }

    #[test]
    fn shrinking_drops_blank_rows_below_the_cursor_first() {
        // ConPTY は行数が減ったとき、カーソルより下の空行から捨てる。
        // 上端から捨てると中身が上へずれ、直後の `CSI row;col H` が空行を指す。
        // 実機で「プロンプトが消えて打った文字だけ浮く」形で出た回帰。
        let mut g = Grid::new(20, 10, AmbiguousWidth::Wide);
        for (i, text) in ["one", "two", "three"].iter().enumerate() {
            g.move_to(i, 0);
            for c in text.chars() {
                g.print(c);
            }
        }
        g.move_to(3, 0);

        g.resize(20, 5);
        assert_eq!(g.scrollback_len(), 0, "中身が履歴へ流れている");
        assert_eq!(g.document_line(0).unwrap().text(), "one");
        assert_eq!(g.document_line(2).unwrap().text(), "three");
        assert_eq!(g.cursor.row, 3, "カーソル行がずれている");
    }

    #[test]
    fn shrinking_past_the_content_still_scrolls_from_the_top() {
        let mut g = Grid::new(20, 4, AmbiguousWidth::Wide);
        for (i, text) in ["a", "b", "c", "d"].iter().enumerate() {
            g.move_to(i, 0);
            for c in text.chars() {
                g.print(c);
            }
        }
        g.move_to(3, 0);
        g.resize(20, 2);
        assert_eq!(g.scrollback_len(), 2, "捨てる空行が無ければ上から送る");
        assert_eq!(g.document_line(0).unwrap().text(), "a");
        assert_eq!(g.cursor.row, 1);
    }

    #[test]
    fn growing_pulls_lines_back_from_history() {
        let mut g = Grid::new(20, 2, AmbiguousWidth::Wide);
        for text in ["one", "two", "three"] {
            for c in text.chars() {
                g.print(c);
            }
            g.carriage_return();
            g.line_feed();
        }
        assert!(g.scrollback_len() > 0);
        let before = g.document_len();

        g.resize(20, 5);
        assert_eq!(g.scrollback_len(), 0, "履歴から引き戻していない");
        assert_eq!(g.document_len(), before.max(5));
        assert_eq!(g.document_line(0).unwrap().text(), "one");
    }

    #[test]
    fn removing_document_lines_takes_them_out_of_the_document() {
        let mut g = Grid::new(20, 3, AmbiguousWidth::Wide);
        for text in ["one", "two", "three", "four", "five"] {
            for c in text.chars() {
                g.print(c);
            }
            g.carriage_return();
            g.line_feed();
        }
        let before = g.document_len();
        // 履歴に落ちた行と画面上の行をまたいで消す
        g.remove_document_lines(1, 2);
        assert!(g.document_len() <= before, "消したのに増えている");
        assert_eq!(g.rows, 3, "画面の行数は変えない");
        let text = g.document_text();
        assert!(!text.contains("two"), "消えていない: {text:?}");
        assert!(!text.contains("three"), "消えていない: {text:?}");
        assert!(text.contains("one") && text.contains("four"));
    }

    #[test]
    fn blanking_cells_leaves_the_rest_of_the_line() {
        let mut g = Grid::new(20, 2, AmbiguousWidth::Wide);
        for c in "abcdefgh".chars() {
            g.print(c);
        }
        g.blank_document_cells(0, 2, 4);
        assert_eq!(g.document_line(0).unwrap().text(), "ab   fgh");
    }

    #[test]
    fn alt_screen_does_not_pollute_scrollback() {
        let mut g = Grid::new(10, 2, AmbiguousWidth::Wide);
        g.enter_alt();
        for _ in 0..5 {
            g.line_feed();
        }
        assert_eq!(g.scrollback_len(), 0, "alt screen の行は履歴に残さない");
        g.leave_alt();
        assert!(!g.is_alt());
    }
}
