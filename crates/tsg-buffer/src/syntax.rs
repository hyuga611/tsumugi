//! 構文強調。**1 行で完結する**軽い字句解析だけを行う。
//!
//! tree-sitter を入れれば正確になるが、文法ごとの依存とパーサ木の維持が要る。
//! ここで欲しいのは「読むときに目が滑らないこと」であって構文木ではない。
//! そこで、行をまたぐ状態を一切持たない字句解析に絞った。
//!
//! # 行をまたぐもの
//!
//! ブロックコメント（`/* … */`）と三重引用符は次の行へ続く。**行だけ見ても
//! 決まらない**ので、行の入り口の状態を持ち回る（`State`）。
//!
//! 持ち回るのは**数バイトの状態 1 つだけ**で、行の中身は覚えない。
//! 呼ぶ側は行ごとの入り口を控えておいて、編集された行から下だけ数え直す。
//! 全部塗り直さずに済むのはそのため。
//!
//! 出力は**セル 1 つにつき 1 つ**の `Token`。呼ぶ側は色を引くだけで済み、
//! 全角の桁ずれ（スペーサセル）をここから外へ漏らさない。

use std::path::Path;

use tsg_term::Cell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    Text,
    Comment,
    Str,
    Number,
    Keyword,
    /// diff の足した行。
    Added,
    /// diff の消した行。
    Removed,
    /// diff の見出し（`diff --git` `@@` `+++` `---`）。
    DiffHead,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    None,
    Rust,
    C,
    Python,
    Js,
    Go,
    Json,
    Toml,
    Yaml,
    Markdown,
    Shell,
    /// `git diff` の出力。行の頭 1 文字で決まる。
    Diff,
}

impl Lang {
    /// 拡張子で決める。中身は見ない（開いた瞬間に決まってほしいため）。
    pub fn of_path(path: &Path) -> Self {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match ext.as_str() {
            "rs" => Lang::Rust,
            "c" | "h" | "cpp" | "cc" | "hpp" | "java" | "cs" => Lang::C,
            "py" | "pyi" => Lang::Python,
            "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" => Lang::Js,
            "go" => Lang::Go,
            "json" => Lang::Json,
            "toml" | "ini" | "cfg" => Lang::Toml,
            "yaml" | "yml" => Lang::Yaml,
            "md" | "markdown" => Lang::Markdown,
            "diff" | "patch" => Lang::Diff,
            "sh" | "bash" | "zsh" | "ps1" => Lang::Shell,
            _ => match path.file_name().and_then(|n| n.to_str()) {
                Some("Makefile" | "Dockerfile" | ".gitignore") => Lang::Shell,
                _ => Lang::None,
            },
        }
    }

    /// 保存先がまだ無いバッファ（`>` の結果）を、中身から当てる。
    ///
    /// 拡張子が無いだけで色が付かないのは惜しいが、当てにいくのは
    /// **形がはっきりしている JSON だけ**にする。外すと嘘の色が出る。
    pub fn sniff(first_line: &str) -> Self {
        let t = first_line.trim_start();
        if t.starts_with('{') || t.starts_with('[') {
            return Lang::Json;
        }
        // diff は形がはっきりしている。**当てにいくのはここまで。**
        if t.starts_with("diff --git") || t.starts_with("--- ") || t.starts_with("@@ ") {
            return Lang::Diff;
        }
        Lang::None
    }

