//! mux クライアント側の状態。ペインの鏡（mirror）と画面上の割り付け。
//!
//! `arch.md` の不変条件 4 の通り、真実はサーバプロセスが持つ。
//! ここにあるのはその写しであり、PTY の生バイトを自前の `tsg-term` で
//! 解析して作る。差分の直列化を実装しないための設計（`protocol.rs` 参照）。

use std::collections::BTreeMap;

use tsg_modal::command::FocusDir;
use tsg_modal::{Buffer, BufferKind, FileBuffer, TermBuffer};
use tsg_mux::protocol::{Dir, Layout, SessionInfo, weights_for};
use tsg_term::{AmbiguousWidth, Cell, SemanticMarks, Terminal};

/// 左ガターの幅。OSC 133 のマーカーを置く場所（`mouse-parity.md` §4.2）。
///
/// ここが「ターミナル固有モーションのマウス版」の置き場所であり、
/// `[[` `]]` `[e` `]e` に対応するクリック標的が自動的に生まれる。
pub const GUTTER: usize = 2;

/// セル単位の矩形。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

impl Rect {
    pub fn contains(&self, x: usize, y: usize) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    pub fn center(&self) -> (usize, usize) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
}

/// ペインが見せている文書。
///
/// モーションもテキストオブジェクトもオペレータも、この 1 つの窓口しか見ない。
/// 端末用とファイル用を二重実装しないのが `Buffer` を作った理由（`concept.md`）。
pub enum PaneBuffer<'a> {
    Term(TermBuffer<'a>),
    File(&'a FileBuffer),
}

impl Buffer for PaneBuffer<'_> {
    fn kind(&self) -> BufferKind {
        match self {
            PaneBuffer::Term(t) => t.kind(),
            PaneBuffer::File(f) => f.kind(),
        }
    }

    fn line_count(&self) -> usize {
        match self {
            PaneBuffer::Term(t) => t.line_count(),
            PaneBuffer::File(f) => f.line_count(),
        }
    }

    fn cells(&self, line: usize) -> Option<&[Cell]> {
        match self {
            PaneBuffer::Term(t) => t.cells(line),
            PaneBuffer::File(f) => f.cells(line),
        }
    }

    fn marks(&self) -> &SemanticMarks {
        match self {
            PaneBuffer::Term(t) => t.marks(),
            PaneBuffer::File(f) => f.marks(),
        }
    }
}

/// 1ペインの写し。
///
/// `file` が入っている間、そのペインは**エディタとして**振る舞う。
/// 下のシェルは走ったままで、`:q` で戻ってくる。ペイン ID とレイアウトは
/// サーバのものを使い続けるので、分割も配置モードもそのまま効く。
pub struct PaneView {
    pub term: Terminal,
    /// エディタとして開いているファイル（`arch.md` §9 の M4）
    pub file: Option<FileBuffer>,
    /// 表示用の名前。`>` の結果のように保存先が無いこともあるので別に持つ。
    pub title: String,
    pub rect: Rect,
    /// 表示先頭のドキュメント絶対行
    pub top: usize,
    pub follow_tail: bool,
    pub alive: bool,
}

