//! ファイルバッファ。`arch.md` §9 の M4「内蔵エディタ」の実体。
//!
//! `Buffer` トレイトを実装するので、`tsg-modal` のモーションもテキスト
//! オブジェクトもオペレータも**そのまま効く**。これが `concept.md` の言う
//! 「エディタ内蔵が実質タダになる」の中身で、ここに専用のモーション実装は無い。
//!
//! ⚠️ `arch.md` は ropey を挙げていたが、行を `Vec<Cell>` として持つ形にした。
//! `Buffer::cells()` がセルの連なりへの参照を返す設計なので、rope を持つと
//! セル配列のキャッシュを別に抱えることになり、二重管理が増えるだけで
//! 数千行の規模では速度差が出ない。大きなファイルを扱う段で入れ替える。

use std::path::{Path, PathBuf};

use tsg_term::{AmbiguousWidth, Cell, SemanticMarks, char_width};

use crate::{Buffer, BufferKind, Pos, Range, RangeKind};

pub struct FileBuffer {
    pub path: Option<PathBuf>,
    lines: Vec<Vec<Cell>>,
    /// 読み込み時に改行が CRLF だったか。保存で勝手に変えない。
    crlf: bool,
    /// 末尾に改行があったか。これも保存で勝手に足さない・消さない。
    trailing_newline: bool,
    pub dirty: bool,
    amb: AmbiguousWidth,
    marks: SemanticMarks,

    undo: Vec<Group>,
    redo: Vec<Group>,
    /// 次の変更が新しい取り消し単位を始めるか。
    new_group: bool,
    group_cursor: Pos,
    /// まだホストへ渡していない変更。サーバへはこれだけを送る。
    pending: Vec<Splice>,
    /// 差分ではなく全文を渡し直す必要がある（取り消し / やり直しの後）。
    resync: bool,
}

/// 変更 1 つ分の置換。位置は `text()` が返す文字列の上のバイト。
///
/// 変更の種類（文字・行・矩形）ごとに逆操作を書き分けると、境界の扱いを
/// 1 つ間違えたときに「取り消したら壊れた」が起きる。そこで**どの経路も
/// 変更前後の差分を取って 1 つの置換にまとめる**。形が 1 種類しか無いので、
/// 取り消しもサーバへの送信も同じものを使い回せる。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Splice {
    pub start: usize,
    pub removed: String,
    pub inserted: String,
}

impl Splice {
    fn cost(&self) -> usize {
        self.removed.len() + self.inserted.len()
    }
}

/// 取り消し 1 段分。中の置換を逆順に戻すと元へ帰る。
#[derive(Clone, Default)]
struct Group {
    splices: Vec<Splice>,
    dirty: bool,
    /// 変更を始めたときのカーソル。戻したときにそこへ戻す。
    cursor: Pos,
}

impl Group {
    fn cost(&self) -> usize {
        self.splices.iter().map(Splice::cost).sum()
    }
}

/// 取り消し履歴に使ってよい総バイト数と段数。
///
/// 全文ではなく差分を積むので、同じ上限でも桁違いに深く戻れる。
const UNDO_BYTES: usize = 4 * 1024 * 1024;
const UNDO_DEPTH: usize = 500;

impl FileBuffer {
    pub fn from_text(text: &str, amb: AmbiguousWidth) -> Self {
        let crlf = text.contains("\r\n");
        let trailing_newline = text.ends_with('\n');
        let body = text.strip_suffix('\n').unwrap_or(text);
        let body = body.strip_suffix('\r').unwrap_or(body);
        let lines: Vec<Vec<Cell>> = body
            .split('\n')
            .map(|l| cells_of(l.strip_suffix('\r').unwrap_or(l), amb))
            .collect();
        Self {
            path: None,
            lines,
            crlf,
            trailing_newline,
            dirty: false,
            amb,
            marks: SemanticMarks::default(),
            undo: Vec::new(),
            redo: Vec::new(),
            new_group: true,
            group_cursor: Pos::default(),
            pending: Vec::new(),
            resync: false,
        }
    }

