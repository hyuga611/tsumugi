//! コマンドパレットと右クリックメニュー。
//!
//! `mouse-parity.md` §2.1 の通り、**どちらも項目をコマンドレジストリから生成する**。
//! 手で並べたメニューは必ず腐る（機能を足した人がメニューを更新し忘れる）ので、
//! 「レジストリに載っている＝メニューにもパレットにも出る」を構造で保証する。
//!
//! §4.7 の言う「パレットが最終保証」もここで成立する。レジストリを全走査するので、
//! `MousePath::Palette` と宣言されたコマンドは必ずマウスから 2 アクションで届く。

use tsg_modal::{CommandSpec, MousePath, REGISTRY, t};

/// 一覧に出す 1 項目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: &'static str,
    pub title: &'static str,
    /// 既定のキー。「マウスから始めた人がキーボードを覚える導線」（§4.7）。
    pub keys: String,
    /// メニューの見出し。項目が増えても読める形にするために持つ。
    pub section: &'static str,
}

impl Item {
    fn of(spec: &'static CommandSpec) -> Self {
        Self {
            id: spec.id,
            title: spec.label(),
            keys: spec.keys.join(" "),
            section: match spec.mouse {
                MousePath::Menu(s) => s,
                _ => "",
            },
        }
    }
}

/// メニューの見出しの訳。
fn section_label(section: &str) -> &'static str {
    match section {
        "編集" => t!("編集", "Edit"),
        "ファイル" => t!("ファイル", "File"),
        "配置" => t!("画面", "Layout"),
        "セッション" => t!("セッション", "Session"),
        _ => "",
    }
}

/// メニューの 1 行。見出しは選べない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Header(&'static str),
    Item(Item),
}

/// 一覧で起きたこと。
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// この id を実行する
    Run(&'static str),
    /// 名前を選んだ（セッション一覧）。レジストリの id ではないので別にする。
    Pick(String),
    Close,
    /// 表示だけ変わった
    Redraw,
    /// 何も起きていない
    None,
}

/// 選んでいる項目が窓の外へ出ないように、見せ始めの位置を寄せる。
///
/// **一覧は必ずここを通す。** M8 まで、パレットは頭から 14 件を切り取って
/// 出すだけだった。↓ で 15 件目へ動くと選択が画面の外へ消え、Enter が
/// 何を実行するのか分からなくなる（実機で踏んだ）。
fn scroll_into_view(offset: &mut usize, selected: usize, height: usize) {
    if height == 0 {
        return;
    }
    if selected < *offset {
        *offset = selected;
    } else if selected >= *offset + height {
        *offset = selected + 1 - height;
    }
}

// ---------------------------------------------------------------------------
// コマンドパレット
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct Palette {
    pub open: bool,
    pub query: String,
    /// 何を打っているか。検索のときは一覧を出さず、打つたびに飛ぶ。
    pub kind: PaletteKind,
    pub selected: usize,
    /// 見せ始めの位置。`view()` が選択に合わせて寄せる。
    offset: usize,
    items: Vec<Item>,
}

impl Palette {
    pub fn show(&mut self) {
        self.open = true;
        self.kind = PaletteKind::Command;
        self.query.clear();
        self.selected = 0;
        self.offset = 0;
        self.refresh();
    }

    /// 検索として開く。一覧は出さない。
    pub fn show_search(&mut self, back: bool) {
        self.open = true;
        self.kind = PaletteKind::Search { back };
        self.query.clear();
        self.selected = 0;
        self.offset = 0;
        self.items.clear();
    }

    pub fn searching(&self) -> bool {
        matches!(self.kind, PaletteKind::Search { .. })
    }

    /// 高さ `height` の窓に収まる範囲。選んでいる項目は必ずこの中に入る。
    pub fn view(&mut self, height: usize) -> (usize, &[Item]) {
        scroll_into_view(&mut self.offset, self.selected, height);
        let end = (self.offset + height).min(self.items.len());
        (self.offset, &self.items[self.offset.min(end)..end])
    }

