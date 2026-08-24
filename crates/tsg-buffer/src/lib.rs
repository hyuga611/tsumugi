//! `Buffer` — ターミナルのグリッドとファイルバッファを同じ座標系に載せる層。
//!
//! `concept.md` の中心命題「グリッドはドキュメントである」を型にしたもの。
//! `tsg-modal` のモーション・テキストオブジェクト・オペレータは**このトレイトの上にだけ**書く。
//! Term 用と File 用を二重実装しないことが、`arch.md` の言う
//! 「エディタ内蔵が実質タダになる」の中身である。
//!
//! 座標は **ドキュメント絶対行 × セル列**。バイト位置ではない。
//! ターミナルでは全角が 2 セルを占めるため、バイトや文字数で数えると
//! 画面上の位置と一致しなくなる。

pub mod file;
pub mod syntax;

use tsg_term::{Cell, Grid, SemanticMarks};

pub use file::{FileBuffer, Splice};
pub use syntax::{Lang, Token, highlight};

pub use tsg_term::{CommandBlock, Cell as BufferCell, MarkKind, SemanticMarks as Marks};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferKind {
    TermPrimary,
    TermAlt,
    File,
}

/// `modal-spec.md` §7 のオペレータ。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OperatorId {
    Yank,
    Delete,
    Change,
    /// `!` 範囲をプロンプトへ送る
    SendToPrompt,
    /// `>` 範囲を外部コマンドへパイプ
    Pipe,
    /// `=` 整形
    Format,
}

/// ドキュメント絶対行 × セル列。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

impl Pos {
    pub fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeKind {
    Char,
    Line,
    Block,
}

/// 両端を含む範囲。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub start: Pos,
    pub end: Pos,
    pub kind: RangeKind,
}

impl Range {
    pub fn new(a: Pos, b: Pos, kind: RangeKind) -> Self {
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        Self { start, end, kind }
    }
}

// ---------------------------------------------------------------------------

pub trait Buffer {
    fn kind(&self) -> BufferKind;

    fn line_count(&self) -> usize;

    /// 表示セルの並びとしての1行。範囲外なら `None`。
    fn cells(&self, line: usize) -> Option<&[Cell]>;

    /// OSC 133 由来のセマンティックマーク。File バッファでは空。
    fn marks(&self) -> &SemanticMarks;

    /// `modal-spec.md` §7 の可否表。
    fn allows(&self, op: OperatorId) -> bool {
        use BufferKind::*;
        use OperatorId::*;
        match (self.kind(), op) {
            (_, Yank | SendToPrompt | Pipe) => true,
            (TermAlt, _) => false,
            (TermPrimary, Delete | Format) => true,
            (TermPrimary, Change) => false,
            (File, _) => true,
        }
    }
}

// ---- トレイトの上に載るヘルパ ---------------------------------------------
//
// ここに集めておくことで、モーション実装が `cells()` の細部
// （スペーサセル・末尾空白）を毎回意識しなくて済む。

/// 1 文字の表示幅。`tsg-modal` が `tsg-term` を直接見ないための橋。
pub fn char_display_width(c: char) -> usize {
    tsg_term::char_width(c, tsg_term::AmbiguousWidth::Wide)
}

/// その行のセル数（確保された幅）。
pub fn line_width(buf: &dyn Buffer, line: usize) -> usize {
    buf.cells(line).map_or(0, <[Cell]>::len)
}

/// 末尾の空白を除いた「中身のある最後の列」。空行なら 0。
pub fn last_col(buf: &dyn Buffer, line: usize) -> usize {
    let Some(cells) = buf.cells(line) else {
        return 0;
    };
    for (i, cell) in cells.iter().enumerate().rev() {
        if !is_blank_cell(cell) {
            return i;
        }
    }
    0
}

/// 中身のある最初の列（`^` の行き先）。
pub fn first_non_blank(buf: &dyn Buffer, line: usize) -> usize {
    let Some(cells) = buf.cells(line) else {
        return 0;
    };
    cells.iter().position(|c| !is_blank_cell(c)).unwrap_or(0)
}

pub fn is_blank_line(buf: &dyn Buffer, line: usize) -> bool {
    buf.cells(line)
        .is_none_or(|cells| cells.iter().all(is_blank_cell))
}

fn is_blank_cell(cell: &Cell) -> bool {
    cell.width == 0 || cell.text.trim().is_empty()
}

/// その列の書記素クラスタ。全角の右半分（スペーサ）では左半分の内容を返す。
pub fn cell_text<'a>(buf: &'a dyn Buffer, line: usize, col: usize) -> &'a str {
    let Some(cells) = buf.cells(line) else {
        return "";
    };
    let mut c = col.min(cells.len().saturating_sub(1));
    while c > 0 && cells.get(c).is_some_and(|cell| cell.width == 0) {
        c -= 1;
    }
    cells.get(c).map_or("", |cell| cell.text.as_str())
}