    fn line_comment(self) -> &'static [&'static str] {
        match self {
            Lang::Rust | Lang::C | Lang::Js | Lang::Go => &["//"],
            Lang::Python | Lang::Toml | Lang::Yaml | Lang::Shell => &["#"],
            _ => &[],
        }
    }

    fn has_block_comment(self) -> bool {
        matches!(self, Lang::Rust | Lang::C | Lang::Js | Lang::Go)
    }

    /// ブロックコメントが入れ子になるか。**Rust だけ**。
    ///
    /// C で入れ子として数えると、`/* /* */` の 1 つ目で閉じないまま
    /// 下の行が全部コメント色になる。
    fn nests_block_comment(self) -> bool {
        self == Lang::Rust
    }

    /// 三重引用符を持つか（`\"\"\"` / `'''`）。
    fn has_triple_quote(self) -> bool {
        self == Lang::Python
    }

    fn quotes(self) -> &'static [char] {
        match self {
            Lang::Json => &['"'],
            Lang::Js => &['"', '\'', '`'],
            Lang::Markdown => &['`'],
            Lang::None => &[],
            _ => &['"', '\''],
        }
    }

    fn keywords(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &[
                "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else",
                "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match",
                "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct",
                "super", "trait", "true", "type", "unsafe", "use", "where", "while",
            ],
            Lang::C => &[
                "auto",
                "break",
                "case",
                "char",
                "class",
                "const",
                "continue",
                "default",
                "do",
                "double",
                "else",
                "enum",
                "extern",
                "float",
                "for",
                "goto",
                "if",
                "int",
                "long",
                "namespace",
                "new",
                "public",
                "private",
                "protected",
                "return",
                "short",
                "signed",
                "sizeof",
                "static",
                "struct",
                "switch",
                "template",
                "typedef",
                "union",
                "unsigned",
                "using",
                "virtual",
                "void",
                "volatile",
                "while",
            ],
            Lang::Python => &[
                "and", "as", "assert", "async", "await", "break", "class", "continue", "def",
                "del", "elif", "else", "except", "False", "finally", "for", "from", "global", "if",
                "import", "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise",
                "return", "True", "try", "while", "with", "yield",
            ],
            Lang::Js => &[
                "async",
                "await",
                "break",
                "case",
                "catch",
                "class",
                "const",
                "continue",
                "default",
                "delete",
                "do",
                "else",
                "export",
                "extends",
                "false",
                "finally",
                "for",
                "from",
                "function",
                "if",
                "import",
                "in",
                "instanceof",
                "let",
                "new",
                "null",
                "of",
                "return",
                "super",
                "switch",
                "this",
                "throw",
                "true",
                "try",
                "typeof",
                "var",
                "void",
                "while",
                "yield",
            ],
            Lang::Go => &[
                "break",
                "case",
                "chan",
                "const",
                "continue",
                "default",
                "defer",
                "else",
                "fallthrough",
                "for",
                "func",
                "go",
                "goto",
                "if",
                "import",
                "interface",
                "map",
                "package",
                "range",
                "return",
                "select",
                "struct",
                "switch",
                "type",
                "var",
            ],
            Lang::Shell => &[
                "case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function",
                "if", "in", "local", "return", "then", "while",
            ],
            Lang::Json => &["true", "false", "null"],
            Lang::Yaml | Lang::Toml => &["true", "false", "null"],
            _ => &[],
        }
    }
}

/// 行の入り口の状態。**行をまたぐものだけ**を覚える。
///
/// 中身（どこまで読んだか）は持たない。持つと、行を 1 つ編集するたびに
/// 下の行の状態が全部変わって、結局全部塗り直すことになる。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum State {
    #[default]
    Normal,
    /// `/* … */` の中。入れ子を数える言語があるので深さで持つ。
    Block(u8),
    /// 三重引用符の中（`\"\"\"` / `'''`）。どちらで開いたかを覚える。
    Triple(char),
}

/// セル 1 つにつき 1 つの `Token` を返す。長さは `cells` と同じ。
///
/// 行の入り口が素の状態のとき用。行をまたぐものを見るなら
/// `highlight_from` を使う。
pub fn highlight(lang: Lang, cells: &[Cell]) -> Vec<Token> {
    highlight_from(lang, cells, State::Normal).0
}