    pub fn open(path: impl AsRef<Path>, amb: AmbiguousWidth) -> std::io::Result<Self> {
        let path = path.as_ref();
        // 無いファイルは「新規」として開く。開けないほうが不便。
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let mut buf = Self::from_text(&text, amb);
        buf.path = Some(path.to_path_buf());
        Ok(buf)
    }

    pub fn save(&mut self) -> std::io::Result<PathBuf> {
        let Some(path) = self.path.clone() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "保存先が決まっていません",
            ));
        };
        std::fs::write(&path, self.text())?;
        self.dirty = false;
        Ok(path)
    }

    /// 保存する形のテキスト。読み込んだときの改行の作法を保つ。
    pub fn text(&self) -> String {
        let sep = if self.crlf { "\r\n" } else { "\n" };
        let mut out = self
            .lines
            .iter()
            .map(|l| line_string(l))
            .collect::<Vec<_>>()
            .join(sep);
        if self.trailing_newline {
            out.push_str(sep);
        }
        out
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// 行の中身（文字列）。
    pub fn line(&self, index: usize) -> String {
        self.lines
            .get(index)
            .map(|l| line_string(l))
            .unwrap_or_default()
    }

    // 全角の右半分（スペーサ）を入れてあるので、**添字と列は一致する**。
    // それでも「セルの途中」に着地することはあるので、境界へ寄せる関数を通す。

    /// `col` を含むセルの先頭添字。
    fn start_index(&self, line: usize, col: usize) -> usize {
        let Some(cells) = self.lines.get(line) else {
            return 0;
        };
        let mut i = col.min(cells.len());
        while i > 0 && cells.get(i).is_some_and(|c| c.width == 0) {
            i -= 1;
        }
        i
    }

    /// `col` を含むセルの**次**の添字（そのセルを含めて消すときの右端）。
    fn end_index(&self, line: usize, col: usize) -> usize {
        let Some(cells) = self.lines.get(line) else {
            return 0;
        };
        let mut i = self.start_index(line, col);
        if let Some(cell) = cells.get(i) {
            i += usize::from(cell.width.max(1));
        }
        i.min(cells.len())
    }

    /// 文字を差し込む。改行を含んでよい。返すのは差し込んだ後のカーソル位置。
    pub fn insert(&mut self, at: Pos, text: &str) -> Pos {
        self.checkpoint();
        let before = self.text();
        if self.lines.is_empty() {
            self.lines.push(Vec::new());
        }
        let line = at.line.min(self.lines.len() - 1);
        let idx = self.start_index(line, at.col);

        let tail: Vec<Cell> = self.lines[line].split_off(idx);
        let mut pieces = text.split('\n').peekable();
        let mut cur = line;

        while let Some(piece) = pieces.next() {
            let piece = piece.strip_suffix('\r').unwrap_or(piece);
            self.lines[cur].extend(cells_of(piece, self.amb));
            if pieces.peek().is_some() {
                cur += 1;
                self.lines.insert(cur, Vec::new());
            }
        }
        let end_index = self.lines[cur].len();
        self.lines[cur].extend(tail);

        self.dirty = true;
        self.note_change(&before);
        Pos::new(cur, end_index)
    }

    /// 範囲を消す。消したテキストを返す（ヤンクへ渡すため）。
    pub fn delete(&mut self, range: &Range) -> String {
        self.checkpoint();
        let before = self.text();
        let removed = crate::extract(self, range);
        match range.kind {
            RangeKind::Line => {
                let last = range.end.line.min(self.lines.len().saturating_sub(1));
                if range.start.line <= last {
                    self.lines.drain(range.start.line..=last);
                }
                if self.lines.is_empty() {
                    self.lines.push(Vec::new());
                }
            }
            RangeKind::Block => {
                let last = range.end.line.min(self.lines.len().saturating_sub(1));
                let (from, to) = (
                    range.start.col.min(range.end.col),
                    range.start.col.max(range.end.col),
                );
                for line in range.start.line..=last {
                    let a = self.start_index(line, from);
                    let b = self.end_index(line, to);
                    if a < b && b <= self.lines[line].len() {
                        self.lines[line].drain(a..b);
                    }
                }
            }
            RangeKind::Char => {
                let last = range.end.line.min(self.lines.len().saturating_sub(1));
                let a = self.start_index(range.start.line, range.start.col);
                let b = self.end_index(last, range.end.col);
                if range.start.line == last {
                    if a < b && b <= self.lines[last].len() {
                        self.lines[last].drain(a..b);
                    }
                } else {
                    let cut = b.min(self.lines[last].len());
                    let tail: Vec<Cell> = self.lines[last].split_off(cut);
                    self.lines[range.start.line].truncate(a);
                    self.lines[range.start.line].extend(tail);
                    self.lines.drain(range.start.line + 1..=last);
                }
            }
        }
        self.dirty = true;
        self.note_change(&before);
        removed
    }

    /// 範囲を `text` で置き換える。消した分を返す。
    ///
    /// **行単位の範囲は行ごと入れ替える。** 消してから同じ位置へ差し込むと、
    /// 置き換えたテキストの最終行が次の行の頭にくっつく
    /// （`=` で JSON を整形したとき、閉じ括弧が次の行と繋がって出た）。
    pub fn replace(&mut self, range: &Range, text: &str) -> String {
        // delete と insert で 2 段にならないよう、単位はここで閉じる
        self.checkpoint();
        let removed = self.delete(range);
        if text.is_empty() {
            return removed;
        }
        if range.kind != RangeKind::Line {
            self.insert(range.start, text);
            return removed;
        }

        // `delete` が置いた「空の 1 行」は入れ替えの跡なので使い回す
        let before = self.text();
        if self.lines.len() == 1 && self.lines[0].is_empty() {
            self.lines.clear();
        }
        let at = range.start.line.min(self.lines.len());
        for (i, line) in text.trim_end_matches('\n').split('\n').enumerate() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            self.lines.insert(at + i, cells_of(line, self.amb));
        }
        self.dirty = true;
        self.note_change(&before);
        removed
    }

    // ---- 取り消し ---------------------------------------------------------

    /// 次の変更から新しい取り消し単位を始める。
    ///
    /// 入力モードに入るとき 1 回、オペレータの適用ごとに 1 回、ホストが呼ぶ。
    /// これが無いと 1 打鍵ずつ取り消すことになって使い物にならない。
    pub fn begin_group(&mut self, cursor: Pos) {
        self.new_group = true;
        self.group_cursor = cursor;
    }

    /// 変更の直前に呼ぶ。同じ単位の中なら何もしない。
    fn checkpoint(&mut self) {
        if !self.new_group {
            return;
        }
        self.new_group = false;
        // 新しい変更が入ったらやり直しの先は捨てる（枝分かれを持たない）
        self.redo.clear();
        self.undo.push(Group {
            splices: Vec::new(),
            dirty: self.dirty,
            cursor: self.group_cursor,
        });
        trim(&mut self.undo);
    }

    /// 変更を記録する。**すべての変更経路がここを通る。**
    ///
    /// 変更前の全文を渡してもらい、前後の共通部分を落として 1 つの置換にする。
    /// 経路ごとに逆操作を書かないので、行単位・矩形・改行の作法の違いが
    /// 取り消しの正しさに影響しない。
    fn note_change(&mut self, before: &str) {
        let after = self.text();
        let Some(splice) = diff_span(before, &after) else {
            return;
        };
        self.pending.push(splice.clone());
        if self.undo.is_empty() {
            self.undo.push(Group {
                splices: Vec::new(),
                dirty: self.dirty,
                cursor: self.group_cursor,
            });
        }
        if let Some(g) = self.undo.last_mut() {
            g.splices.push(splice);
        }
        trim(&mut self.undo);
    }

    /// まだ渡していない変更を取り出す。ホストがサーバへ送るのに使う。
    ///
    /// **これを送らないとウィンドウを閉じた時点で編集が消える。**
    pub fn take_splices(&mut self) -> Vec<Splice> {
        std::mem::take(&mut self.pending)
    }

    /// 1 段戻す。戻ったらカーソルの行き先を返す。
    pub fn undo(&mut self) -> Option<Pos> {
        let group = self.undo.pop()?;
        let mut text = self.text();
        // 後ろに積んだものから戻す。前から戻すと 2 つ目以降の位置がずれる。
        for s in group.splices.iter().rev() {
            let end = (s.start + s.inserted.len()).min(text.len());
            if s.start <= end {
                text.replace_range(s.start..end, &s.removed);
            }
        }
        let dirty_now = self.dirty;
        self.load_text(&text, group.dirty);
        self.redo.push(Group {
            dirty: dirty_now,
            ..group.clone()
        });
        self.pending.clear();
        self.resync = true;
        Some(self.clamp(group.cursor))
    }

    /// 1 段やり直す。
    pub fn redo(&mut self) -> Option<Pos> {
        let group = self.redo.pop()?;
        let mut text = self.text();
        for s in &group.splices {
            let end = (s.start + s.removed.len()).min(text.len());
            if s.start <= end {
                text.replace_range(s.start..end, &s.inserted);
            }
        }
        let dirty_now = self.dirty;
        self.load_text(&text, true);
        self.undo.push(Group {
            dirty: dirty_now,
            ..group.clone()
        });
        self.pending.clear();
        self.resync = true;
        Some(self.clamp(group.cursor))
    }

    /// 全文を入れ替える。取り消し / やり直しの適用はこれ。
    fn load_text(&mut self, text: &str, dirty: bool) {
        let rebuilt = Self::from_text(text, self.amb);
        self.lines = rebuilt.lines;
        self.crlf = rebuilt.crlf;
        self.trailing_newline = rebuilt.trailing_newline;
        self.dirty = dirty;
        // 戻した直後の変更は新しい単位から
        self.new_group = true;
    }

    /// 差分ではなく全文を渡し直す必要があるか（取り消しの後）。
    ///
    /// 取り消しは何段でも一度に動くので、位置つきの差分へ落とすより
    /// **全文を 1 度渡すほうが確実**。頻度が低いので実害も無い。
    pub fn take_resync(&mut self) -> bool {
        std::mem::take(&mut self.resync)
    }

    /// カーソルを文書の中へ収める。
    pub fn clamp(&self, pos: Pos) -> Pos {
        crate::clamp(self, pos)
    }
}