    pub fn hide(&mut self) {
        self.open = false;
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// 絞り込み。id・題名・キーのどれかに含まれれば通す。
    ///
    /// あいまい検索にしないのは、打った文字と出てくる項目の関係が
    /// 見て分かることを優先しているため。
    fn refresh(&mut self) {
        if self.searching() {
            self.items.clear();
            return;
        }
        let q = self.query.to_lowercase();
        self.items = REGISTRY
            .iter()
            .filter(|s| s.in_palette)
            .filter(|s| {
                q.is_empty()
                    || s.id.to_lowercase().contains(&q)
                    || s.label().to_lowercase().contains(&q)
                    || s.title.to_lowercase().contains(&q)
                    || s.keys.iter().any(|k| k.to_lowercase().contains(&q))
            })
            .map(Item::of)
            .collect();
        self.selected = self.selected.min(self.items.len().saturating_sub(1));
        self.offset = self.offset.min(self.selected);
    }

    pub fn push(&mut self, c: char) -> Action {
        self.query.push(c);
        self.selected = 0;
        self.offset = 0;
        self.refresh();
        Action::Redraw
    }

    pub fn backspace(&mut self) -> Action {
        if self.query.pop().is_none() {
            return Action::Close;
        }
        self.selected = 0;
        self.offset = 0;
        self.refresh();
        Action::Redraw
    }

    pub fn move_by(&mut self, delta: isize) -> Action {
        if self.items.is_empty() {
            return Action::None;
        }
        let n = self.items.len() as isize;
        self.selected = ((self.selected as isize + delta).rem_euclid(n)) as usize;
        Action::Redraw
    }

    pub fn accept(&mut self) -> Action {
        match self.items.get(self.selected) {
            Some(item) => Action::Run(item.id),
            None => Action::Close,
        }
    }

    /// 一覧のどの行がクリックされたか。`row` は**見えている**先頭を 0 とした行。
    pub fn click(&mut self, row: usize) -> Action {
        match self.items.get(self.offset + row) {
            Some(item) => Action::Run(item.id),
            None => Action::None,
        }
    }
}

// ---------------------------------------------------------------------------
// 右クリックメニュー
// ---------------------------------------------------------------------------

/// パレットを開くための擬似項目。メニューの最後に必ず置く。
///
/// これがあるので、メニューに出ていないコマンドでも**メニュー -> パレット -> 選択**の
/// 2 アクションで届く。`mouse-parity.md` §1.1 の約束はここで閉じる。
pub const OPEN_PALETTE: &str = "ui.palette";

#[derive(Default)]
pub struct Menu {
    pub open: bool,
    /// 左上のセル座標
    pub at: (usize, usize),
    pub selected: Option<usize>,
    rows: Vec<Row>,
    /// 見せ始めの行。窓に入り切らないときだけ 0 より大きくなる。
    offset: usize,
    /// 実際に描く行数。`fit()` が窓の高さから決める。
    view_h: usize,
}

impl Menu {
    /// その場に合わせてメニューを組む。
    ///
    /// `has_selection` が偽なら、範囲を要する項目は出さない
    /// （押せるのに何も起きない項目を並べない）。
    pub fn show(&mut self, at: (usize, usize), has_selection: bool) {
        let items: Vec<Item> = REGISTRY
            .iter()
            .filter(|s| matches!(s.mouse, MousePath::Menu(_)))
            .filter(|s| has_selection || !needs_range(s.id))
            .map(Item::of)
            .collect();

        // 見出しでまとめる。20 項目を平らに並べると、目で追えない。
        self.rows.clear();
        for section in ["編集", "ファイル", "配置", "セッション"] {
            let group: Vec<&Item> = items.iter().filter(|i| i.section == section).collect();
            if group.is_empty() {
                continue;
            }
            self.rows.push(Row::Header(section_label(section)));
            for i in group {
                self.rows.push(Row::Item(i.clone()));
            }
        }
        self.rows.push(Row::Item(Item {
            id: OPEN_PALETTE,
            title: t!("すべてのコマンド…", "All commands…"),
            keys: ":".into(),
            section: "",
        }));
        self.at = at;
        self.selected = None;
        self.offset = 0;
        self.view_h = self.rows.len();
        self.open = true;
    }

