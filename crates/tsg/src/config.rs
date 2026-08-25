//! 設定ファイル。
//!
//! **無くても動く**ことを最優先にしている。壊れた設定でターミナルが開かないのは
//! 端末エミュレータとして最悪の事故なので、読めなければ警告を出して既定へ倒す。

use std::path::PathBuf;

use serde::Deserialize;
use tsg_modal::Lang;
use tsg_term::AmbiguousWidth;

use crate::theme::{self, Theme};

/// 既定で少し透かす。**背景がぼけて後ろが透ける**のが標準の見た目。
///
/// 不透明が既定だと「透過は設定でできる」で終わってしまい、
/// 初めて開いた人がこの端末の見た目を知らないまま使うことになる。
pub const DEFAULT_OPACITY: f32 = 0.85;
pub const DEFAULT_FONT_SIZE: f32 = 18.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub opacity: f32,
    pub blur: bool,
    pub font_size: f32,
    /// 合字を組むか（`->` を 1 つの字形にする）。
    pub ligatures: bool,
    /// 使う字体の名前。無ければ OS ごとの既定の候補から探す。
    ///
    /// **見つからなくても開く。** 名前を間違えただけで端末が起動しないのは、
    /// 端末エミュレータとして最悪の事故。
    pub font_family: Option<String>,
    /// East Asian Ambiguous を 1 幅で数えるか 2 幅で数えるか（`arch.md` §6.1）。
    ///
    /// **これも読み直しでは変えない。** 桁の勘定そのものが変わるので、
    /// 途中で切り替えると既に組んだグリッドと食い違う。
    pub ambiguous_width: AmbiguousWidth,
    /// スクロールバックに残す行数。
    pub scrollback: usize,
    /// 表示の言語。設定に無ければ OS の言語を見る。
    ///
    /// **これだけは読み直しで変えない。** 言語はプロセス全体で 1 つと決めてあり
    /// （`t!` が起動時の値を見る）、途中で変えると画面の一部だけ古い言語のまま残る。
    pub lang: Lang,
    /// 使うテーマの名前。表示と `:theme` のために持つ。
    pub theme_name: String,
    pub theme: Theme,
    /// 案内の一行を出すか（「そのまま打てます」「Esc で読むモードへ」など）。
    ///
    /// **慣れた人には邪魔になる。** 覚えてしまえば読む必要が無く、
    /// 下の行がずっと文字で埋まっているだけになる。切れるようにしておく。
    /// 結果の知らせ（「保存しました」）は案内ではないので、切っても出る。
    pub guides: bool,
    /// ファイルを開いたときに左へ行番号を出すか。
    ///
    /// **端末には出さない。** 端末の「行」はコマンドの出力が積み上がった
    /// もので、番号を振っても指す先が無い。
    pub line_numbers: bool,
    /// 差し替えたキー。**既定は消さず、上に重ねる**（`keymap.rs`）。
    pub keys: tsg_modal::Keymap,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            opacity: DEFAULT_OPACITY,
            blur: true,
            font_size: DEFAULT_FONT_SIZE,
            ligatures: true,
            font_family: None,
            ambiguous_width: AmbiguousWidth::Narrow,
            scrollback: tsg_term::grid::DEFAULT_MAX_SCROLLBACK,
            lang: detect_lang(),
            theme_name: theme::DEFAULT_THEME.to_string(),
            theme: Theme::default(),
            guides: true,
            line_numbers: true,
            keys: tsg_modal::Keymap::default(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct File {
    #[serde(default)]
    window: Window,
    #[serde(default)]
    font: Font,
    #[serde(default)]
    ui: Ui,
    #[serde(default)]
    scrollback: Scrollback,
    #[serde(default)]
    theme: ThemeFile,
    /// `[keys]` は読むモード、`[keys.insert]` は入力モード。
    #[serde(default)]
    keys: KeysFile,
}

#[derive(Debug, Default, Deserialize)]
struct KeysFile {
    /// 入力モードでの割り当て。**素の 1 字は受け取らない**（打てなくなる）。
    #[serde(default)]
    insert: std::collections::BTreeMap<String, String>,
    /// 読むモードでの割り当て。`"ctrl+k" = "search.open"` のように書く。
    #[serde(flatten)]
    normal: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
struct Scrollback {
    lines: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
struct ThemeFile {
    /// 組み込みテーマの名前。
    name: Option<String>,
    /// 個別の色の上書き。`#rrggbb` / `#rrggbbaa` / `#rgb`。
    ///
    /// 知らない名前と読めない値は**黙って捨てず、警告に出す**。
    /// 綴りを間違えたときに「書いたのに効かない」で終わらせない。
    #[serde(default)]
    colors: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
struct Ui {
    /// `"ja"` / `"en"` / `"auto"`。
    lang: Option<String>,
    /// 案内の一行を出すか。
    guides: Option<bool>,
    /// ファイルを開いたときに行番号を出すか。
    line_numbers: Option<bool>,
}

/// `ambiguous_width` を読む。`"narrow"` / `"wide"` と `1` / `2` の両方を通す。
///
/// **知らない値は黙って捨てない。** 書いたのに効かないのが一番困る。
fn parse_ambiguous(v: Option<&toml::Value>) -> Result<Option<AmbiguousWidth>, String> {
    let Some(v) = v else {
        return Ok(None);
    };
    match v {
        toml::Value::Integer(1) => Ok(Some(AmbiguousWidth::Narrow)),
        toml::Value::Integer(2) => Ok(Some(AmbiguousWidth::Wide)),
        toml::Value::String(s) if s.eq_ignore_ascii_case("narrow") => {
            Ok(Some(AmbiguousWidth::Narrow))
        }
        toml::Value::String(s) if s.eq_ignore_ascii_case("wide") => Ok(Some(AmbiguousWidth::Wide)),
        other => Err(format!(
            "ambiguous_width = {other} を読めません（\"narrow\" / \"wide\" / 1 / 2）"
        )),
    }
}

/// OS の言語を見る。分からなければ日本語（作者の既定）。
///
/// 知らない言語のときに英語へ倒すか日本語へ倒すかは趣味の問題だが、
/// **設定 1 行で変えられる**ので、迷ったら書いてもらえばよい。
fn detect_lang() -> Lang {
    let Some(loc) = crate::platform::ui_language() else {
        return Lang::Ja;
    };
    if loc.to_ascii_lowercase().starts_with("ja") {
        Lang::Ja
    } else {
        Lang::En
    }
}

#[derive(Debug, Default, Deserialize)]
struct Window {
    opacity: Option<f32>,
    blur: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct Font {
    size: Option<f32>,
    family: Option<String>,
    ligatures: Option<bool>,
    /// `"narrow"` / `"wide"`、または `1` / `2`。
    ambiguous_width: Option<toml::Value>,
}

/// 設定ファイルの場所。
pub fn path() -> Option<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    Some(base?.join("tsumugi").join("config.toml"))
}

/// まだ設定ファイルが無いときに置く雛形。
///
/// **空のファイルを開いても何も分からない。** 何が書けて、いまの既定が何かを
/// その場で読めるようにするために、すべてコメントで並べておく。
/// 全部コメントなので、読み直しても既定のままで、消しても壊れない。
pub fn template() -> String {
    let d = Config::default();
    format!(
        r##"# tsumugi の設定。すべて任意です。
# 行頭の # を外すと効きます。保存した瞬間に反映されます（言語だけは次の起動から）。
# 書けるものの一覧は tsg --help にもあります。

[window]
# 背景の不透明度。1.0 で不透明。
# opacity = {opacity}
# 背景をぼかす（Windows 11 / macOS）。
# blur = {blur}

[font]
# 文字の大きさ（px）。Ctrl＋ホイールでも変えられます。
# size = {size}
# 使う字体。見つからなければ既定の候補から探します（開かなくなりません）。
# family = "Cascadia Code"
# -> や != を 1 つの字形に組む（字体が持っていれば）。
# ligatures = {lig}
# 罫線素片・記号など East Asian Ambiguous を何幅で数えるか。
# "narrow"（1・既定。TUI が崩れない）/ "wide"（2・日本語端末の古い慣習）。
# ambiguous_width = "narrow"

[ui]
# 表示の言語。"ja" / "en" / "auto"（既定は OS に合わせる）。
# lang = "auto"
# 下の行に出る案内（「そのまま打てます」「Esc で読むモードへ」など）。
# 覚えてしまったら false に。保存や失敗の知らせは、切っても出ます。
# guides = {guides}
# ファイルを開いたとき、左に行番号を出す（端末の画面には出ません）。
# line_numbers = {nums}

[scrollback]
# さかのぼって読める行数。
# lines = {sb}

[keys]
# キーを差し替える。左がキー、右がコマンドの id（一覧は tsg --commands）。
# **書いた分だけ既定より先に見ます。** 書かなかったキーは今までどおりです。
# "ctrl+k" = "search.open"
# "F5"     = "git.diff"
#
# [keys.insert]
# 入力モードは Ctrl や F キーだけ（素の字を奪うと、その字が打てません）。
# "ctrl+g" = "agent.next"

[theme]
# 配色の名前。{themes}
# name = "{theme}"

# 個別の色だけ変えたいとき。#rrggbb / #rrggbbaa / #rgb。
# [theme.colors]
# background = "#11131a"
# foreground = "#d8dee9"
# accent     = "#e0a54a"
"##,
        opacity = d.opacity,
        blur = d.blur,
        size = d.font_size,
        lig = d.ligatures,
        guides = d.guides,
        nums = d.line_numbers,
        sb = d.scrollback,
        theme = d.theme_name,
        themes = theme::names().join(" / "),
    )
}

/// 使い方を一度でも出したか、の目印。
///
/// **設定と同じところに置く。** 実行時の一時領域だと再起動のたびに
/// 「初回」へ戻り、毎回全画面で出てしまう。
fn welcome_mark() -> Option<PathBuf> {
    Some(path()?.with_file_name("welcomed"))
}

/// 初回なら真を返し、**同時に目印を残す**。
///
/// 置き場所が分からない / 書けない環境では、いつも真になる（今までと同じ）。
/// 出ないより、出すぎるほうがまし。
pub fn take_first_run() -> bool {
    welcome_mark().is_none_or(|m| take_first_run_at(&m))
}

/// 前回の窓の大きさ。**次に開いたとき同じ大きさで出す。**
///
/// 毎回既定の大きさに戻る端末は、開くたびに窓を引き伸ばすことになる。
/// 設定に書かせるものではない（書きたい人は書けるが、書かない人のほうが多い）。
fn size_mark() -> Option<PathBuf> {
    Some(path()?.with_file_name("window"))
}

/// 覚えておく大きさ（論理ピクセル）。
///
/// **ドラッグ中は書かない。** 窓の端を掴んで動かすと 1 秒に何十回も
/// 呼ばれる。掴んでいる間ずっとファイルを書くのは、得るものに対して高い。
/// 手を離したあとの最後の 1 回が残ればいい。
pub fn remember_size(w: f64, h: f64) {
    remember_size_inner(w, h, false);
}

/// 待たずに書く。閉じる直前に、ドラッグの最後の 1 回を取りこぼさないため。
pub fn remember_size_now(w: f64, h: f64) {
    remember_size_inner(w, h, true);
}

fn remember_size_inner(w: f64, h: f64, force: bool) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static LAST: AtomicU64 = AtomicU64::new(0);

    // 畳んだ / 極端に小さいときは覚えない。次に開けなくなる。
    if !(200.0..=20000.0).contains(&w) || !(150.0..=20000.0).contains(&h) {
        return;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let last = LAST.load(Ordering::Relaxed);
    if !force && now.saturating_sub(last) < 500 {
        return;
    }
    LAST.store(now, Ordering::Relaxed);

    let Some(mark) = size_mark() else {
        return;
    };
    if let Some(dir) = mark.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(mark, format!("{}x{}", w.round(), h.round()));
}

/// 前回の大きさ。無ければ既定。
pub fn last_size() -> Option<(f64, f64)> {
    let text = std::fs::read_to_string(size_mark()?).ok()?;
    let (w, h) = text.trim().split_once('x')?;
    let (w, h) = (w.parse::<f64>().ok()?, h.parse::<f64>().ok()?);
    // 壊れた値で開かない。
    (w >= 200.0 && h >= 150.0 && w <= 20000.0 && h <= 20000.0).then_some((w, h))
}

fn take_first_run_at(mark: &std::path::Path) -> bool {
    if mark.exists() {
        return false;
    }
    if let Some(dir) = mark.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(mark, "");
    true
}

impl Config {
    /// 設定ファイルを読む。無ければ既定。壊れていれば既定＋警告。
    pub fn load() -> (Self, Option<String>) {
        let Some(p) = path() else {
            return (Self::default(), None);
        };
        let Ok(text) = std::fs::read_to_string(&p) else {
            return (Self::default(), None);
        };
        match toml::from_str::<File>(&text) {
            Ok(f) => {
                let (cfg, warn) = Self::from_file(&f);
                (cfg, warn.map(|w| format!("{}: {w}", p.display())))
            }
            Err(e) => (
                Self::default(),
                Some(format!("{} が読めません: {e}", p.display())),
            ),
        }
    }

    fn from_file(f: &File) -> (Self, Option<String>) {
        let d = Self::default();
        let mut bad: Vec<String> = Vec::new();

        // テーマ。知らない名前なら既定のまま進む（**開かないより開く**）。
        let want = f.theme.name.clone().unwrap_or_else(|| d.theme_name.clone());
        let (name, mut theme) = match theme::builtin(&want) {
            Some(t) => (want, t),
            None => {
                bad.push(format!(
                    "テーマ「{want}」を知りません（{}）",
                    theme::names().join(" / ")
                ));
                (d.theme_name.clone(), d.theme)
            }
        };
        for (key, value) in &f.theme.colors {
            match theme::parse_color(value) {
                Some(c) if theme::set_named(&mut theme, key, c) => {}
                Some(_) => bad.push(format!("色の名前「{key}」を知りません")),
                None => bad.push(format!("色「{key} = {value}」を読めません（#rrggbb の形）")),
            }
        }

        // キーの差し替え。読めないものは**理由を出して足さない**。
        let mut keys = tsg_modal::Keymap::default();
        for (k, id) in &f.keys.normal {
            if let Err(e) = keys.add(tsg_modal::KeyWhen::Normal, k, id) {
                bad.push(e);
            }
        }
        for (k, id) in &f.keys.insert {
            if let Err(e) = keys.add(tsg_modal::KeyWhen::Insert, k, id) {
                bad.push(e);
            }
        }

        let cfg = Self {
            opacity: f.window.opacity.unwrap_or(d.opacity).clamp(0.2, 1.0),
            blur: f.window.blur.unwrap_or(d.blur),
            font_size: f.font.size.unwrap_or(d.font_size).clamp(6.0, 96.0),
            ligatures: f.font.ligatures.unwrap_or(d.ligatures),
            font_family: f.font.family.clone().filter(|s| !s.trim().is_empty()),
            ambiguous_width: match parse_ambiguous(f.font.ambiguous_width.as_ref()) {
                Ok(v) => v.unwrap_or(d.ambiguous_width),
                Err(w) => {
                    bad.push(w);
                    d.ambiguous_width
                }
            },
            scrollback: f
                .scrollback
                .lines
                .unwrap_or(d.scrollback)
                .clamp(100, 1_000_000),
            lang: f.ui.lang.as_deref().and_then(Lang::parse).unwrap_or(d.lang),
            guides: f.ui.guides.unwrap_or(d.guides),
            line_numbers: f.ui.line_numbers.unwrap_or(d.line_numbers),
            theme_name: name,
            theme,
            keys,
        };
        (cfg, (!bad.is_empty()).then(|| bad.join(" / ")))
    }

    /// コマンドラインの指定で上書きする。指定が無いものは触らない。
    pub fn override_with(&mut self, cli: &crate::cli::Cli) {
        if let Some(v) = cli.opacity {
            self.opacity = v.clamp(0.2, 1.0);
        }
        if let Some(v) = cli.blur {
            self.blur = v;
        }
        if let Some(v) = cli.font_size {
            self.font_size = v.clamp(6.0, 96.0);
        }
        if let Some(v) = cli.lang.as_deref().and_then(Lang::parse) {
            self.lang = v;
        }
        if let Some(name) = cli.theme.as_deref()
            && let Some(t) = theme::builtin(name)
        {
            self.theme = t;
            self.theme_name = name.to_string();
        }
    }

    /// テーマだけを差し替える。`:theme` と設定の読み直しが使う。
    pub fn set_theme(&mut self, name: &str) -> bool {
        let Some(t) = theme::builtin(name) else {
            return false;
        };
        self.theme = t;
        self.theme_name = name.to_string();
        true
    }

    /// 背景を透かすか。1.0 なら不透明のまま扱う（余計な合成をしない）。
    pub fn transparent(&self) -> bool {
        self.opacity < 0.999
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(s: &str) -> Config {
        Config::from_file(&toml::from_str::<File>(s).expect("読めない")).0
    }

    fn warning(s: &str) -> Option<String> {
        Config::from_file(&toml::from_str::<File>(s).expect("読めない")).1
    }

    /// 使い方は**初回だけ**。2 度目からは出ない。
    #[test]
    fn the_welcome_screen_shows_once() {
        let mark = std::env::temp_dir().join("tsumugi-welcome-test-mark");
        let _ = std::fs::remove_file(&mark);
        assert!(take_first_run_at(&mark), "初回に出ない");
        assert!(!take_first_run_at(&mark), "2 度目にも出る");
        assert!(!take_first_run_at(&mark), "3 度目にも出る");
        let _ = std::fs::remove_file(&mark);
    }

    /// 雛形は**そのままで読めて、既定と同じ**でなければならない。
    /// 「設定ファイルを開く」で置いたものが、開いた瞬間に警告を出すのは論外。
    #[test]
    fn the_template_reads_back_as_the_default() {
        let t = template();
        let cfg = parsed(&t);
        assert_eq!(cfg, Config::default(), "雛形を置くだけで設定が変わる");
        assert_eq!(warning(&t), None, "雛形が警告を出す");
        assert!(
            t.lines().filter(|l| l.starts_with('#')).count() > 10,
            "雛形に説明が足りない"
        );
    }

    #[test]
    fn an_empty_file_is_the_default() {
        assert_eq!(parsed(""), Config::default());
    }

    #[test]
    fn a_partial_file_only_overrides_what_it_names() {
        let c = parsed("[window]\nopacity = 0.8\n");
        assert_eq!(c.opacity, 0.8);
        assert_eq!(
            c.blur,
            Config::default().blur,
            "書いていない項目まで変わっている"
        );
        assert_eq!(c.font_size, DEFAULT_FONT_SIZE);
    }

    #[test]
    fn absurd_values_are_clamped_not_obeyed() {
        // 不透明度 0 は「開いたのに何も見えない」になる。設定で自分の首を絞めさせない。
        let c = parsed("[window]\nopacity = 0.0\n\n[font]\nsize = 900.0\n");
        assert!(c.opacity >= 0.2);
        assert!(c.font_size <= 96.0);
    }

    #[test]
    fn a_broken_file_does_not_stop_the_terminal() {
        assert!(toml::from_str::<File>("[window\nopacity =").is_err());
        // load() は既定＋警告を返す。ここでは既定側の性質だけ固定しておく。
        assert_eq!(Config::default().opacity, DEFAULT_OPACITY);
    }

    #[test]
    fn a_theme_can_be_picked_by_name_and_a_typo_is_reported() {
        let c = parsed(
            "[theme]
name = \"白磁\"
",
        );
        assert_eq!(c.theme_name, "白磁");
        assert_eq!(c.theme, theme::builtin("白磁").expect("引けない"));

        // 知らない名前でも**開く**。ただし黙らない。
        let c = parsed(
            "[theme]
name = \"no-such-theme\"
",
        );
        assert_eq!(
            c.theme,
            Theme::default(),
            "知らない名前で既定に落ちていない"
        );
        let w = warning(
            "[theme]
name = \"no-such-theme\"
",
        )
        .expect("警告が出ていない");
        assert!(w.contains("no-such-theme"), "何が悪いか言っていない: {w}");
    }

    #[test]
    fn individual_colors_can_be_overridden_and_mistakes_are_reported() {
        let c = parsed(
            "[theme.colors]
background = \"#102030\"
ansi1 = \"#ff0000\"
",
        );
        assert_eq!(c.theme.bg, [16.0 / 255.0, 32.0 / 255.0, 48.0 / 255.0, 1.0]);
        assert_eq!(c.theme.ansi[1], [1.0, 0.0, 0.0, 1.0]);

        // **書いたのに効かない**で終わらせない。
        let w = warning(
            "[theme.colors]
backgruond = \"#102030\"
",
        )
        .expect("警告が出ていない");
        assert!(w.contains("backgruond"), "綴り違いを指摘していない: {w}");
        let w = warning(
            "[theme.colors]
background = \"blue\"
",
        )
        .expect("警告が出ていない");
        assert!(w.contains("#rrggbb"), "書き方を教えていない: {w}");
    }

    #[test]
    fn an_absurd_scrollback_is_clamped_not_obeyed() {
        // 0 行だとモーションの行き先が消え、10 億行だとメモリが尽きる。
        assert!(
            parsed(
                "[scrollback]
lines = 0
"
            )
            .scrollback
                >= 100
        );
        assert!(
            parsed(
                "[scrollback]
lines = 99999999999
"
            )
            .scrollback
                <= 1_000_000
        );
    }

    #[test]
    fn the_default_look_is_translucent_and_blurred() {
        let d = Config::default();
        assert!(d.transparent(), "既定で透けていない");
        assert!(d.blur, "既定でぼけていない");
        // 完全不透明にしたときだけ合成をやめる
        let c = parsed("[window]\nopacity = 1.0\n");
        assert!(!c.transparent());
    }

    #[test]
    fn the_language_can_be_pinned_and_bad_values_fall_back() {
        assert_eq!(parsed("[ui]\nlang = \"en\"\n").lang, Lang::En);
        assert_eq!(parsed("[ui]\nlang = \"ja\"\n").lang, Lang::Ja);
        // `auto` も知らない値も、OS を見た既定に落ちる
        assert_eq!(
            parsed("[ui]\nlang = \"auto\"\n").lang,
            Config::default().lang
        );
        assert_eq!(parsed("[ui]\nlang = \"kl\"\n").lang, Config::default().lang);
    }

    #[test]
    fn cli_wins_over_the_file() {
        let mut c = parsed("[window]\nopacity = 0.8\nblur = true\n");
        let cli = crate::cli::parse(
            ["--opacity", "0.5", "--no-blur"]
                .iter()
                .map(|s| (*s).to_string()),
        );
        c.override_with(&cli);
        assert_eq!(c.opacity, 0.5);
        assert!(!c.blur);
    }
}