impl PaneView {
    /// 本文の領域。ガターを除いた残り。
    ///
    /// **PTY に伝える桁数もこれ**。ここがずれると、シェルの折り返し位置と
    /// 画面の折り返し位置が食い違って行がにじむ。
    pub fn text_rect(&self) -> Rect {
        Rect {
            x: self.rect.x + GUTTER,
            y: self.rect.y,
            w: self.rect.w.saturating_sub(GUTTER),
            h: self.rect.h,
        }
    }

    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            term: Terminal::new(cols.max(1), rows.max(1), AmbiguousWidth::Wide),
            file: None,
            title: String::new(),
            rect: Rect::default(),
            top: 0,
            follow_tail: true,
            alive: true,
        }
    }

    /// いま見せている文書。
    pub fn buffer(&self) -> PaneBuffer<'_> {
        match &self.file {
            Some(f) => PaneBuffer::File(f),
            None => PaneBuffer::Term(TermBuffer::new(&self.term.state.grid, &self.term.state.marks)),
        }
    }

    /// エディタとして開いているか。
    /// このペインの構文強調の言語。端末なら `None`。
    ///
    /// 保存先が無いバッファ（`>` の結果）は中身から当てにいく。
    pub fn lang(&self) -> tsg_modal::SyntaxLang {
        let Some(file) = self.file.as_ref() else {
            return tsg_modal::SyntaxLang::None;
        };
        match file.path.as_ref() {
            Some(p) => tsg_modal::SyntaxLang::of_path(p),
            None => tsg_modal::SyntaxLang::sniff(&file.line(0)),
        }
    }

    pub fn editing(&self) -> bool {
        self.file.is_some()
    }

    /// 文書の行数（端末なら履歴込み）。
    pub fn doc_len(&self) -> usize {
        match &self.file {
            Some(f) => f.line_count(),
            None => self.term.state.grid.document_len(),
        }
    }

    /// ステータス行に出す名前。
    pub fn label(&self) -> Option<String> {
        let f = self.file.as_ref()?;
        let name = f
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .or_else(|| (!self.title.is_empty()).then(|| self.title.clone()))
            .unwrap_or_else(|| "(無題)".into());
        Some(if f.dirty {
            format!("{name} *")
        } else {
            name
        })
    }

    /// 再アタッチ時の画面復元。行を流し込んで写しを組み直す。
    pub fn restore(&mut self, lines: &[String], cols: usize, rows: usize) {
        self.term = Terminal::new(cols.max(1), rows.max(1), AmbiguousWidth::Wide);
        let mut data = String::new();
        for line in lines {
            data.push_str(line);
            data.push_str("\r\n");
        }
        self.term.feed(data.as_bytes());
        self.follow_tail = true;
    }
}

#[derive(Default)]
pub struct Session {
    pub info: Option<SessionInfo>,
    pub panes: BTreeMap<u32, PaneView>,
    pub active: u32,
}

impl Session {
    /// 現在のタブのレイアウト木。
    pub fn active_layout(&self) -> Option<&Layout> {
        let info = self.info.as_ref()?;
        info.tabs
            .iter()
            .find(|t| t.id == info.active_tab)
            .map(|t| &t.layout)
    }

    /// 今のタブの id。
    pub fn active_tab(&self) -> Option<u32> {
        self.info.as_ref().map(|i| i.active_tab)
    }

    /// ズーム中のペイン（`Space z`）。
    ///
    /// レイアウト木は畳まない。**出すものを選ぶだけ**にしてあるので、
    /// 戻したときに分割比まで元どおりになる。
    pub fn zoomed(&self) -> Option<u32> {
        let info = self.info.as_ref()?;
        let tab = info.tabs.iter().find(|t| t.id == info.active_tab)?;
        let z = tab.zoom?;
        tab.layout.panes().contains(&z).then_some(z)
    }

    /// 表示領域をレイアウト木で割り付ける。区切り線に 1 セル使う。
    pub fn assign_rects(&mut self, area: Rect) {
        if let Some(z) = self.zoomed() {
            if let Some(view) = self.panes.get_mut(&z) {
                view.rect = area;
            }
            return;
        }
        let Some(layout) = self.active_layout().cloned() else {
            return;
        };
        let mut rects = BTreeMap::new();
        split_rects(&layout, area, &mut rects);
        for (id, rect) in rects {
            if let Some(view) = self.panes.get_mut(&id) {
                view.rect = rect;
            }
        }
    }

    /// 表示中のペイン（現在のタブに属するもの）。
    pub fn visible_panes(&self) -> Vec<u32> {
        if let Some(z) = self.zoomed() {
            return vec![z];
        }
        self.active_layout().map(Layout::panes).unwrap_or_default()
    }

