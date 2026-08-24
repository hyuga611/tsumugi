//! 設定ファイル。
//!
//! **無くても動く**ことを最優先にしている。壊れた設定でターミナルが開かないのは
//! 端末エミュレータとして最悪の事故なので、読めなければ警告を出して既定へ倒す。

use std::path::PathBuf;

use serde::Deserialize;
use tsg_modal::Lang;

/// 既定で少し透かす。**背景がぼけて後ろが透ける**のが標準の見た目。
///
/// 不透明が既定だと「透過は設定でできる」で終わってしまい、
/// 初めて開いた人がこの端末の見た目を知らないまま使うことになる。
pub const DEFAULT_OPACITY: f32 = 0.90;
pub const DEFAULT_FONT_SIZE: f32 = 18.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub opacity: f32,
    pub blur: bool,
    pub font_size: f32,
    /// 表示の言語。設定に無ければ OS の言語を見る。
    pub lang: Lang,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            opacity: DEFAULT_OPACITY,
            blur: true,
            font_size: DEFAULT_FONT_SIZE,
            lang: detect_lang(),
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
}

#[derive(Debug, Default, Deserialize)]
struct Ui {
    /// `"ja"` / `"en"` / `"auto"`。
    lang: Option<String>,
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
            Ok(f) => (Self::from_file(&f), None),
            Err(e) => (
                Self::default(),
                Some(format!("{} が読めません: {e}", p.display())),
            ),
        }
    }

    fn from_file(f: &File) -> Self {
        let d = Self::default();
        Self {
            opacity: f.window.opacity.unwrap_or(d.opacity).clamp(0.2, 1.0),
            blur: f.window.blur.unwrap_or(d.blur),
            font_size: f.font.size.unwrap_or(d.font_size).clamp(6.0, 96.0),
            lang: f
                .ui
                .lang
                .as_deref()
                .and_then(Lang::parse)
                .unwrap_or(d.lang),
        }
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
        Config::from_file(&toml::from_str::<File>(s).expect("読めない"))
    }

    #[test]
    fn an_empty_file_is_the_default() {
        assert_eq!(parsed(""), Config::default());
    }

    #[test]
    fn a_partial_file_only_overrides_what_it_names() {
        let c = parsed("[window]\nopacity = 0.8\n");
        assert_eq!(c.opacity, 0.8);
        assert_eq!(c.blur, Config::default().blur, "書いていない項目まで変わっている");
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
        assert_eq!(parsed("[ui]\nlang = \"auto\"\n").lang, Config::default().lang);
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