impl Buffer for FileBuffer {
    fn kind(&self) -> BufferKind {
        BufferKind::File
    }

    fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn cells(&self, line: usize) -> Option<&[Cell]> {
        self.lines.get(line).map(Vec::as_slice)
    }

    fn marks(&self) -> &SemanticMarks {
        &self.marks
    }
}

/// 履歴を上限内へ収める。古いものから捨てる。
fn trim(stack: &mut Vec<Group>) {
    while stack.len() > UNDO_DEPTH {
        stack.remove(0);
    }
    let mut total: usize = stack.iter().map(Group::cost).sum();
    while total > UNDO_BYTES && stack.len() > 1 {
        total -= stack.remove(0).cost();
    }
}

/// 2 つの文字列の違いを 1 つの置換にまとめる。
///
/// 前後の共通部分を落とすので、1 打鍵の変更は数バイトの記録で済む。
/// 文字境界へ寄せるのは、全角の途中で切ると `String` を組み直せないため。
fn diff_span(before: &str, after: &str) -> Option<Splice> {
    if before == after {
        return None;
    }
    let (a, b) = (before.as_bytes(), after.as_bytes());

    let mut head = 0;
    while head < a.len() && head < b.len() && a[head] == b[head] {
        head += 1;
    }
    while head > 0 && !(before.is_char_boundary(head) && after.is_char_boundary(head)) {
        head -= 1;
    }

    let mut tail = 0;
    while tail < a.len() - head
        && tail < b.len() - head
        && a[a.len() - 1 - tail] == b[b.len() - 1 - tail]
    {
        tail += 1;
    }
    while tail > 0
        && !(before.is_char_boundary(a.len() - tail) && after.is_char_boundary(b.len() - tail))
    {
        tail -= 1;
    }

    Some(Splice {
        start: head,
        removed: before[head..a.len() - tail].to_string(),
        inserted: after[head..b.len() - tail].to_string(),
    })
}

