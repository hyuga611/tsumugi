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

// ---------------------------------------------------------------------------
// コマンドパレット
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct Palette {
    pub open: bool,
    pub query: String,
    pub selected: usize,
    items: Vec<Item>,
}

impl Palette {
    pub fn show(&mut self) {
        self.open = true;
        self.query.clear();
        self.selected = 0;
        self.refresh();
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
    }

    pub fn push(&mut self, c: char) -> Action {
        self.query.push(c);
        self.selected = 0;
        self.refresh();
        Action::Redraw
    }

    pub fn backspace(&mut self) -> Action {
        if self.query.pop().is_none() {
            return Action::Close;
        }
        self.selected = 0;
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

    /// 一覧のどの行がクリックされたか。`row` は一覧の先頭を 0 とした行。
    pub fn click(&mut self, row: usize) -> Action {
        match self.items.get(row) {
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
        self.open = true;
    }

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

    pub fn height(&self) -> usize {
        self.rows.len()
    }

    /// 画面座標がメニューの何行目か。外なら `None`。
    pub fn row_at(&self, col: usize, row: usize) -> Option<usize> {
        let (x, y) = self.at;
        let inside = col >= x && col < x + self.width() && row >= y && row < y + self.height();
        inside.then(|| row - y)
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
                return Action::Redraw;
            }
        }
        Action::None
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
}

impl Picker {
    pub fn show(&mut self, title: &str, items: Vec<String>) {
        self.title = title.to_string();
        self.items = items;
        self.selected = 0;
        self.open = true;
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

    /// 一覧の何行目か（先頭を 0 とする）。外なら閉じる。
    pub fn click(&mut self, row: usize) -> Action {
        match self.items.get(row) {
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
    s.chars()
        .map(|c| tsg_term::char_width(c, tsg_term::AmbiguousWidth::Wide))
        .sum()
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
        p.show("セッション", vec!["default".into(), "作業:1".into()]);
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
}