/// 入り口の状態から塗って、**出口の状態も返す**。
pub fn highlight_from(lang: Lang, cells: &[Cell], state: State) -> (Vec<Token>, State) {
    let mut out = vec![Token::Text; cells.len()];
    if lang == Lang::None || cells.is_empty() {
        // 空行はまたぐものを変えない（コメントの中の空行はコメントのまま）。
        let carried = if lang == Lang::None {
            State::Normal
        } else {
            state
        };
        return (out, carried);
    }

    // セル列 -> 文字列。スペーサ（全角の 2 セル目）は飛ばし、
    // 文字ごとに元の列を覚えておく。
    let mut text = String::new();
    let mut at: Vec<usize> = Vec::new();
    for (col, cell) in cells.iter().enumerate() {
        if cell.width == 0 {
            continue;
        }
        let c = cell.text.chars().next().unwrap_or(' ');
        at.push(col);
        text.push(c);
    }
    let chars: Vec<char> = text.chars().collect();

    // diff は**行の頭 1 文字で決まる**。1 行の中を細かく見る意味がない。
    if lang == Lang::Diff {
        let head = chars.first().copied().unwrap_or(' ');
        let is_head = ["+++", "---", "@@", "diff ", "index "]
            .iter()
            .any(|p| text.starts_with(p));
        let tok = if is_head {
            Token::DiffHead
        } else {
            match head {
                '+' => Token::Added,
                '-' => Token::Removed,
                _ => Token::Text,
            }
        };
        if tok != Token::Text {
            for t in out.iter_mut() {
                *t = tok;
            }
        }
        return (out, State::Normal);
    }

    let paint = |out: &mut Vec<Token>, from: usize, to: usize, tok: Token| {
        let upto = to.min(at.len());
        for col in at.iter().take(upto).skip(from).copied() {
            let width = cells.get(col).map_or(1, |c| usize::from(c.width.max(1)));
            for c in col..(col + width).min(out.len()) {
                out[c] = tok;
            }
        }
    };

    if lang == Lang::Markdown {
        markdown(&chars, &mut |f, t, k| paint(&mut out, f, t, k));
        return (out, State::Normal);
    }

    // 前の行から続いているものを先に閉じる。
    let mut i = 0usize;
    let mut carried = state;
    match carried {
        State::Normal => {}
        State::Block(depth) => {
            let (end, left) = close_block(&chars, 0, depth, lang.nests_block_comment());
            paint(&mut out, 0, end, Token::Comment);
            if left > 0 {
                // まだ閉じていない。**この行は全部コメント。**
                return (out, State::Block(left));
            }
            carried = State::Normal;
            i = end;
        }
        State::Triple(q) => match close_triple(&chars, 0, q) {
            Some(end) => {
                paint(&mut out, 0, end, Token::Str);
                carried = State::Normal;
                i = end;
            }
            None => {
                paint(&mut out, 0, chars.len(), Token::Str);
                return (out, State::Triple(q));
            }
        },
    }
    let _ = carried;

    while i < chars.len() {
        let rest: String = chars[i..].iter().collect();

        // 行コメント: ここから行末まで
        if lang.line_comment().iter().any(|p| rest.starts_with(p)) {
            paint(&mut out, i, chars.len(), Token::Comment);
            break;
        }

        // ブロックコメント。閉じが無ければ**次の行へ持ち越す**。
        if lang.has_block_comment() && rest.starts_with("/*") {
            let (end, left) = close_block(&chars, i + 2, 1, lang.nests_block_comment());
            paint(&mut out, i, end, Token::Comment);
            if left > 0 {
                return (out, State::Block(left));
            }
            i = end;
            continue;
        }

        // 三重引用符。閉じが無ければ**次の行へ持ち越す**。
        if lang.has_triple_quote() && (rest.starts_with("\"\"\"") || rest.starts_with("'''")) {
            let q = chars[i];
            match close_triple(&chars, i + 3, q) {
                Some(end) => {
                    paint(&mut out, i, end, Token::Str);
                    i = end;
                    continue;
                }
                None => {
                    paint(&mut out, i, chars.len(), Token::Str);
                    return (out, State::Triple(q));
                }
            }
        }

        // 文字列。エスケープを読み飛ばす。閉じが無ければ行末まで
        if lang.quotes().contains(&chars[i]) {
            let quote = chars[i];
            let mut j = i + 1;
            while j < chars.len() {
                if chars[j] == '\\' {
                    j += 2;
                    continue;
                }
                if chars[j] == quote {
                    j += 1;
                    break;
                }
                j += 1;
            }
            let end = j.min(chars.len());
            paint(&mut out, i, end, Token::Str);
            i = end;
            continue;
        }

        // 数値。識別子の途中の数字は拾わない（`x1` を光らせない）
        if chars[i].is_ascii_digit() && (i == 0 || !is_ident(chars[i - 1])) {
            let mut j = i;
            while j < chars.len()
                && (chars[j].is_ascii_alphanumeric() || chars[j] == '.' || chars[j] == '_')
            {
                j += 1;
            }
            paint(&mut out, i, j, Token::Number);
            i = j;
            continue;
        }

        // 識別子。予約語なら色を付ける
        if is_ident(chars[i]) {
            let mut j = i;
            while j < chars.len() && is_ident(chars[j]) {
                j += 1;
            }
            let word: String = chars[i..j].iter().collect();
            if lang.keywords().contains(&word.as_str()) {
                paint(&mut out, i, j, Token::Keyword);
            }
            i = j;
            continue;
        }

        i += 1;
    }
    (out, State::Normal)
}

/// `/*` の中を読み進める。返すのは (終わった位置, 残った深さ)。
///
/// 残りが 0 でなければ、その行では閉じていない = 次の行へ続く。
fn close_block(chars: &[char], from: usize, depth: u8, nests: bool) -> (usize, u8) {
    let mut depth = depth;
    let mut i = from;
    while i < chars.len() {
        if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return (i, 0);
            }
            continue;
        }
        if nests && chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
            depth = depth.saturating_add(1);
            i += 2;
            continue;
        }
        i += 1;
    }
    (chars.len(), depth)
}