fn cells_of(s: &str, amb: AmbiguousWidth) -> Vec<Cell> {
    let mut out: Vec<Cell> = Vec::new();
    for c in s.chars() {
        let w = char_width(c, amb);
        if w == 0 {
            // 結合文字は直前へ足す。単独で置くと列がずれる（グリッドと同じ規則）。
            if let Some(last) = out.iter_mut().rev().find(|c| c.width > 0) {
                last.text.push(c);
            }
            continue;
        }
        out.push(Cell {
            text: c.to_string(),
            width: w as u8,
            attrs: tsg_term::Attrs::default(),
        });
        // 全角の右半分。グリッドと同じ規則にしないと、添字と列が 1 つずつずれて
        // 文字が重なって描かれる（実機で踏んだ）。
        for _ in 1..w {
            out.push(Cell::spacer(tsg_term::Attrs::default()));
        }
    }
    out
}

fn line_string(cells: &[Cell]) -> String {
    cells
        .iter()
        .filter(|c| c.width > 0)
        .map(|c| c.text.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(text: &str) -> FileBuffer {
        FileBuffer::from_text(text, AmbiguousWidth::Wide)
    }

    #[test]
    fn text_round_trips_including_the_line_ending_style() {
        for src in ["a\nb\n", "a\nb", "a\r\nb\r\n", "", "\n"] {
            let b = buf(src);
            assert_eq!(b.text(), src, "改行の作法が変わっている: {src:?}");
        }
    }

    #[test]
    fn a_missing_file_opens_as_an_empty_new_one() {
        let p = std::env::temp_dir().join("tsumugi-does-not-exist-1234.txt");
        let _ = std::fs::remove_file(&p);
        let b = FileBuffer::open(&p, AmbiguousWidth::Wide).expect("新規として開けない");
        assert_eq!(b.line_count(), 1);
        assert!(!b.dirty);
    }

    #[test]
    fn inserting_text_and_newlines() {
        let mut b = buf("hello world");
        let at = b.insert(Pos::new(0, 5), ",");
        assert_eq!(b.line(0), "hello, world");
        assert_eq!(at, Pos::new(0, 6));

        let at = b.insert(Pos::new(0, 6), "\n  ");
        assert_eq!(b.line(0), "hello,");
        assert_eq!(b.line(1), "   world");
        assert_eq!(at, Pos::new(1, 2));
        assert!(b.dirty);
    }

    #[test]
    fn deleting_a_charwise_range_inside_one_line() {
        let mut b = buf("abcdefgh");
        let got = b.delete(&Range::new(Pos::new(0, 2), Pos::new(0, 4), RangeKind::Char));
        assert_eq!(got, "cde");
        assert_eq!(b.line(0), "abfgh");
    }

    #[test]
    fn deleting_across_lines_joins_the_ends() {
        let mut b = buf("one\ntwo\nthree");
        b.delete(&Range::new(Pos::new(0, 1), Pos::new(2, 1), RangeKind::Char));
        assert_eq!(b.line_count(), 1);
        assert_eq!(b.line(0), "oree");
    }

    #[test]
    fn deleting_lines_removes_them_whole() {
        let mut b = buf("one\ntwo\nthree\n");
        let got = b.delete(&Range::new(Pos::new(0, 0), Pos::new(1, 0), RangeKind::Line));
        assert_eq!(got, "one\ntwo\n");
        assert_eq!(b.line_count(), 1);
        assert_eq!(b.line(0), "three");
    }

    #[test]
    fn deleting_every_line_leaves_one_empty_line() {
        // 0 行のバッファはカーソルの置き場が無くなる
        let mut b = buf("one\ntwo");
        b.delete(&Range::new(Pos::new(0, 0), Pos::new(1, 0), RangeKind::Line));
        assert_eq!(b.line_count(), 1);
        assert_eq!(b.line(0), "");
    }

    #[test]
    fn replacing_lines_does_not_glue_onto_the_next_one() {
        // `=` で JSON を整形したとき、閉じ括弧が次の行の頭にくっついて出た回帰。
        let mut b = buf("{\"a\":1}\nNAME SIZE\nx 1\n");
        b.replace(
            &Range::new(Pos::new(0, 0), Pos::new(0, 0), RangeKind::Line),
            "{\n  \"a\": 1\n}",
        );
        assert_eq!(b.line(0), "{");
        assert_eq!(b.line(1), "  \"a\": 1");
        assert_eq!(b.line(2), "}");
        assert_eq!(b.line(3), "NAME SIZE", "次の行とくっついている");
        assert_eq!(b.line_count(), 5);
    }

    #[test]
    fn replacing_every_line_leaves_no_stray_blank() {
        let mut b = buf("a\nb");
        b.replace(
            &Range::new(Pos::new(0, 0), Pos::new(1, 0), RangeKind::Line),
            "one\ntwo",
        );
        assert_eq!(b.line_count(), 2, "消した跡の空行が残っている");
        assert_eq!(b.line(0), "one");
        assert_eq!(b.line(1), "two");
    }

    #[test]
    fn replacing_a_charwise_range_stays_on_the_line() {
        let mut b = buf("hello world");
        b.replace(
            &Range::new(Pos::new(0, 0), Pos::new(0, 4), RangeKind::Char),
            "HI",
        );
        assert_eq!(b.line(0), "HI world");
        assert_eq!(b.line_count(), 1);
    }

    #[test]
    fn wide_characters_keep_columns_and_indices_apart() {
        // 全角は 2 列 1 文字。列で数えたまま消すと 1 文字ずれる。
        let mut b = buf("あいうえお");
        assert_eq!(
            b.cells(0).unwrap().len(),
            10,
            "全角は 2 セル（右半分を含む）"
        );
        b.delete(&Range::new(Pos::new(0, 2), Pos::new(0, 3), RangeKind::Char));
        assert_eq!(b.line(0), "あうえお");
    }

    #[test]
    fn a_wide_character_occupies_two_cells_like_the_grid() {
        // ここがずれると、セルの添字と列が 1 つずつずれて文字が重なって描かれる。
        // 実機でファイルを開いて日本語が潰れて見えたときの回帰。
        let b = buf("あiう");
        let cells = b.cells(0).unwrap();
        assert_eq!(cells.len(), 5);
        assert_eq!(cells[0].width, 2);
        assert_eq!(cells[1].width, 0, "全角の右半分が無い");
        assert_eq!(cells[2].width, 1);
        assert_eq!(cells[3].width, 2);
        assert_eq!(b.line(0), "あiう", "スペーサが文字列に混ざっている");
    }

    #[test]
    fn inserting_before_a_wide_character_lands_on_a_boundary() {
        let mut b = buf("あい");
        let at = b.insert(Pos::new(0, 2), "X");
        assert_eq!(b.line(0), "あXい");
        assert_eq!(at, Pos::new(0, 3));
    }

    #[test]
    fn the_motions_from_tsg_modal_see_it_as_any_other_buffer() {
        // ここが「エディタ内蔵が実質タダ」の実際の意味
        let b = buf("foo bar\nbaz");
        assert_eq!(b.kind(), BufferKind::File);
        assert_eq!(crate::last_col(&b, 0), 6);
        assert_eq!(crate::line_text(&b, 1), "baz");
        assert!(
            b.allows(crate::OperatorId::Change),
            "File では c が使えるはず"
        );
    }

    // ---- 取り消し ----

    #[test]
    fn undo_puts_the_text_back_and_redo_puts_it_forward_again() {
        let mut b = buf("one\ntwo\nthree\n");
        b.begin_group(Pos::new(1, 0));
        b.delete(&Range::new(Pos::new(1, 0), Pos::new(1, 0), RangeKind::Line));
        assert_eq!(b.text(), "one\nthree\n");

        let at = b.undo().expect("戻せない");
        assert_eq!(b.text(), "one\ntwo\nthree\n");
        assert_eq!(at.line, 1, "変更したところへ戻らない");

        b.redo().expect("やり直せない");
        assert_eq!(b.text(), "one\nthree\n");
    }

    /// 1 打鍵で送るのはその 1 文字ぶんだけ。ここが全文に戻ると、
    /// 大きなファイルで打鍵ごとにファイル長を流すことになる。
    #[test]
    fn a_keystroke_only_produces_its_own_bytes() {
        let mut b = buf("a".repeat(10_000).as_str());
        b.begin_group(Pos::new(0, 0));
        let _ = b.take_splices();
        b.insert(Pos::new(0, 5_000), "X");

        let out = b.take_splices();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start, 5_000);
        assert_eq!(out[0].removed, "");
        assert_eq!(out[0].inserted, "X");
        assert!(b.take_splices().is_empty(), "取り出した分が残っている");
    }

    /// 全角の途中で切らない。切ると受け側で `String` に戻せない。
    #[test]
    fn a_splice_never_lands_inside_a_character() {
        let mut b = buf("あいうえお");
        b.begin_group(Pos::new(0, 0));
        let _ = b.take_splices();
        b.insert(Pos::new(0, 4), "X");

        let out = b.take_splices();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start, 6, "「あい」の後ろ = 6 バイト");
        assert_eq!(out[0].inserted, "X");
    }

    /// 行単位の置き換えも同じ形の差分になる。経路ごとに逆操作を書き分けない。
    #[test]
    fn a_linewise_replace_is_one_splice_too() {
        let mut b = buf("one\ntwo\nthree\n");
        b.begin_group(Pos::new(0, 0));
        let _ = b.take_splices();
        b.replace(
            &Range::new(Pos::new(1, 0), Pos::new(1, 0), RangeKind::Line),
            "TWO\n",
        );
        assert_eq!(b.text(), "one\nTWO\nthree\n");

        let out = b.take_splices();
        let total: usize = out.iter().map(|s| s.removed.len() + s.inserted.len()).sum();
        assert!(
            total < 20,
            "行の入れ替えで全文が流れている（{total} バイト）"
        );

        b.undo().expect("戻せない");
        assert_eq!(b.text(), "one\ntwo\nthree\n", "取り消しで元へ戻らない");
    }

    /// 差分を積むので、全文を積んでいたときより桁違いに深く戻れる。
    #[test]
    fn the_undo_history_is_not_measured_in_whole_files() {
        let mut b = buf("x".repeat(100_000).as_str());
        for i in 0..60 {
            b.begin_group(Pos::new(0, 0));
            b.insert(Pos::new(0, i), "y");
        }
        // 全文スナップショットなら 60 * 100KB = 6MB で上限（4MB）に当たり、
        // 最初のほうの段は捨てられていた。
        let mut steps = 0;
        while b.undo().is_some() {
            steps += 1;
        }
        assert_eq!(steps, 60, "取り消しの段が捨てられている");
        assert_eq!(b.text(), "x".repeat(100_000), "全部戻って元に一致しない");
    }

    /// 取り消しの後は差分ではなく全文を渡し直す（何段でも一度に動くため）。
    #[test]
    fn undo_asks_for_a_full_resync() {
        let mut b = buf("abc");
        b.begin_group(Pos::new(0, 0));
        b.insert(Pos::new(0, 0), "X");
        let _ = b.take_splices();
        assert!(!b.take_resync());

        b.undo().expect("戻せない");
        assert!(
            b.take_resync(),
            "取り消しの後に全文を渡し直すよう頼んでいない"
        );
        assert!(!b.take_resync(), "1 度取ったら下りる");
    }

    #[test]
    fn one_insert_session_is_one_undo_step() {
        // 1 打鍵ずつ戻ると使い物にならない
        let mut b = buf("");
        b.begin_group(Pos::new(0, 0));
        let mut at = Pos::new(0, 0);
        for c in ["h", "e", "l", "l", "o"] {
            at = b.insert(at, c);
        }
        assert_eq!(b.line(0), "hello");

        b.undo().expect("戻せない");
        assert_eq!(b.line(0), "", "1 文字ずつしか戻っていない");
        assert!(b.undo().is_none(), "余計な段が積まれている");
    }

    #[test]
    fn each_operator_is_its_own_step() {
        let mut b = buf("a\nb\nc\n");
        b.begin_group(Pos::new(0, 0));
        b.delete(&Range::new(Pos::new(0, 0), Pos::new(0, 0), RangeKind::Line));
        b.begin_group(Pos::new(0, 0));
        b.delete(&Range::new(Pos::new(0, 0), Pos::new(0, 0), RangeKind::Line));
        assert_eq!(b.text(), "c\n");

        b.undo();
        assert_eq!(b.text(), "b\nc\n");
        b.undo();
        assert_eq!(b.text(), "a\nb\nc\n");
    }

    #[test]
    fn a_replace_is_a_single_step() {
        // `=` は中で delete と insert を両方やる。2 段になってはいけない。
        let mut b = buf("{\"a\":1}\nx\n");
        b.begin_group(Pos::new(0, 0));
        b.replace(
            &Range::new(Pos::new(0, 0), Pos::new(0, 0), RangeKind::Line),
            "{\n  \"a\": 1\n}",
        );
        b.undo().expect("戻せない");
        assert_eq!(b.text(), "{\"a\":1}\nx\n", "1 段で戻らない");
    }

    #[test]
    fn a_new_edit_after_undo_drops_the_redo_branch() {
        let mut b = buf("a\n");
        b.begin_group(Pos::new(0, 0));
        b.insert(Pos::new(0, 1), "X");
        b.undo();
        b.begin_group(Pos::new(0, 0));
        b.insert(Pos::new(0, 1), "Y");
        assert!(b.redo().is_none(), "捨てたはずの枝が残っている");
        assert_eq!(b.line(0), "aY");
    }

    #[test]
    fn undo_restores_the_saved_state_too() {
        // 保存 -> 編集 -> 取り消し で「未保存」の印が残ると嘘になる
        let p = std::env::temp_dir().join("tsumugi-undo-dirty.txt");
        let mut b = buf("a\n");
        b.path = Some(p.clone());
        b.save().expect("保存できない");
        assert!(!b.dirty);

        b.begin_group(Pos::new(0, 0));
        b.insert(Pos::new(0, 1), "X");
        assert!(b.dirty);

        b.undo();
        assert!(!b.dirty, "取り消したのに未保存のまま");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn there_is_nothing_to_undo_on_a_fresh_buffer() {
        let mut b = buf("a\n");
        assert!(b.undo().is_none());
        assert!(b.redo().is_none());
    }

    #[test]
    fn saving_writes_what_text_returns() {
        let p = std::env::temp_dir().join("tsumugi-save-test.txt");
        let mut b = buf("hello\nworld\n");
        b.path = Some(p.clone());
        b.dirty = true;
        b.save().expect("保存できない");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hello\nworld\n");
        assert!(!b.dirty, "保存後も dirty のまま");
        let _ = std::fs::remove_file(&p);
    }
}
