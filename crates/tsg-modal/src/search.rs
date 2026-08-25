//! 探すときの当て方。**素の文字列と正規表現の両方**。
//!
//! # なぜ素の文字列が既定なのか
//!
//! 端末で探すものは、たいていパスかエラー文かハッシュで、`.` も `(` も
//! そのままの字として入っている。既定を正規表現にすると、`foo.rs` と
//! 打っただけで `fooXrs` にも当たる。**打った通りに当たる**ほうが、
//! 端末では正しい。正規表現は `g/` で明示的に入る。
//!
//! # 大小の区別
//!
//! 打った字に大文字が混じっていたら区別する（smartcase）。全部小文字なら
//! 区別しない。**「Error」と打った人は Error を探している**が、「error」と
//! 打った人はどちらでもいい、という経験則で、vim 以来広く使われている。

use regex::{Regex, RegexBuilder};

/// 探しているもの。**組み立ては 1 回だけ**（行ごとに組み直すと、
/// 大きな画面で目に見えて遅くなる）。
#[derive(Debug, Clone)]
pub struct Search {
    /// 打たれたそのまま。入力欄に出すために持つ。
    query: String,
    /// 小文字に畳んだもの（素の文字列として当てるとき用）。
    folded: String,
    /// 正規表現として組んだもの。素の文字列なら `None`。
    re: Option<Regex>,
    /// 正規表現を頼まれたが組めなかった。**黙って素の文字列に落ちない**
    /// ように、打った側へ伝えるために持つ。
    pub bad_regex: bool,
    ci: bool,
}

impl PartialEq for Search {
    /// 中身は組み立ての結果でしかないので、打たれたものだけで比べる。
    fn eq(&self, other: &Self) -> bool {
        self.query == other.query && self.re.is_some() == other.re.is_some()
    }
}

impl Search {
    pub fn new(query: &str, as_regex: bool) -> Self {
        // 大文字が混じっていたら区別する。
        let ci = !query.chars().any(char::is_uppercase);
        let re = as_regex
            .then(|| {
                RegexBuilder::new(query)
                    .case_insensitive(ci)
                    // 一行ずつ当てるので、`.` が改行を跨ぐ必要は無い。
                    .size_limit(1 << 20)
                    .build()
                    .ok()
            })
            .flatten();
        Self {
            query: query.to_string(),
            folded: query.to_lowercase(),
            bad_regex: as_regex && re.is_none(),
            re,
            ci,
        }
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn is_regex(&self) -> bool {
        self.re.is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.query.is_empty()
    }

    /// その行に当たるところ（バイト位置の範囲）を前から順に。
    ///
    /// **空の一致は飛ばす。** `a*` のような形はどこにでも当たるので、
    /// そのまま返すと 1 文字ごとに印が付いて画面が読めなくなる。
    pub fn ranges(&self, text: &str) -> Vec<(usize, usize)> {
        if self.query.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        match &self.re {
            Some(re) => {
                for m in re.find_iter(text) {
                    if m.start() != m.end() {
                        out.push((m.start(), m.end()));
                    }
                }
            }
            None => {
                // 大小を無視するときは、畳んだ側で探して同じ位置を使う。
                // **畳んでも長さが変わる字がある**（ß → ss）ので、
                // 畳んだ文字列の位置をそのまま元の文字列へ当てない。
                let hay = if self.ci {
                    std::borrow::Cow::Owned(text.to_lowercase())
                } else {
                    std::borrow::Cow::Borrowed(text)
                };
                let needle = if self.ci { &self.folded } else { &self.query };
                if hay.len() != text.len() {
                    // 長さが変わった。**位置が信用できないので当てない。**
                    // ここへ来るのは ß や ﬁ のような字を含む行だけ。
                    return Vec::new();
                }
                let mut at = 0usize;
                while let Some(i) = hay[at..].find(needle.as_str()) {
                    let start = at + i;
                    out.push((start, start + needle.len()));
                    at = start + needle.len().max(1);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 素の文字列は**打った通り**に当たる。`.` は `.` のまま。
    #[test]
    fn a_plain_query_matches_exactly_what_you_typed() {
        let s = Search::new("foo.rs", false);
        assert_eq!(s.ranges("foo.rs"), vec![(0, 6)]);
        assert!(s.ranges("fooXrs").is_empty(), "正規表現として当たっている");
    }

    /// 全部小文字なら大小を区別しない。混じっていたら区別する。
    #[test]
    fn case_is_ignored_until_you_type_a_capital() {
        assert_eq!(Search::new("error", false).ranges("ERROR").len(), 1);
        assert!(Search::new("Error", false).ranges("error").is_empty());
    }

    /// `g/` で入れば正規表現。
    #[test]
    fn a_regex_query_matches_as_a_pattern() {
        let s = Search::new(r"\d+", true);
        assert!(s.is_regex());
        assert_eq!(s.ranges("abc 123 def"), vec![(4, 7)]);
    }

    /// **組めない正規表現で落ちない。** 素の文字列として扱い、
    /// 組めなかったことを伝える。
    #[test]
    fn a_broken_regex_is_reported_not_crashed() {
        let s = Search::new("(unclosed", true);
        assert!(s.bad_regex, "組めなかったことが伝わらない");
        assert!(!s.is_regex());
        assert_eq!(
            s.ranges("(unclosed"),
            vec![(0, 9)],
            "素の文字列に落ちていない"
        );
    }

    /// 空の一致は飛ばす。**1 文字ごとに印が付かない。**
    #[test]
    fn an_empty_match_is_skipped() {
        let s = Search::new("x*", true);
        assert!(s.ranges("abc").is_empty(), "どこにでも当たっている");
        assert_eq!(s.ranges("axxb"), vec![(1, 3)]);
    }
}
