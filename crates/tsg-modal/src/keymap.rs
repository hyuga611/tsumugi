//! キーの割り当てを差し替える。
//!
//! **既定を消さない。上に重ねるだけ。** 設定に書いた分だけが既定より先に
//! 見られ、書かなかったものは今までどおり動く。全部を書き直させる形にすると、
//! 新しいコマンドが増えるたびに設定が古くなる。
//!
//! 割り当て先は**コマンドの id**（`tsg --commands` で出るもの）。モーションや
//! オペレータのような「引数を取る動き」には割り当てない — `d` を別のキーに
//! すると `d` を待つ状態がどこにも無くなり、モーダルの筋が通らなくなる。
//! ここで差し替えられるのは**それ単体で完結するコマンド**だけ。

use std::collections::BTreeMap;

use crate::REGISTRY;
use crate::engine::KeyInput;

/// 設定から読んだ割り当て。空なら既定のまま。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Keymap {
    /// 読むモードで押されたとき。
    normal: BTreeMap<KeyInput, &'static str>,
    /// 入力モードで押されたとき。**ここは慎重に。** 塞ぐと字が打てなくなる。
    insert: BTreeMap<KeyInput, &'static str>,
}

/// 割り当てを書く場所（設定の `[keys]` / `[keys.insert]`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    Normal,
    Insert,
}

impl Keymap {
    pub fn is_empty(&self) -> bool {
        self.normal.is_empty() && self.insert.is_empty()
    }

    /// 1 つ足す。読めない書き方・知らない id は**理由を返して足さない**。
    ///
    /// 黙って捨てると「書いたのに効かない」で終わる。設定の間違いは
    /// 起動時に画面へ出す（`config.rs` と同じ方針）。
    pub fn add(&mut self, when: When, key: &str, id: &str) -> Result<(), String> {
        let Some(k) = parse_key(key) else {
            return Err(format!("キー '{key}' を読めません"));
        };
        let Some(spec) = REGISTRY.iter().find(|s| s.id == id) else {
            return Err(format!("コマンド '{id}' を知りません"));
        };
        // 入力モードで素の 1 文字を奪うと、その字が打てなくなる。
        if when == When::Insert && matches!(k, KeyInput::Char(_)) {
            return Err(format!(
                "入力モードでは '{key}' のような素の字に割り当てられません（Ctrl や F キーにしてください）"
            ));
        }
        match when {
            When::Normal => self.normal.insert(k, spec.id),
            When::Insert => self.insert.insert(k, spec.id),
        };
        Ok(())
    }

    /// そのキーに割り当てられたコマンド。無ければ `None`（既定へ進む）。
    pub fn lookup(&self, when: When, key: KeyInput) -> Option<&'static str> {
        match when {
            When::Normal => self.normal.get(&key).copied(),
            When::Insert => self.insert.get(&key).copied(),
        }
    }
}

/// `"ctrl+p"` `"F5"` `"space"` `"g"` のような書き方を読む。
///
/// **人が書く形に寄せる。** `<C-p>` と `ctrl+p` の両方を通す。
pub fn parse_key(s: &str) -> Option<KeyInput> {
    let t = s.trim().trim_start_matches('<').trim_end_matches('>');
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();

    // Ctrl 付き（`ctrl+p` / `c-p` / `^p`）
    for prefix in ["ctrl+", "ctrl-", "c-", "^"] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            let c = rest.chars().next()?;
            return (rest.chars().count() == 1 && c.is_ascii_alphabetic())
                .then_some(KeyInput::Ctrl(c));
        }
    }
    // F キー
    if let Some(n) = lower.strip_prefix('f')
        && let Ok(n) = n.parse::<u8>()
        && (1..=12).contains(&n)
    {
        return Some(KeyInput::Function(n));
    }
    match lower.as_str() {
        "esc" | "escape" => return Some(KeyInput::Esc),
        "enter" | "cr" | "return" => return Some(KeyInput::Enter),
        "tab" => return Some(KeyInput::Tab),
        "backspace" | "bs" => return Some(KeyInput::Backspace),
        "space" => return Some(KeyInput::Char(' ')),
        _ => {}
    }
    // 素の 1 文字。**大文字小文字は区別する**（`g` と `G` は別のキー）。
    let mut chars = t.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some(KeyInput::Char(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ways_people_write_keys_all_parse() {
        assert_eq!(parse_key("ctrl+p"), Some(KeyInput::Ctrl('p')));
        assert_eq!(parse_key("<C-p>"), Some(KeyInput::Ctrl('p')));
        assert_eq!(parse_key("^p"), Some(KeyInput::Ctrl('p')));
        assert_eq!(parse_key("F5"), Some(KeyInput::Function(5)));
        assert_eq!(parse_key("esc"), Some(KeyInput::Esc));
        assert_eq!(parse_key("space"), Some(KeyInput::Char(' ')));
        assert_eq!(parse_key("g"), Some(KeyInput::Char('g')));
        // 大文字は別のキー
        assert_ne!(parse_key("G"), parse_key("g"));
        assert_eq!(parse_key("nonsense"), None);
        assert_eq!(parse_key(""), None);
    }

    /// **書いたのに効かない**を作らない。読めない設定は理由を返す。
    #[test]
    fn a_binding_that_cannot_work_is_refused_with_a_reason() {
        let mut m = Keymap::default();
        assert!(
            m.add(When::Normal, "zzz", "ui.help").is_err(),
            "読めないキー"
        );
        assert!(
            m.add(When::Normal, "g", "no.such.command").is_err(),
            "知らないコマンド"
        );
        // 入力モードで素の字を奪うと、その字が打てなくなる
        assert!(m.add(When::Insert, "a", "ui.help").is_err());
        assert!(m.add(When::Insert, "ctrl+g", "ui.help").is_ok());
    }

    #[test]
    fn what_you_bind_is_what_you_get() {
        let mut m = Keymap::default();
        m.add(When::Normal, "ctrl+k", "search.open")
            .expect("足せない");
        assert_eq!(
            m.lookup(When::Normal, KeyInput::Ctrl('k')),
            Some("search.open")
        );
        // 書かなかったキーは既定へ進む
        assert_eq!(m.lookup(When::Normal, KeyInput::Char('j')), None);
        // モードをまたがない
        assert_eq!(m.lookup(When::Insert, KeyInput::Ctrl('k')), None);
    }
}