/// 三重引用符の中を読み進める。
///
/// 閉じたらその直後の位置、閉じていなければ `None`。**行末で閉じた場合と
/// 閉じずに行末へ着いた場合を、位置だけでは区別できない**（どちらも
/// 行の長さになる）ので、閉じたかどうかを型で返す。
fn close_triple(chars: &[char], from: usize, quote: char) -> Option<usize> {
    let mut i = from;
    while i < chars.len() {
        if chars.get(i) == Some(&quote)
            && chars.get(i + 1) == Some(&quote)
            && chars.get(i + 2) == Some(&quote)
        {
            return Some(i + 3);
        }
        i += 1;
    }
    None
}

/// Markdown だけは語ではなく**行の形**で決まる。
fn markdown(chars: &[char], paint: &mut impl FnMut(usize, usize, Token)) {
    let head: String = chars.iter().collect();
    let t = head.trim_start();
    if t.starts_with('#') {
        paint(0, chars.len(), Token::Keyword);
        return;
    }
    if t.starts_with("```") || t.starts_with("---") || t.starts_with('>') {
        paint(0, chars.len(), Token::Comment);
        return;
    }
    // インラインコード
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '`' {
            let mut j = i + 1;
            while j < chars.len() && chars[j] != '`' {
                j += 1;
            }
            let end = (j + 1).min(chars.len());
            paint(i, end, Token::Str);
            i = end;
            continue;
        }
        i += 1;
    }
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FileBuffer;
    use tsg_term::AmbiguousWidth;

    fn toks(lang: Lang, line: &str) -> Vec<Token> {
        let f = FileBuffer::from_text(line, AmbiguousWidth::Wide);
        let cells = crate::Buffer::cells(&f, 0).unwrap_or(&[]);
        highlight(lang, cells)
    }

    /// 色は**セル**に付く。全角が混ざっても桁がずれない。
    #[test]
    fn wide_characters_do_not_shift_the_colours() {
        let line = r#"let s = "日本語"; // 説明"#;
        let t = toks(Lang::Rust, line);
        let f = FileBuffer::from_text(line, AmbiguousWidth::Wide);
        let cells = crate::Buffer::cells(&f, 0).unwrap();
        assert_eq!(t.len(), cells.len(), "セル数と色の数が合っていない");

        // `"日本語"` の開きクォートの位置
        let q = cells.iter().position(|c| c.text == "\"").unwrap();
        assert_eq!(t[q], Token::Str);
        // 全角 1 文字目とそのスペーサが同じ色
        assert_eq!(t[q + 1], Token::Str);
        assert_eq!(t[q + 2], Token::Str);
    }

    #[test]
    fn a_line_comment_swallows_the_rest_of_the_line() {
        let t = toks(Lang::Rust, "let x = 1; // ここから後ろ");
        assert_eq!(t[0], Token::Keyword, "let が予約語になっていない");
        let c = "let x = 1; ".len();
        assert!(t[c..].iter().all(|k| *k == Token::Comment));
    }

    #[test]
    fn an_unterminated_string_stops_at_the_end_of_the_line() {
        // 行をまたぐ状態は持たない。閉じが無ければその行だけで終わる。
        let t = toks(Lang::Rust, "let s = \"開いたまま");
        assert_eq!(*t.last().unwrap(), Token::Str);
        let next = toks(Lang::Rust, "let y = 1;");
        assert_eq!(next[0], Token::Keyword, "次の行まで文字列が続いている");
    }

    #[test]
    fn numbers_inside_identifiers_are_not_numbers() {
        let t = toks(Lang::Rust, "let x1 = 2;");
        let i = "let x".len();
        assert_eq!(t[i], Token::Text, "識別子の中の数字が光っている");
        let n = "let x1 = ".len();
        assert_eq!(t[n], Token::Number);
    }

    #[test]
    fn escapes_do_not_close_the_string() {
        let t = toks(Lang::Rust, r#""a\"b" x"#);
        let x = r#""a\"b" "#.len();
        assert_eq!(t[x], Token::Text, "エスケープされた引用符で閉じている");
        assert_eq!(t[0], Token::Str);
    }

    #[test]
    fn the_language_comes_from_the_extension() {
        assert_eq!(Lang::of_path(Path::new("a/b.rs")), Lang::Rust);
        assert_eq!(Lang::of_path(Path::new("x.MD")), Lang::Markdown);
        assert_eq!(Lang::of_path(Path::new("Makefile")), Lang::Shell);
        assert_eq!(Lang::of_path(Path::new("noext")), Lang::None);
    }

    #[test]
    fn json_is_guessed_only_from_an_obvious_start() {
        assert_eq!(Lang::sniff("  {\"a\": 1}"), Lang::Json);
        assert_eq!(Lang::sniff("[1,2]"), Lang::Json);
        assert_eq!(
            Lang::sniff("hello"),
            Lang::None,
            "当てずっぽうで色を付けない"
        );
    }

    #[test]
    fn markdown_headings_stand_out() {
        let t = toks(Lang::Markdown, "## 見出し");
        assert!(t.iter().all(|k| *k == Token::Keyword));
        let t = toks(Lang::Markdown, "text `code` text");
        let i = "text ".len();
        assert_eq!(t[i], Token::Str);
        assert_eq!(t[0], Token::Text);
    }

    #[test]
    fn plain_text_gets_no_colour_at_all() {
        let t = toks(Lang::None, "let x = 1; // c");
        assert!(t.iter().all(|k| *k == Token::Text));
    }
}

