//! 表示する言葉。日本語と英語。
//!
//! **言語はプロセス全体で 1 つ**。設定で決まって起動中は変わらないので、
//! すべての関数へ引き回すより 1 か所に置くほうが読みやすい。
//! ここを見るのは**表示用の文字列だけ**で、モード機械の判断は言語に依存しない
//! （`arch.md` の不変条件 2「`tsg-modal` は純粋」は保たれる）。
//!
//! 訳は `t!` マクロで**同じ行に並べて**書く。別ファイルの表に逃がすと、
//! 片方だけ直してもう片方が古いまま、が必ず起きる。

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Lang {
    #[default]
    Ja,
    En,
}

impl Lang {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ja" | "jp" | "japanese" | "日本語" => Some(Lang::Ja),
            "en" | "english" => Some(Lang::En),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Lang::Ja => "ja",
            Lang::En => "en",
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

pub fn set_lang(lang: Lang) {
    CURRENT.store(lang as u8, Ordering::Relaxed);
}

pub fn lang() -> Lang {
    if CURRENT.load(Ordering::Relaxed) == Lang::En as u8 {
        Lang::En
    } else {
        Lang::Ja
    }
}

/// 日本語と英語を並べて置く。
///
/// ```ignore
/// t!("保存しました", "saved")
/// t!(format!("マーク {n} を置きました"), format!("mark {n} set"))
/// ```
#[macro_export]
macro_rules! t {
    ($ja:expr, $en:expr $(,)?) => {
        if $crate::text::lang() == $crate::text::Lang::En {
            $en
        } else {
            $ja
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_map_both_ways() {
        assert_eq!(Lang::parse("EN"), Some(Lang::En));
        assert_eq!(Lang::parse("日本語"), Some(Lang::Ja));
        assert_eq!(Lang::parse("fr"), None);
        assert_eq!(Lang::Ja.name(), "ja");
    }

    /// 既定は日本語。設定が読めなくても言葉が消えないこと。
    #[test]
    fn the_default_is_japanese() {
        assert_eq!(Lang::default(), Lang::Ja);
    }
}
