//! ディレクトリを**バッファとして**見せる（`explorer`）。
//!
//! 左に木を出すためだけに描画経路を増やさない。`Buffer` を実装すれば
//! `j` `k` `gg` `G` も検索も選択もマウスも**そのまま効く**。これは
//! `concept.md` の「エディタ内蔵が実質タダになる」と同じ理屈で、
//! ここに専用のモーション実装は 1 行も無い。
//!
//! ## 中身を作るのはサーバ
//!
//! ディスクを読むのは `tsg-mux` のサーバ側（`ServerMsg::DirListing`）。
//! **遠隔（`[domains]`）でセッションを開いているとき、ファイルが在るのは
//! 向こう**なので、クライアントが手元の `std::fs` を読むと、まったく別の
//! 木が出る。開いているファイルの中身をサーバに持たせているのと同じ理由。
//!
//! ここが持つのは「どこを開いているか（`expanded`）」だけで、それも
//! **並べ直しの入力**としてサーバへ送り返す。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use tsg_term::{AmbiguousWidth, Attrs, Cell, Color, SemanticMarks};

use crate::file::cells_of;
use crate::{Buffer, BufferKind};

/// 木の 1 行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    /// 根からの深さ。根そのものは 0。
    pub depth: usize,
}

/// 1 段ぶんの字下げ。
const INDENT: usize = 2;

/// 開いた印・閉じた印。**ファイルにも同じ幅を空ける**ので、名前の頭がそろう。
const OPEN: &str = "▾ ";
const SHUT: &str = "▸ ";
const LEAF: &str = "  ";

/// ディレクトリの木。
pub struct DirBuffer {
    /// 根。**ペインが見ている場所**で、`go` が決める。
    pub root: PathBuf,
    entries: Vec<DirEntry>,
    /// 開いてある枝。**並べ直しても畳まれない**ように、パスで持つ。
    expanded: BTreeSet<PathBuf>,
    lines: Vec<Vec<Cell>>,
    marks: SemanticMarks,
    amb: AmbiguousWidth,
    /// まだサーバの返事が来ていない。
    pub loading: bool,
    /// 最後にどの行に居たか。
    ///
    /// **カーソルは窓に 1 つしか無い。** 端末から木へ移ったとき、端末の
    /// 行番号（履歴込みで何千行）を木の行数へ丸めると、必ず一番下の行に
    /// 着く。木の中で選んでいた行に戻るのが、頼んだことに近い。
    pub cursor_line: usize,
}

impl DirBuffer {
    pub fn new(root: PathBuf, amb: AmbiguousWidth) -> Self {
        let mut me = Self {
            root: root.clone(),
            entries: Vec::new(),
            expanded: BTreeSet::from([root]),
            lines: Vec::new(),
            marks: SemanticMarks::default(),
            amb,
            loading: true,
            cursor_line: 0,
        };
        me.rebuild();
        me
    }

    /// サーバが並べた行で総取り替えする。
    ///
    /// **差分では受けない。** 名前を変えた・作った・動かしたのあとは
    /// 木の形が大きく変わることがあり、差分を当てにいくと
    /// 「画面には在るのにディスクには無い」行が静かに残る。
    pub fn set_entries(&mut self, entries: Vec<DirEntry>) {
        self.entries = entries;
        self.loading = false;
        // 消えたディレクトリを開いたままにしない（次の並べ直しへ持ち越す入力）。
        let alive: BTreeSet<&PathBuf> = self.entries.iter().map(|e| &e.path).collect();
        self.expanded
            .retain(|p| p == &self.root || alive.contains(p));
        self.cursor_line = self.cursor_line.min(self.entries.len().saturating_sub(1));
        self.rebuild();
    }

    pub fn entries(&self) -> &[DirEntry] {
        &self.entries
    }

    /// その行が指しているもの。
    pub fn entry(&self, line: usize) -> Option<&DirEntry> {
        self.entries.get(line)
    }