pub fn char_at(buf: &dyn Buffer, line: usize, col: usize) -> Option<char> {
    cell_text(buf, line, col).chars().next()
}

/// 全角の右半分に着地しないよう、セルの先頭へ寄せる。
pub fn snap_to_cell_start(buf: &dyn Buffer, pos: Pos) -> Pos {
    let Some(cells) = buf.cells(pos.line) else {
        return pos;
    };
    let mut col = pos.col.min(cells.len().saturating_sub(1));
    while col > 0 && cells.get(col).is_some_and(|c| c.width == 0) {
        col -= 1;
    }
    Pos::new(pos.line, col)
}

/// 行数・列数の範囲に収める。
pub fn clamp(buf: &dyn Buffer, pos: Pos) -> Pos {
    let lines = buf.line_count();
    if lines == 0 {
        return Pos::default();
    }
    let line = pos.line.min(lines - 1);
    let max_col = last_col(buf, line);
    snap_to_cell_start(buf, Pos::new(line, pos.col.min(max_col)))
}

/// 入力モードの収め方。**行末の 1 つ先に立てる。**
///
/// 通常モードのカーソルは「文字の上」に居るので最後の文字までしか行けない。
/// 入力モードのカーソルは「文字と文字の間」に居るので、行末の後ろが要る。
/// 同じ `clamp` を使い回すと、行末に 1 文字打った直後にカーソルが引き戻され、
/// 次の文字がその手前へ入る（実機で `alpha-EDIT` が `alphaEDIT-` になった）。
pub fn clamp_insert(buf: &dyn Buffer, pos: Pos) -> Pos {
    let lines = buf.line_count();
    if lines == 0 {
        return Pos::default();
    }
    let line = pos.line.min(lines - 1);
    let width = line_width(buf, line);
    // 行末の 1 つ先はセルが無い。`snap_to_cell_start` は「あるセルの先頭」へ
    // 寄せるので、ここを通すと必ず最後の文字の上へ引き戻される。
    if pos.col >= width {
        return Pos::new(line, width);
    }
    snap_to_cell_start(buf, Pos::new(line, pos.col))
}

/// 次のセル先頭の列（全角なら 2 進む）。行末を超えない。
pub fn next_col(buf: &dyn Buffer, line: usize, col: usize) -> Option<usize> {
    let cells = buf.cells(line)?;
    let step = cells.get(col).map_or(1, |c| c.width.max(1) as usize);
    let next = col + step;
    (next < cells.len()).then_some(next)
}

/// 前のセル先頭の列。
pub fn prev_col(buf: &dyn Buffer, line: usize, col: usize) -> Option<usize> {
    if col == 0 {
        return None;
    }
    let cells = buf.cells(line)?;
    let mut c = col - 1;
    while c > 0 && cells.get(c).is_some_and(|cell| cell.width == 0) {
        c -= 1;
    }
    Some(c)
}

/// 行のテキスト（末尾の空白は落とす）。
pub fn line_text(buf: &dyn Buffer, line: usize) -> String {
    let Some(cells) = buf.cells(line) else {
        return String::new();
    };
    let mut s = String::new();
    for cell in cells {
        if cell.width != 0 {
            s.push_str(&cell.text);
        }
    }
    s.trim_end().to_string()
}

/// 行の一部（両端を含む列範囲）。
fn line_text_range(buf: &dyn Buffer, line: usize, from: usize, to: usize) -> String {
    let Some(cells) = buf.cells(line) else {
        return String::new();
    };
    let to = to.min(cells.len().saturating_sub(1));
    let mut s = String::new();
    for cell in cells.iter().take(to + 1).skip(from) {
        if cell.width != 0 {
            s.push_str(&cell.text);
        }
    }
    s
}

/// 範囲のテキストを取り出す（ヤンクの実体）。
pub fn extract(buf: &dyn Buffer, range: &Range) -> String {
    let (start, end) = (range.start, range.end);
    match range.kind {
        RangeKind::Line => {
            let mut out = String::new();
            for line in start.line..=end.line.min(buf.line_count().saturating_sub(1)) {
                out.push_str(&line_text(buf, line));
                out.push('\n');
            }
            out
        }
        RangeKind::Block => {
            let (from, to) = if start.col <= end.col {
                (start.col, end.col)
            } else {
                (end.col, start.col)
            };
            let mut out = String::new();
            for line in start.line..=end.line.min(buf.line_count().saturating_sub(1)) {
                out.push_str(line_text_range(buf, line, from, to).trim_end());
                out.push('\n');
            }
            out
        }
        RangeKind::Char => {
            if start.line == end.line {
                return line_text_range(buf, start.line, start.col, end.col);
            }
            let mut out = line_text_range(buf, start.line, start.col, usize::MAX)
                .trim_end()
                .to_string();
            out.push('\n');
            for line in (start.line + 1)..end.line {
                out.push_str(&line_text(buf, line));
                out.push('\n');
            }
            if end.line < buf.line_count() {
                out.push_str(&line_text_range(buf, end.line, 0, end.col));
            }
            out
        }
    }
}