    /// 窓に入る高さへ詰める。**入り切らない分は切り捨てず、たどれるようにする。**
    ///
    /// 以前は縦に入らないと下がそのまま画面外へ出ていた。項目が 29 行あるので、
    /// 少し低いウィンドウでは「セッション」と「すべてのコマンド…」に手が届かなかった。
    pub fn fit(&mut self, max_h: usize) {
        self.view_h = self.rows.len().min(max_h.max(1));
        if let Some(sel) = self.selected {
            scroll_into_view(&mut self.offset, sel, self.view_h);
        }
        let max_off = self.rows.len().saturating_sub(self.view_h);
        self.offset = self.offset.min(max_off);
    }

    /// 見えている行（`offset` からの `view_h` 行）。
    pub fn view(&self) -> (usize, &[Row]) {
        let end = (self.offset + self.view_h).min(self.rows.len());
        (self.offset, &self.rows[self.offset.min(end)..end])
    }

    /// 上 / 下に隠れている行数。
    pub fn hidden(&self) -> (usize, usize) {
        (
            self.offset,
            self.rows.len().saturating_sub(self.offset + self.view_h),
        )
    }

    /// 全部の行（見えていない分も含む）。テストと数え上げ用。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// その行が選べるか（見出しは選べない）。
    fn item_at(&self, row: usize) -> Option<&Item> {
        match self.rows.get(row) {
            Some(Row::Item(i)) => Some(i),
            _ => None,
        }
    }

    pub fn hide(&mut self) {
        self.open = false;
    }

    /// 表示に要する幅（セル）。
    pub fn width(&self) -> usize {
        self.rows
            .iter()
            .map(|r| match r {
                Row::Header(h) => display_width(h) + 4,
                Row::Item(i) => display_width(i.title) + display_width(&i.keys) + 6,
            })
            .max()
            .unwrap_or(24)
            .max(24)
    }

    /// 描く高さ。`fit()` を通したあとは窓に入る高さ。
    pub fn height(&self) -> usize {
        if self.view_h == 0 {
            self.rows.len()
        } else {
            self.view_h
        }
    }

    /// 画面座標がメニューの何行目か（一覧の先頭を 0 とする）。外なら `None`。
    pub fn row_at(&self, col: usize, row: usize) -> Option<usize> {
        let (x, y) = self.at;
        let inside = col >= x && col < x + self.width() && row >= y && row < y + self.height();
        inside.then(|| self.offset + (row - y))
    }

    pub fn hover(&mut self, col: usize, row: usize) -> Action {
        // 見出しの上ではどれも選ばれていない状態にする（押せないので）
        let next = self
            .row_at(col, row)
            .filter(|r| matches!(self.rows.get(*r), Some(Row::Item(_))));
        if next == self.selected {
            return Action::None;
        }
        self.selected = next;
        Action::Redraw
    }

    pub fn click(&mut self, col: usize, row: usize) -> Action {
        let Some(r) = self.row_at(col, row) else {
            // 外を押したら閉じる。これが無いとメニューから抜けられない。
            return Action::Close;
        };
        match self.item_at(r) {
            Some(item) => Action::Run(item.id),
            None => Action::None,
        }
    }

    /// キーで動かすとき、見出しを飛ばして次の項目へ。
    pub fn step(&mut self, delta: isize) -> Action {
        if self.rows.is_empty() {
            return Action::None;
        }
        let n = self.rows.len() as isize;
        let mut i = self.selected.map_or(-1, |s| s as isize);
        for _ in 0..n {
            i = (i + delta).rem_euclid(n);
            if matches!(self.rows[i as usize], Row::Item(_)) {
                self.selected = Some(i as usize);
                scroll_into_view(&mut self.offset, i as usize, self.view_h);
                self.snap_header();
                return Action::Redraw;
            }
        }
        Action::None
    }