    /// 方向を指定して隣のペインを探す。
    ///
    /// 中心から目的方向へ進み、最初に当たったペインを採る。
    /// 木を辿るより、見えている配置に対して素直に効く。
    pub fn neighbor(&self, from: u32, dir: FocusDir) -> Option<u32> {
        let origin = self.panes.get(&from)?.rect;
        let (cx, cy) = origin.center();
        let visible = self.visible_panes();

        let (dx, dy): (isize, isize) = match dir {
            FocusDir::Left => (-1, 0),
            FocusDir::Right => (1, 0),
            FocusDir::Up => (0, -1),
            FocusDir::Down => (0, 1),
        };

        let (mut x, mut y) = (cx as isize, cy as isize);
        for _ in 0..1000 {
            x += dx;
            y += dy;
            if x < 0 || y < 0 {
                return None;
            }
            for id in &visible {
                if *id == from {
                    continue;
                }
                if let Some(v) = self.panes.get(id)
                    && v.rect.contains(x as usize, y as usize)
                {
                    return Some(*id);
                }
            }
        }
        None
    }

    /// 画面座標からペインを引く。割り付けの逆写像。
    pub fn pane_at(&self, x: usize, y: usize) -> Option<u32> {
        self.visible_panes()
            .into_iter()
            .find(|id| self.panes.get(id).is_some_and(|v| v.rect.contains(x, y)))
    }

    /// 画面座標がペイン境界の上なら、その境界と、ドラッグで太らせるペイン。
    ///
    /// 境界は**左（上）のペインの右（下）端**に属するものとして扱う。
    /// 「掴んで右へ動かせばそのペインが広がる」という素直な対応になる。
    pub fn divider_at(&self, x: usize, y: usize) -> Option<(u32, Dir)> {
        let visible = self.visible_panes();
        let occupied = |px: usize, py: usize| {
            visible
                .iter()
                .any(|id| self.panes.get(id).is_some_and(|v| v.rect.contains(px, py)))
        };
        for id in &visible {
            let Some(v) = self.panes.get(id) else {
                continue;
            };
            let r = v.rect;
            // 端の外周は境界ではない（隣にペインが居ることを条件にする）
            if r.x + r.w == x && (r.y..r.y + r.h).contains(&y) && occupied(x + 1, y) {
                return Some((*id, Dir::Horizontal));
            }
            if r.y + r.h == y && (r.x..r.x + r.w).contains(&x) && occupied(x, y + 1) {
                return Some((*id, Dir::Vertical));
            }
        }
        None
    }
}