    /// その行が指しているディレクトリ。ファイルの行なら親を返す。
    ///
    /// 「ここに新しいファイルを作る」の行き先。ファイルを選んだまま
    /// 作れないのは不便なだけで、意味としては**その隣**に作りたい。
    pub fn dir_at(&self, line: usize) -> PathBuf {
        match self.entries.get(line) {
            Some(e) if e.is_dir => e.path.clone(),
            Some(e) => e
                .path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.root.clone()),
            None => self.root.clone(),
        }
    }

    pub fn is_expanded(&self, path: &Path) -> bool {
        self.expanded.contains(path)
    }

    /// 枝を開く / 畳む。**根は畳めない**（畳むと何も指せなくなる）。
    ///
    /// 返り値は「並べ直しが要るか」。
    pub fn toggle(&mut self, path: &Path) -> bool {
        if path == self.root {
            return false;
        }
        if self.expanded.contains(path) {
            // 中の枝も畳む。開いたまま残すと、開き直したときに
            // **前に見ていたより深く**開いて、位置を見失う。
            self.expanded.retain(|p| !p.starts_with(path));
        } else {
            self.expanded.insert(path.to_path_buf());
        }
        true
    }

    pub fn expand(&mut self, path: &Path) -> bool {
        self.expanded.insert(path.to_path_buf())
    }

    /// サーバへ渡す「開いてある枝」。
    pub fn expanded_paths(&self) -> Vec<String> {
        self.expanded
            .iter()
            .map(|p| p.display().to_string())
            .collect()
    }

    /// そのパスが何行目に出ているか。作った直後にそこへ飛ぶために使う。
    pub fn line_of(&self, path: &Path) -> Option<usize> {
        self.entries.iter().position(|e| e.path == path)
    }

    fn rebuild(&mut self) {
        if self.entries.is_empty() {
            let text = if self.loading { "…" } else { "（空）" };
            self.lines = vec![cells_of(text, self.amb)];
            return;
        }
        self.lines = self
            .entries
            .iter()
            .map(|e| {
                let mark = if !e.is_dir {
                    LEAF
                } else if self.expanded.contains(&e.path) {
                    OPEN
                } else {
                    SHUT
                };
                let text = format!(
                    "{}{}{}{}",
                    " ".repeat(e.depth * INDENT),
                    mark,
                    e.name,
                    if e.is_dir { "/" } else { "" }
                );
                let mut cells = cells_of(&text, self.amb);
                if e.is_dir {
                    // ディレクトリは太字。**色だけで分けない**（配色を
                    // 変えている人や、色の見え方が違う人の画面で消える）。
                    for c in &mut cells {
                        c.attrs.fg = Color::Indexed(4);
                        c.attrs.set(Attrs::BOLD);
                    }
                }
                cells
            })
            .collect();
    }
}

impl Buffer for DirBuffer {
    /// **ファイルとして振る舞う。** 一覧は行の並びで、端末の
    /// スクロールバックではない（矢印も印も、ファイルの規則が正しい）。
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, name: &str, is_dir: bool, depth: usize) -> DirEntry {
        DirEntry {
            path: PathBuf::from(path),
            name: name.into(),
            is_dir,
            depth,
        }
    }

    fn sample() -> DirBuffer {
        let mut b = DirBuffer::new(PathBuf::from("/w"), AmbiguousWidth::Narrow);
        b.set_entries(vec![
            entry("/w", "w", true, 0),
            entry("/w/src", "src", true, 1),
            entry("/w/README.md", "README.md", false, 1),
        ]);
        b
    }

    #[test]
    fn the_root_row_comes_first_and_cannot_be_folded_away() {
        let mut b = sample();
        assert_eq!(b.entry(0).unwrap().name, "w");
        assert!(!b.toggle(&PathBuf::from("/w")), "根は畳めない");
        assert!(b.is_expanded(&PathBuf::from("/w")));
    }

    #[test]
    fn folding_a_branch_also_folds_what_was_open_inside_it() {
        let mut b = sample();
        b.expand(&PathBuf::from("/w/src"));
        b.expand(&PathBuf::from("/w/src/deep"));
        b.toggle(&PathBuf::from("/w/src"));
        assert!(!b.is_expanded(&PathBuf::from("/w/src")));
        assert!(
            !b.is_expanded(&PathBuf::from("/w/src/deep")),
            "中の枝が開いたまま残ると、開き直したときに前より深く開く"
        );
    }

    #[test]
    fn a_file_row_points_at_its_parent_when_asked_where_to_create() {
        let b = sample();
        assert_eq!(b.dir_at(2), PathBuf::from("/w"));
        assert_eq!(b.dir_at(1), PathBuf::from("/w/src"));
    }

    #[test]
    fn every_row_is_a_line_so_motions_work_without_any_extra_code() {
        let b = sample();
        assert_eq!(b.line_count(), 3);
        assert!(b.cells(1).is_some());
        assert_eq!(b.kind(), BufferKind::File);
    }

    /// 木は**自分が居た行を覚えている**。端末から戻ったときに
    /// 一番下へ飛ぶのは、頼んだことと違う。
    #[test]
    fn the_tree_remembers_which_row_you_were_on() {
        let mut b = sample();
        b.cursor_line = 2;
        b.set_entries(vec![entry("/w", "w", true, 0)]);
        assert_eq!(b.cursor_line, 0, "行が減ったら中へ寄せる");
    }

    #[test]
    fn a_branch_that_disappeared_from_disk_stops_counting_as_open() {
        let mut b = sample();
        b.expand(&PathBuf::from("/w/src"));
        b.set_entries(vec![entry("/w", "w", true, 0)]);
        assert!(!b.is_expanded(&PathBuf::from("/w/src")));
        assert!(b.is_expanded(&PathBuf::from("/w")), "根は残る");
    }
}