    /// 先頭が見出しの直下なら、見出しごと見せる。
    /// 一番上の項目を選んだときに「編集」だけ切れて見えるのを避ける。
    fn snap_header(&mut self) {
        if self.offset == 0 || self.view_h == 0 {
            return;
        }
        if !matches!(self.rows.get(self.offset - 1), Some(Row::Header(_))) {
            return;
        }
        let sel = self.selected.unwrap_or(self.offset);
        if sel < self.offset - 1 + self.view_h {
            self.offset -= 1;
        }
    }

    /// 今選んでいる項目の id。
    pub fn chosen(&self) -> Option<&'static str> {
        self.selected.and_then(|r| self.item_at(r)).map(|i| i.id)
    }
}

// ---------------------------------------------------------------------------
// 名前を選ぶだけの一覧
// ---------------------------------------------------------------------------

/// セッションのように、**レジストリに載っていないものを選ぶ**一覧。
///
/// パレットと分けているのは、パレットの項目が `&'static str` の id を持つのに対し、
/// こちらは実行時に決まる名前だから。無理に同じ型へ押し込むと id が嘘になる。
#[derive(Default)]
pub struct Picker {
    pub open: bool,
    pub title: String,
    pub items: Vec<String>,
    pub selected: usize,
    /// 選んだあと何をするか。**選ぶ一覧と行き先を 1 つにしておく**ので、
    /// 呼び出し側が「いまはセッションの一覧のはず」と覚えておかなくていい。
    pub kind: PickKind,
    offset: usize,
}

/// パレットの入力欄が何を受け取っているか。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PaletteKind {
    #[default]
    Command,
    /// 検索。`back` なら後ろ向き。
    Search { back: bool },
}

/// 選んだものの扱い。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PickKind {
    #[default]
    Session,
    /// ファイルパス。選んだらそのペインで開く。
    Path,
    /// 補完の候補。選んだら打ちかけの語と入れ替える。
    Completion,
    /// 串刺し検索の結果（`パス:行:本文`）。選んだらその行を開く。
    Grep,
}

impl Picker {
    pub fn show(&mut self, title: &str, items: Vec<String>, kind: PickKind) {
        self.title = title.to_string();
        self.items = items;
        self.kind = kind;
        self.selected = 0;
        self.offset = 0;
        self.open = true;
    }

    /// 高さ `height` の窓に収まる範囲。
    pub fn view(&mut self, height: usize) -> (usize, &[String]) {
        scroll_into_view(&mut self.offset, self.selected, height);
        let end = (self.offset + height).min(self.items.len());
        (self.offset, &self.items[self.offset.min(end)..end])
    }

    pub fn hide(&mut self) {
        self.open = false;
    }

    pub fn move_by(&mut self, delta: isize) -> Action {
        if self.items.is_empty() {
            return Action::None;
        }
        let n = self.items.len() as isize;
        self.selected = ((self.selected as isize + delta).rem_euclid(n)) as usize;
        Action::Redraw
    }

    pub fn accept(&mut self) -> Action {
        match self.items.get(self.selected) {
            Some(name) => Action::Pick(name.clone()),
            None => Action::Close,
        }
    }

    /// 一覧の何行目か（**見えている**先頭を 0 とする）。外なら閉じる。
    pub fn click(&mut self, row: usize) -> Action {
        match self.items.get(self.offset + row) {
            Some(name) => Action::Pick(name.clone()),
            None => Action::Close,
        }
    }
}

/// 範囲が無いと意味を持たない操作か。
fn needs_range(id: &str) -> bool {
    matches!(id, "op.yank" | "op.send_to_prompt" | "register.select")
}