fn split_rects(layout: &Layout, area: Rect, out: &mut BTreeMap<u32, Rect>) {
    match layout {
        Layout::Leaf { pane } => {
            out.insert(*pane, area);
        }
        Layout::Split {
            dir,
            children,
            weights,
        } => {
            let n = children.len();
            if n == 0 {
                return;
            }
            if n == 1 {
                split_rects(&children[0], area, out);
                return;
            }
            let dividers = n - 1;
            let w = weights_for(children, weights);
            let total: u64 = w.iter().map(|x| u64::from(*x)).sum();
            let share = |avail: usize, i: usize| {
                ((avail as u64 * u64::from(w[i])) / total.max(1)) as usize
            };
            match dir {
                Dir::Horizontal => {
                    let avail = area.w.saturating_sub(dividers);
                    let mut x = area.x;
                    for (i, child) in children.iter().enumerate() {
                        let w = if i == n - 1 {
                            (area.x + area.w).saturating_sub(x)
                        } else {
                            share(avail, i).max(1)
                        };
                        split_rects(
                            child,
                            Rect {
                                x,
                                y: area.y,
                                w,
                                h: area.h,
                            },
                            out,
                        );
                        x += w + 1;
                    }
                }
                Dir::Vertical => {
                    let avail = area.h.saturating_sub(dividers);
                    let mut y = area.y;
                    for (i, child) in children.iter().enumerate() {
                        let h = if i == n - 1 {
                            (area.y + area.h).saturating_sub(y)
                        } else {
                            share(avail, i).max(1)
                        };
                        split_rects(
                            child,
                            Rect {
                                x: area.x,
                                y,
                                w: area.w,
                                h,
                            },
                            out,
                        );
                        y += h + 1;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect {
            x: 0,
            y: 0,
            w: 100,
            h: 40,
        }
    }

    #[test]
    fn a_single_pane_takes_the_whole_area() {
        let mut out = BTreeMap::new();
        split_rects(&Layout::leaf(1), area(), &mut out);
        assert_eq!(out[&1], area());
    }

    #[test]
    fn horizontal_split_divides_width_and_leaves_a_divider() {
        let mut l = Layout::leaf(1);
        l.split(1, 2, Dir::Horizontal);
        let mut out = BTreeMap::new();
        split_rects(&l, area(), &mut out);

        let (a, b) = (out[&1], out[&2]);
        assert_eq!(a.x, 0);
        assert_eq!(a.h, 40);
        assert_eq!(b.h, 40);
        assert_eq!(b.x, a.w + 1, "区切りに 1 セル空ける");
        assert_eq!(a.w + 1 + b.w, 100, "領域を余さず使い切る");
    }

    #[test]
    fn vertical_split_divides_height() {
        let mut l = Layout::leaf(1);
        l.split(1, 2, Dir::Vertical);
        let mut out = BTreeMap::new();
        split_rects(&l, area(), &mut out);
        let (a, b) = (out[&1], out[&2]);
        assert_eq!(b.y, a.h + 1);
        assert_eq!(a.h + 1 + b.h, 40);
        assert_eq!(a.w, 100);
    }

    #[test]
    fn three_way_split_uses_the_whole_area() {
        let mut l = Layout::leaf(1);
        l.split(1, 2, Dir::Horizontal);
        l.split(2, 3, Dir::Horizontal);
        let mut out = BTreeMap::new();
        split_rects(&l, area(), &mut out);
        assert_eq!(out.len(), 3);
        let total: usize = out.values().map(|r| r.w).sum();
        assert_eq!(total + 2, 100, "区切り 2 本ぶんを除いて使い切る");
    }

    fn session_with(layout: Layout, ids: &[u32]) -> Session {
        let mut s = Session {
            info: Some(SessionInfo {
                name: "t".into(),
                tabs: vec![tsg_mux::protocol::TabInfo {
                    id: 1,
                    layout,
                    active_pane: ids[0],
                    zoom: None,
                }],
                active_tab: 1,
                panes: vec![],
            }),
            panes: BTreeMap::new(),
            active: ids[0],
        };
        for id in ids {
            s.panes.insert(*id, PaneView::new(80, 24));
        }
        s.assign_rects(area());
        s
    }

    #[test]
    fn neighbor_finds_the_pane_to_the_right() {
        let mut l = Layout::leaf(1);
        l.split(1, 2, Dir::Horizontal);
        let s = session_with(l, &[1, 2]);
        assert_eq!(s.neighbor(1, FocusDir::Right), Some(2));
        assert_eq!(s.neighbor(2, FocusDir::Left), Some(1));
        assert_eq!(s.neighbor(1, FocusDir::Left), None, "端では動かない");
    }

    #[test]
    fn neighbor_works_vertically() {
        let mut l = Layout::leaf(1);
        l.split(1, 2, Dir::Vertical);
        let s = session_with(l, &[1, 2]);
        assert_eq!(s.neighbor(1, FocusDir::Down), Some(2));
        assert_eq!(s.neighbor(2, FocusDir::Up), Some(1));
    }

    #[test]
    fn pane_at_maps_screen_coordinates_back() {
        let mut l = Layout::leaf(1);
        l.split(1, 2, Dir::Horizontal);
        let s = session_with(l, &[1, 2]);
        assert_eq!(s.pane_at(5, 5), Some(1));
        assert_eq!(s.pane_at(95, 5), Some(2));
    }

    #[test]
    fn the_gutter_comes_out_of_the_text_area_not_the_pane() {
        let s = session_with(Layout::leaf(1), &[1]);
        let v = &s.panes[&1];
        assert_eq!(v.rect.w, 100, "ペインは領域いっぱい");
        assert_eq!(v.text_rect().w, 100 - GUTTER, "本文がガターぶん狭くない");
        assert_eq!(v.text_rect().x, GUTTER, "本文の左端がずれていない");
        assert_eq!(v.text_rect().h, v.rect.h, "高さは削らない");
    }

    #[test]
    fn divider_at_finds_the_seam_between_panes() {
        let mut l = Layout::leaf(1);
        l.split(1, 2, Dir::Horizontal);
        let s = session_with(l, &[1, 2]);
        let seam = s.panes[&1].rect.w;

        assert_eq!(
            s.divider_at(seam, 10),
            Some((1, Dir::Horizontal)),
            "境界は左のペインに属する"
        );
        assert_eq!(s.divider_at(seam - 1, 10), None, "本文を境界と誤認している");
        assert_eq!(s.divider_at(99, 10), None, "画面の右端は境界ではない");
    }

    #[test]
    fn divider_at_works_for_horizontal_seams_too() {
        let mut l = Layout::leaf(1);
        l.split(1, 2, Dir::Vertical);
        let s = session_with(l, &[1, 2]);
        let seam = s.panes[&1].rect.h;
        assert_eq!(s.divider_at(10, seam), Some((1, Dir::Vertical)));
        assert_eq!(s.divider_at(10, 39), None, "画面の下端は境界ではない");
    }

    #[test]
    fn weights_change_the_allocation() {
        let mut l = Layout::leaf(1);
        l.split(1, 2, Dir::Horizontal);
        l.resize(1, 100); // 1 の取り分を倍にする
        let s = session_with(l, &[1, 2]);
        assert!(
            s.panes[&1].rect.w > s.panes[&2].rect.w,
            "重みが割り付けに効いていない: {} vs {}",
            s.panes[&1].rect.w,
            s.panes[&2].rect.w
        );
        assert_eq!(
            s.panes[&1].rect.w + 1 + s.panes[&2].rect.w,
            100,
            "領域を使い切っていない"
        );
    }

    #[test]
    fn restore_keeps_the_colors_the_server_sent() {
        // サーバは SGR 付きの ANSI を送る（protocol.rs 版 2）。
        // 復元路が素通しになっていると、再アタッチで色だけ落ちる。
        let mut v = PaneView::new(40, 5);
        v.restore(&["\x1b[1;31mERROR\x1b[0m: boom".to_string()], 40, 5);

        let line = v.term.state.grid.document_line(0).unwrap();
        assert_eq!(line.text(), "ERROR: boom");
        assert_eq!(line.cells[0].attrs.fg, tsg_term::Color::Indexed(1));
        assert!(line.cells[0].attrs.has(tsg_term::Attrs::BOLD));
        assert_eq!(
            line.cells[5].attrs,
            tsg_term::Attrs::default(),
            "リセットが効いていない"
        );
    }

    #[test]
    fn zooming_shows_one_pane_at_full_size() {
        let mut layout = Layout::leaf(1);
        layout.split(1, 2, tsg_mux::Dir::Horizontal);
        let mut s = Session {
            info: Some(SessionInfo {
                name: "t".into(),
                tabs: vec![tsg_mux::protocol::TabInfo {
                    id: 1,
                    layout,
                    active_pane: 1,
                    zoom: Some(2),
                }],
                active_tab: 1,
                panes: vec![],
            }),
            panes: [(1, PaneView::new(80, 24)), (2, PaneView::new(80, 24))]
                .into_iter()
                .collect(),
            active: 1,
        };
        let area = Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        };
        s.assign_rects(area);
        assert_eq!(s.visible_panes(), vec![2], "ズーム中に他のペインが出ている");
        assert_eq!(s.panes[&2].rect, area, "ズームしたペインが全面になっていない");
    }

    #[test]
    fn a_zoom_on_a_pane_that_is_gone_is_ignored() {
        // 閉じたペインをズーム指定したまま残すと、何も映らない画面になる。
        let s = Session {
            info: Some(SessionInfo {
                name: "t".into(),
                tabs: vec![tsg_mux::protocol::TabInfo {
                    id: 1,
                    layout: Layout::leaf(1),
                    active_pane: 1,
                    zoom: Some(99),
                }],
                active_tab: 1,
                panes: vec![],
            }),
            panes: [(1, PaneView::new(80, 24))].into_iter().collect(),
            active: 1,
        };
        assert_eq!(s.zoomed(), None);
        assert_eq!(s.visible_panes(), vec![1]);
    }

    #[test]
    fn restore_rebuilds_the_document_from_lines() {
        let mut v = PaneView::new(40, 5);
        let lines: Vec<String> = (0..12).map(|i| format!("line {i}")).collect();
        v.restore(&lines, 40, 5);
        let text = v.term.state.grid.document_text();
        assert!(text.contains("line 0"), "古い行が失われている");
        assert!(text.contains("line 11"), "新しい行が入っていない");
    }
}