#[cfg(test)]
mod across_lines {
    use super::*;
    use tsg_term::AmbiguousWidth;

    fn cells(s: &str) -> Vec<Cell> {
        use crate::Buffer as _;
        crate::file::FileBuffer::from_text(s, AmbiguousWidth::Narrow)
            .cells(0)
            .map(<[Cell]>::to_vec)
            .unwrap_or_default()
    }

    fn run(lang: Lang, lines: &[&str]) -> Vec<Vec<Token>> {
        let mut state = State::Normal;
        lines
            .iter()
            .map(|l| {
                let (t, next) = highlight_from(lang, &cells(l), state);
                state = next;
                t
            })
            .collect()
    }

    /// **開いたブロックコメントは次の行へ続く。**
    ///
    /// 行だけ見て判断していたころは、2 行目から素の色に戻っていた。
    #[test]
    fn a_block_comment_carries_to_the_next_line() {
        let out = run(Lang::Rust, &["/* start", "middle", "end */ let x = 1;"]);
        assert!(out[0].iter().all(|t| *t == Token::Comment), "1 行目");
        assert!(out[1].iter().all(|t| *t == Token::Comment), "2 行目");
        // 3 行目は閉じたあとが素に戻る
        assert_eq!(out[2][0], Token::Comment, "閉じの手前がコメントでない");
        assert!(
            out[2].contains(&Token::Keyword),
            "閉じたあとが素に戻っていない"
        );
    }

    /// Rust のブロックコメントは**入れ子になる**。
    #[test]
    fn a_nested_block_comment_needs_both_closers() {
        let out = run(
            Lang::Rust,
            &["/* a /* b", "still */ inner", "done */ let x = 1;"],
        );
        assert!(
            out[1].iter().all(|t| *t == Token::Comment),
            "内側で閉じている"
        );
        assert!(out[2].contains(&Token::Keyword), "外側で閉じていない");
    }

    /// C は入れ子にしない。**1 つ目の `*/` で閉じる。**
    #[test]
    fn c_closes_at_the_first_closer() {
        let out = run(Lang::C, &["/* a /* b", "still */ int x = 1;"]);
        assert!(out[1].contains(&Token::Keyword), "1 つ目で閉じていない");
    }

    /// Python の三重引用符も次の行へ続く。
    #[test]
    fn a_triple_quote_carries_to_the_next_line() {
        let out = run(
            Lang::Python,
            &["x = \"\"\"start", "middle", "end\"\"\"", "if True:"],
        );
        assert!(
            out[1].iter().all(|t| *t == Token::Str),
            "2 行目が文字列でない"
        );
        assert!(
            out[3].contains(&Token::Keyword),
            "閉じたあとが素に戻っていない"
        );
    }

    /// コメントの中の空行は**コメントのまま**（状態を落とさない）。
    #[test]
    fn an_empty_line_inside_a_comment_stays_inside() {
        let out = run(Lang::Rust, &["/* a", "", "b */ let x = 1;"]);
        assert!(out[2].contains(&Token::Keyword), "閉じられていない");
    }

    /// 行の中で開いて閉じたら、次の行へは持ち越さない。
    #[test]
    fn a_comment_closed_on_its_line_carries_nothing() {
        let mut state = State::Normal;
        let (_, next) = highlight_from(Lang::Rust, &cells("/* a */ let x = 1;"), state);
        state = next;
        assert_eq!(state, State::Normal);
    }
}