fn display_width(s: &str) -> usize {
    s.chars().map(tsg_term::width_of).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_palette_lists_everything_that_declares_itself_palette_reachable() {
        let mut p = Palette::default();
        p.show();
        let listed = p.items().len();
        let declared = REGISTRY.iter().filter(|s| s.in_palette).count();
        assert_eq!(listed, declared, "パレットに出ないコマンドがある");
        assert!(listed > 10, "レジストリを読めていない");
    }

    #[test]
    fn the_picker_returns_the_name_that_was_chosen() {
        let mut p = Picker::default();
        p.show(
            "セッション",
            vec!["default".into(), "作業:1".into()],
            PickKind::Session,
        );
        p.move_by(1);
        assert_eq!(p.accept(), Action::Pick("作業:1".into()));
        assert_eq!(p.click(0), Action::Pick("default".into()));
        assert_eq!(p.click(9), Action::Close, "外を押したら閉じる");
    }

    #[test]
    fn typing_filters_and_enter_runs_the_selection() {
        let mut p = Palette::default();
        p.show();
        for c in "でたっち".chars() {
            p.push(c);
        }
        assert_eq!(p.items().len(), 0, "日本語の部分一致は題名にしか無い");

        p.show();
        for c in "detach".chars() {
            p.push(c);
        }
        assert_eq!(p.items().len(), 1);
        assert_eq!(p.accept(), Action::Run("layout.detach"));
    }

    #[test]
    fn filtering_matches_the_key_binding_too() {
        // 「あのキーなんだっけ」から引けること
        let mut p = Palette::default();
        p.show();
        for c in "F1".chars() {
            p.push(c);
        }
        assert!(p.items().iter().any(|i| i.id == "ui.help"));
    }

    #[test]
    fn selection_wraps_and_stays_in_range() {
        let mut p = Palette::default();
        p.show();
        let n = p.items().len();
        p.move_by(-1);
        assert_eq!(p.selected, n - 1, "上に戻ると末尾へ回らない");
        p.move_by(1);
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn backspace_on_an_empty_query_closes_it() {
        let mut p = Palette::default();
        p.show();
        p.push('x');
        assert_eq!(p.backspace(), Action::Redraw);
        assert_eq!(p.backspace(), Action::Close);
    }

    #[test]
    fn accepting_with_no_match_closes_instead_of_running_something_else() {
        let mut p = Palette::default();
        p.show();
        for c in "zzzzz".chars() {
            p.push(c);
        }
        assert!(p.items().is_empty());
        assert_eq!(p.accept(), Action::Close);
    }

    fn menu_ids(m: &Menu) -> Vec<&'static str> {
        m.rows()
            .iter()
            .filter_map(|r| match r {
                Row::Item(i) => Some(i.id),
                Row::Header(_) => None,
            })
            .collect()
    }

    #[test]
    fn the_menu_hides_operations_that_need_a_selection() {
        let mut m = Menu::default();
        m.show((0, 0), false);
        assert!(
            !menu_ids(&m).contains(&"op.yank"),
            "選択が無いのにコピーを出している"
        );

        m.show((0, 0), true);
        assert!(menu_ids(&m).contains(&"op.yank"));
    }

    #[test]
    fn the_menu_always_offers_the_palette() {
        // これがメニューに無いと「メニューに出ていない機能」へマウスで届かなくなる
        let mut m = Menu::default();
        m.show((0, 0), false);
        assert_eq!(menu_ids(&m).last().copied(), Some(OPEN_PALETTE));
    }

    /// 見出しは選べない。キーで動かしたときに空行で止まると、
    /// 「押しても何も起きない」に見える。
    #[test]
    fn headings_are_never_selectable() {
        let mut m = Menu::default();
        m.show((0, 0), true);
        assert!(
            matches!(m.rows().first(), Some(Row::Header(_))),
            "見出しから始まっていない"
        );
        m.step(1);
        assert!(matches!(m.rows()[m.selected.unwrap()], Row::Item(_)));

        // 見出しの行を押しても閉じない・実行しない
        let (x, y) = m.at;
        assert_eq!(m.click(x + 1, y), Action::None);
    }

    #[test]
    fn clicking_outside_the_menu_closes_it() {
        let mut m = Menu::default();
        m.show((10, 5), true);
        // 先頭は見出しなので、その次（最初の項目）を押す
        assert!(matches!(m.click(10, 6), Action::Run(_)));
        assert_eq!(m.click(0, 0), Action::Close, "外を押しても閉じない");
    }

    #[test]
    fn the_menu_is_wide_enough_for_its_widest_row() {
        let mut m = Menu::default();
        m.show((0, 0), true);
        let widest = m
            .rows()
            .iter()
            .map(|r| match r {
                Row::Header(h) => display_width(h),
                Row::Item(i) => display_width(i.title) + display_width(&i.keys),
            })
            .max()
            .unwrap();
        assert!(m.width() > widest, "題名が枠からはみ出す");
    }

    // -- 窓に入り切らないときの回帰 -----------------------------------------
    //
    // M8 まで、どの一覧も「頭から N 件を切り取る」だけだった。↓ で N 件目より
    // 下へ動くと選択が画面の外へ消え、Enter が何を実行するのか分からなくなる。

    #[test]
    fn the_palette_keeps_the_selection_inside_the_window() {
        let mut p = Palette::default();
        p.show();
        let h = 5;
        for _ in 0..20 {
            p.move_by(1);
            let sel = p.selected;
            let (start, view) = p.view(h);
            assert!(view.len() <= h);
            assert!(
                sel >= start && sel < start + view.len(),
                "選んだ {sel} 件目が窓 {start}..{} の外へ出た",
                start + view.len()
            );
        }
    }

    #[test]
    fn a_palette_click_lands_on_the_row_you_can_see() {
        let mut p = Palette::default();
        p.show();
        let h = 5;
        for _ in 0..8 {
            p.move_by(1);
        }
        let (start, view) = p.view(h);
        let want = view[1].id;
        assert!(start > 0, "この時点では窓は動いているはず");
        assert_eq!(
            p.click(1),
            Action::Run(want),
            "見えている行と実行が食い違う"
        );
    }

    #[test]
    fn the_picker_keeps_the_selection_inside_the_window() {
        let mut p = Picker::default();
        p.show(
            "s",
            (0..30).map(|i| format!("s{i}")).collect(),
            PickKind::Session,
        );
        for _ in 0..25 {
            p.move_by(1);
            let sel = p.selected;
            let (start, view) = p.view(6);
            assert!(sel >= start && sel < start + view.len(), "選択が窓の外");
        }
        p.move_by(-1);
        let sel = p.selected;
        let (start, view) = p.view(6);
        assert_eq!(view[sel - start], format!("s{sel}"));
    }

    #[test]
    fn every_menu_item_is_reachable_in_a_short_window() {
        let mut m = Menu::default();
        m.show((0, 0), true);
        let total = m.rows().len();
        m.fit(6);
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..total * 2 {
            m.step(1);
            let sel = m.selected.unwrap();
            let (start, view) = m.view();
            assert!(
                sel >= start && sel < start + view.len(),
                "選んだ行が見えていない"
            );
            seen.insert(sel);
        }
        let items = m
            .rows()
            .iter()
            .filter(|r| matches!(r, Row::Item(_)))
            .count();
        assert_eq!(seen.len(), items, "たどり着けない項目がある");
    }

    #[test]
    fn a_menu_click_lands_on_the_row_you_can_see_after_scrolling() {
        let mut m = Menu::default();
        m.show((0, 2), true);
        m.fit(6);
        for _ in 0..10 {
            m.step(1);
        }
        let (start, view) = m.view();
        assert!(start > 0);
        // 画面の 3 行目（y=2 の 1 つ下）を押したら、そこに見えている項目が動く
        let want = match &view[1] {
            Row::Item(i) => i.id,
            Row::Header(_) => unreachable!("見出しの直後は項目"),
        };
        assert_eq!(m.click(2, 3), Action::Run(want));
    }
}