// ---- ターミナルグリッドの実装 ---------------------------------------------

/// `tsg-term` の `Grid` を `Buffer` として見せる（借用ビュー）。
pub struct TermBuffer<'a> {
    grid: &'a Grid,
    marks: &'a SemanticMarks,
}

impl<'a> TermBuffer<'a> {
    pub fn new(grid: &'a Grid, marks: &'a SemanticMarks) -> Self {
        Self { grid, marks }
    }
}

impl Buffer for TermBuffer<'_> {
    fn kind(&self) -> BufferKind {
        if self.grid.is_alt() {
            BufferKind::TermAlt
        } else {
            BufferKind::TermPrimary
        }
    }

    fn line_count(&self) -> usize {
        self.grid.document_len()
    }

    fn cells(&self, line: usize) -> Option<&[Cell]> {
        self.grid.document_line(line).map(|l| l.cells.as_slice())
    }

    fn marks(&self) -> &SemanticMarks {
        self.marks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsg_term::{AmbiguousWidth, Terminal};

    fn term_with(text: &str) -> Terminal {
        let mut t = Terminal::new(40, 6, AmbiguousWidth::Wide);
        t.feed(text.as_bytes());
        t
    }

    #[test]
    fn reads_lines_as_cells() {
        let t = term_with("hello\r\nworld\r\n");
        let buf = TermBuffer::new(&t.state.grid, &t.state.marks);
        assert_eq!(line_text(&buf, 0), "hello");
        assert_eq!(line_text(&buf, 1), "world");
        assert_eq!(last_col(&buf, 0), 4);
    }

    #[test]
    fn wide_chars_occupy_two_columns() {
        let t = term_with("a日b");
        let buf = TermBuffer::new(&t.state.grid, &t.state.marks);
        // a=0, 日=1(2セル), b=3
        assert_eq!(next_col(&buf, 0, 0), Some(1));
        assert_eq!(next_col(&buf, 0, 1), Some(3), "全角は 2 列進む");
        assert_eq!(prev_col(&buf, 0, 3), Some(1), "戻るときも全角の先頭へ");
        assert_eq!(cell_text(&buf, 0, 1), "日");
        assert_eq!(
            cell_text(&buf, 0, 2),
            "日",
            "全角の右半分は左半分の内容を返す"
        );
    }

    #[test]
    fn snapping_avoids_landing_on_a_spacer() {
        let t = term_with("a日b");
        let buf = TermBuffer::new(&t.state.grid, &t.state.marks);
        assert_eq!(snap_to_cell_start(&buf, Pos::new(0, 2)), Pos::new(0, 1));
    }

    #[test]
    fn extracts_charwise_across_lines() {
        let t = term_with("abcdef\r\nghijkl\r\n");
        let buf = TermBuffer::new(&t.state.grid, &t.state.marks);
        let r = Range::new(Pos::new(0, 2), Pos::new(1, 2), RangeKind::Char);
        assert_eq!(extract(&buf, &r), "cdef\nghi");
    }

    #[test]
    fn extracts_linewise() {
        let t = term_with("one\r\ntwo\r\nthree\r\n");
        let buf = TermBuffer::new(&t.state.grid, &t.state.marks);
        let r = Range::new(Pos::new(0, 0), Pos::new(1, 0), RangeKind::Line);
        assert_eq!(extract(&buf, &r), "one\ntwo\n");
    }

    #[test]
    fn extracts_blockwise() {
        let t = term_with("abcdef\r\nghijkl\r\nmnopqr\r\n");
        let buf = TermBuffer::new(&t.state.grid, &t.state.marks);
        let r = Range::new(Pos::new(0, 1), Pos::new(2, 3), RangeKind::Block);
        assert_eq!(extract(&buf, &r), "bcd\nhij\nnop\n");
    }

    #[test]
    fn operator_table_follows_buffer_kind() {
        let t = term_with("hi");
        let buf = TermBuffer::new(&t.state.grid, &t.state.marks);
        assert!(buf.allows(OperatorId::Yank));
        assert!(buf.allows(OperatorId::Delete), "primary では d が使える");
        assert!(!buf.allows(OperatorId::Change), "Term では c は使えない");

        let mut t2 = term_with("hi");
        t2.feed(b"\x1b[?1049h");
        let alt = TermBuffer::new(&t2.state.grid, &t2.state.marks);
        assert!(alt.allows(OperatorId::Yank));
        assert!(!alt.allows(OperatorId::Delete), "alt は読み取り専用");
    }
}
