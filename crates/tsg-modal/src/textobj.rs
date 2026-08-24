//! テキストオブジェクト。`modal-spec.md` §6.1（汎用）と §6.2（ターミナル固有）。
//!
//! ここが `mouse-parity.md` §4.3 の**ダブルクリックの実体**でもある。
//! 「`src/main.rs:42` の上でのダブルクリックは `src` ではなく全体を取る」を
//! 成立させているのは [`at_pointer`] で、キーボードの `vif` と同じ関数を通る。
//! 二重実装にすると、片方だけ賢くなって必ず食い違う。

use tsg_buffer::{
    Buffer, BufferKind, CommandBlock, Pos, Range, RangeKind, is_blank_line, last_col,
    snap_to_cell_start,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextObject {
    /// `iw` / `iW`
    Word { big: bool },
    /// `i"` `i'` `` i` ``
    Quote(char),
    /// `i(` `i[` `i{` `i<`
    Bracket(char),
    /// `ip`
    Paragraph,
    /// `is`
    Sentence,

    // ---- ターミナル固有（§6.2） ----
    /// `ic` コマンド行 / `ac` プロンプト＋コマンド＋出力
    CommandBlock,
    /// `io` 出力本体 / `ao` 末尾の空行込み
    OutputBlock,
    /// `if` パス / `af` `:行:列` サフィックス込み
    Path,
    /// `iu` URL / `au` 囲みの括弧込み
    Url,
    /// `ih` git SHA・UUID・コンテナ ID
    Hash,
    /// `in` 数値 / `an` 単位込み
    Number,
    /// `ie` 連続した出力の塊 / `ae` 失敗したコマンドの出力全体
    ErrorBlock,
}

impl TextObject {
    /// `i` / `a` の後に来る1文字から引く。
    ///
    /// `kind` を見るのは `f` のためで、`modal-spec.md` §6.2 の通り
    /// Term では「ファイルパス」、File では「関数」を指す。
    /// **同じキーが常にそのバッファで最も意味のあるものを指す**という設計。
    pub fn from_key(c: char, kind: BufferKind) -> Option<Self> {
        Some(match c {
            'w' => TextObject::Word { big: false },
            'W' => TextObject::Word { big: true },
            '"' | '\'' | '`' => TextObject::Quote(c),
            '(' | ')' | 'b' => TextObject::Bracket('('),
            '[' | ']' => TextObject::Bracket('['),
            '{' | '}' | 'B' => TextObject::Bracket('{'),
            '<' | '>' => TextObject::Bracket('<'),
            'p' => TextObject::Paragraph,
            's' => TextObject::Sentence,
            'c' => TextObject::CommandBlock,
            'o' => TextObject::OutputBlock,
            'u' => TextObject::Url,
            'h' => TextObject::Hash,
            'n' => TextObject::Number,
            'e' => TextObject::ErrorBlock,
            'f' => match kind {
                // File バッファの `if`（関数）は File バッファ実装と同時に入れる
                BufferKind::File => return None,
                _ => TextObject::Path,
            },
            _ => return None,
        })
    }
}

/// 位置 `at` を含むオブジェクトの範囲。`around` が `a`、そうでなければ `i`。
pub fn range_of(buf: &dyn Buffer, at: Pos, obj: TextObject, around: bool) -> Option<Range> {
    let at = snap_to_cell_start(buf, at);
    match obj {
        TextObject::Word { big } => word(buf, at, big, around),
        TextObject::Quote(q) => quote(buf, at, q, around),
        TextObject::Bracket(open) => bracket(buf, at, open, around),
        TextObject::Paragraph => paragraph(buf, at, around),
        TextObject::Sentence => sentence(buf, at, around),
        TextObject::CommandBlock => command_block(buf, at, around),
        TextObject::OutputBlock => output_block(buf, at, around),
        TextObject::Path => path(buf, at, around),
        TextObject::Url => url(buf, at, around),
        TextObject::Hash => hash(buf, at, around),
        TextObject::Number => number(buf, at, around),
        TextObject::ErrorBlock => error_block(buf, at, around),
    }
}

/// ダブルクリックが取るべき範囲。`mouse-parity.md` §4.3。
///
/// 「単語」を文脈依存にするのが要点。狭いものから順に当て、最初に成立したものを返す。
/// URL を先に見るのは、URL がパスとハッシュと数値をすべて内包しうるため。
pub fn at_pointer(buf: &dyn Buffer, at: Pos) -> Option<Range> {
    const TRY: [TextObject; 5] = [
        TextObject::Url,
        TextObject::Path,
        TextObject::Hash,
        TextObject::Number,
        TextObject::Word { big: false },
    ];
    // 引用符・括弧の**上**をダブルクリックしたときは対応範囲を取る（§4.3）
    if let Some(c) = char_at_pos(buf, at) {
        match c {
            '"' | '\'' | '`' => return range_of(buf, at, TextObject::Quote(c), false),
            '(' | ')' => return range_of(buf, at, TextObject::Bracket('('), false),
            '[' | ']' => return range_of(buf, at, TextObject::Bracket('['), false),
            '{' | '}' => return range_of(buf, at, TextObject::Bracket('{'), false),
            _ => {}
        }
    }
    TRY.into_iter().find_map(|o| range_of(buf, at, o, false))
}

// ---------------------------------------------------------------------------
// 行の走査
//
// セル列と文字列の間を行き来する。全角は 2 列 1 文字、結合文字は 1 列 n 文字な
// ので、`chars` の添字と列番号は一致しない。両方を持って写像で解く。
// ---------------------------------------------------------------------------

struct Scan {
    chars: Vec<char>,
    /// `chars[i]` が居る列
    cols: Vec<usize>,
}

impl Scan {
    fn of(buf: &dyn Buffer, line: usize) -> Self {
        let mut chars = Vec::new();
        let mut cols = Vec::new();
        if let Some(cells) = buf.cells(line) {
            for (col, cell) in cells.iter().enumerate() {
                if cell.width == 0 {
                    continue; // 全角の右半分
                }
                if cell.text.is_empty() {
                    chars.push(' ');
                    cols.push(col);
                    continue;
                }
                for c in cell.text.chars() {
                    chars.push(c);
                    cols.push(col);
                }
            }
        }
        Self { chars, cols }
    }

    /// 列 -> `chars` の添字。
    fn index_of(&self, col: usize) -> Option<usize> {
        self.cols.iter().position(|c| *c == col)
    }

    fn get(&self, i: usize) -> Option<char> {
        self.chars.get(i).copied()
    }

    /// `i` を含む、述語が真である連なり。
    fn run(&self, i: usize, ok: impl Fn(char) -> bool) -> Option<(usize, usize)> {
        if !ok(*self.chars.get(i)?) {
            return None;
        }
        let mut a = i;
        while a > 0 && ok(self.chars[a - 1]) {
            a -= 1;
        }
        let mut b = i;
        while b + 1 < self.chars.len() && ok(self.chars[b + 1]) {
            b += 1;
        }
        Some((a, b))
    }

    fn text(&self, a: usize, b: usize) -> String {
        self.chars[a..=b.min(self.chars.len().saturating_sub(1))]
            .iter()
            .collect()
    }

    /// `chars` の添字の対を列の範囲へ戻す。
    fn range(&self, line: usize, a: usize, b: usize) -> Range {
        Range::new(
            Pos::new(line, self.cols[a]),
            Pos::new(line, self.cols[b.min(self.cols.len() - 1)]),
            RangeKind::Char,
        )
    }
}

fn char_at_pos(buf: &dyn Buffer, at: Pos) -> Option<char> {
    let scan = Scan::of(buf, at.line);
    scan.get(scan.index_of(at.col)?)
}

// ---- 汎用 -----------------------------------------------------------------

fn class(c: char, big: bool) -> u8 {
    if c.is_whitespace() {
        0
    } else if big || c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

fn word(buf: &dyn Buffer, at: Pos, big: bool, around: bool) -> Option<Range> {
    let scan = Scan::of(buf, at.line);
    let i = scan.index_of(at.col)?;
    let k = class(scan.get(i)?, big);
    let (a, mut b) = scan.run(i, |c| class(c, big) == k)?;
    if around {
        // `aw` は後続の空白まで取る。無ければ前方の空白を取る（vim と同じ）
        let mut e = b;
        while e + 1 < scan.chars.len() && scan.chars[e + 1].is_whitespace() {
            e += 1;
        }
        if e > b {
            b = e;
        } else {
            let mut s = a;
            while s > 0 && scan.chars[s - 1].is_whitespace() {
                s -= 1;
            }
            return Some(scan.range(at.line, s, b));
        }
    }
    Some(scan.range(at.line, a, b))
}

fn quote(buf: &dyn Buffer, at: Pos, q: char, around: bool) -> Option<Range> {
    let scan = Scan::of(buf, at.line);
    let i = scan.index_of(at.col)?;

    // 行頭から数えて、カーソルを挟む組を探す
    let marks: Vec<usize> = scan
        .chars
        .iter()
        .enumerate()
        .filter(|(_, c)| **c == q)
        .map(|(i, _)| i)
        .collect();
    let pair = marks
        .chunks(2)
        .filter(|p| p.len() == 2)
        .find(|p| i <= p[1] && i >= p[0].saturating_sub(0))?;
    let (open, close) = (pair[0], pair[1]);

    if around {
        Some(scan.range(at.line, open, close))
    } else if close > open + 1 {
        Some(scan.range(at.line, open + 1, close - 1))
    } else {
        None
    }
}

fn closing(open: char) -> char {
    match open {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        _ => '>',
    }
}

fn bracket(buf: &dyn Buffer, at: Pos, open: char, around: bool) -> Option<Range> {
    let close = closing(open);
    let scan = Scan::of(buf, at.line);
    let i = scan.index_of(at.col)?;

    // カーソルより左で、まだ閉じていない開き括弧
    let mut depth = 0i32;
    let mut start = None;
    for k in (0..=i).rev() {
        let c = scan.chars[k];
        if c == close && k != i {
            depth += 1;
        } else if c == open {
            if depth == 0 {
                start = Some(k);
                break;
            }
            depth -= 1;
        }
    }
    let start = start?;

    depth = 0;
    let mut end = None;
    for (k, c) in scan.chars.iter().enumerate().skip(start + 1) {
        if *c == open {
            depth += 1;
        } else if *c == close {
            if depth == 0 {
                end = Some(k);
                break;
            }
            depth -= 1;
        }
    }
    let end = end?;

    if around {
        Some(scan.range(at.line, start, end))
    } else if end > start + 1 {
        Some(scan.range(at.line, start + 1, end - 1))
    } else {
        None
    }
}

/// 空行で挟まれた連続行。`ap` は後続の空行込み。
fn paragraph(buf: &dyn Buffer, at: Pos, around: bool) -> Option<Range> {
    let blank = is_blank_line(buf, at.line);
    let mut a = at.line;
    while a > 0 && is_blank_line(buf, a - 1) == blank {
        a -= 1;
    }
    let mut b = at.line;
    while b + 1 < buf.line_count() && is_blank_line(buf, b + 1) == blank {
        b += 1;
    }
    if around {
        while b + 1 < buf.line_count() && is_blank_line(buf, b + 1) != blank {
            b += 1;
        }
    }
    Some(linewise(a, b))
}

fn sentence(buf: &dyn Buffer, at: Pos, around: bool) -> Option<Range> {
    let scan = Scan::of(buf, at.line);
    let i = scan.index_of(at.col)?;
    let is_end = |c: char| matches!(c, '.' | '!' | '?' | '。' | '！' | '？');

    let mut a = i;
    while a > 0 && !is_end(scan.chars[a - 1]) {
        a -= 1;
    }
    let mut b = i;
    while b + 1 < scan.chars.len() && !is_end(scan.chars[b]) {
        b += 1;
    }
    if !around {
        while a < b && scan.chars[a].is_whitespace() {
            a += 1;
        }
    } else {
        while b + 1 < scan.chars.len() && scan.chars[b + 1].is_whitespace() {
            b += 1;
        }
    }
    Some(scan.range(at.line, a, b))
}

// ---- ターミナル固有 --------------------------------------------------------

fn linewise(a: usize, b: usize) -> Range {
    Range::new(Pos::new(a, 0), Pos::new(b, usize::MAX), RangeKind::Line)
}

/// `at` を含むコマンドブロックと、その最終行。
///
/// 終端は OSC 133 の `D` が来た行だが、**`D` はたいてい次の行頭で来る**
/// （出力を書き終えてから吐くため）。次のプロンプト行の手前で頭打ちにしないと、
/// `ac` が次のプロンプトまで飲む。実機のガタークリックで踏んだ。
fn block_at(buf: &dyn Buffer, line: usize) -> Option<(CommandBlock, usize)> {
    let blocks = buf.marks().blocks();
    let i = blocks.iter().rposition(|b| b.prompt_line <= line)?;
    let b = blocks[i].clone();

    let mut end = b
        .output_end
        .or(b.output_start)
        .or(b.command_line)
        .unwrap_or(b.prompt_line)
        .max(b.prompt_line)
        .min(buf.line_count().saturating_sub(1));
    if let Some(next) = blocks.get(i + 1) {
        end = end.min(next.prompt_line.saturating_sub(1));
    }
    (line <= end).then_some((b, end))
}

/// `ic` = 打ったコマンド行だけ / `ac` = プロンプトから出力の終わりまで。
fn command_block(buf: &dyn Buffer, at: Pos, around: bool) -> Option<Range> {
    let (b, end) = block_at(buf, at.line)?;
    if around {
        return Some(linewise(b.prompt_line, end));
    }
    let line = b.command_line?;
    Some(Range::new(
        Pos::new(line, b.command_col),
        Pos::new(line, last_col(buf, line)),
        RangeKind::Char,
    ))
}

/// `io` = 出力本体 / `ao` = 末尾の空行込み。
fn output_block(buf: &dyn Buffer, at: Pos, around: bool) -> Option<Range> {
    let (b, mut end) = block_at(buf, at.line)?;
    let start = b.output_start?;
    if !around {
        while end > start && is_blank_line(buf, end) {
            end -= 1;
        }
    }
    (end >= start).then(|| linewise(start, end))
}

/// `ie` = カーソルを含む連続した非空行の塊（＝スタックトレース本体）
/// `ae` = 失敗したコマンドの出力全体。成功していれば `ie` と同じ。
fn error_block(buf: &dyn Buffer, at: Pos, around: bool) -> Option<Range> {
    if around
        && let Some((b, end)) = block_at(buf, at.line)
        && b.is_error()
        && let Some(start) = b.output_start
    {
        return Some(linewise(start, end));
    }
    if is_blank_line(buf, at.line) {
        return None;
    }
    let mut a = at.line;
    while a > 0 && !is_blank_line(buf, a - 1) {
        a -= 1;
    }
    let mut b = at.line;
    while b + 1 < buf.line_count() && !is_blank_line(buf, b + 1) {
        b += 1;
    }
    Some(linewise(a, b))
}

// ---- パス・URL・ハッシュ・数値 ---------------------------------------------

fn is_path_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '/' | '\\' | '.' | '_' | '-' | '~' | '+' | '@' | '#' | '%')
}

/// `foo` を拾わないための最低条件。区切りを含むか、拡張子らしき尻尾を持つこと。
fn looks_like_path(s: &str) -> bool {
    if s.contains('/') || s.contains('\\') {
        return true;
    }
    match s.rsplit_once('.') {
        Some((stem, ext)) => {
            !stem.is_empty()
                && (1..=8).contains(&ext.chars().count())
                && ext.chars().all(char::is_alphanumeric)
                && ext.chars().any(char::is_alphabetic)
        }
        None => false,
    }
}

/// `if` = パスのみ / `af` = `:42:8` のような位置サフィックス込み。
fn path(buf: &dyn Buffer, at: Pos, around: bool) -> Option<Range> {
    let scan = Scan::of(buf, at.line);
    let i = scan.index_of(at.col)?;

    // `:` を境界にしないと `src/main.rs:42` の `42` まで `if` が飲む。
    // 一方 `C:\Users` の `:` はパスの一部なので、後から取り込む。
    let (mut a, b) = scan.run(i, is_path_char)?;
    if a >= 2
        && scan.chars[a - 1] == ':'
        && scan.chars[a - 2].is_ascii_alphabetic()
        && (a < 3 || !scan.chars[a - 3].is_alphanumeric())
    {
        a -= 2;
    }

    let text = scan.text(a, b);
    if !looks_like_path(&text) {
        return None;
    }
    if !around {
        return Some(scan.range(at.line, a, b));
    }

    // `:{数}` を繰り返し取り込む
    let mut e = b;
    while scan.get(e + 1) == Some(':') && scan.get(e + 2).is_some_and(|c| c.is_ascii_digit()) {
        e += 2;
        while scan.get(e + 1).is_some_and(|c| c.is_ascii_digit()) {
            e += 1;
        }
    }
    Some(scan.range(at.line, a, e))
}

fn is_url_char(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(
            c,
            '-' | '.' | '_' | '~' | ':' | '/' | '?' | '#' | '[' | ']' | '@'
                | '!' | '$' | '&' | '\'' | '(' | ')' | '*' | '+' | ',' | ';' | '=' | '%'
        )
}

fn url(buf: &dyn Buffer, at: Pos, around: bool) -> Option<Range> {
    let scan = Scan::of(buf, at.line);
    let i = scan.index_of(at.col)?;
    let (mut a, b) = scan.run(i, is_url_char)?;
    // `(https://...)` のように囲まれていると、括弧も URL 文字なので run に入る。
    // スキームは英数で始まるので、頭の非英数はここで落とす。
    while a < b && !scan.chars[a].is_alphanumeric() {
        a += 1;
    }
    let text = scan.text(a, b);
    if !text.contains("://") && !text.starts_with("www.") {
        return None;
    }

    // 文末の句読点・閉じ括弧は URL ではない
    let mut e = b;
    while e > a
        && matches!(
            scan.chars[e],
            '.' | ',' | ';' | ':' | ')' | ']' | '!' | '?' | '\''
        )
    {
        e -= 1;
    }

    if around {
        // 囲みの括弧・引用符まで含める
        let mut s = a;
        let mut t = e;
        let wraps = [('(', ')'), ('[', ']'), ('<', '>'), ('"', '"'), ('\'', '\'')];
        if let Some((_, r)) = wraps
            .iter()
            .find(|(l, _)| s > 0 && scan.chars[s - 1] == *l)
            && scan.get(t + 1) == Some(*r)
        {
            s -= 1;
            t += 1;
        }
        return Some(scan.range(at.line, s, t));
    }
    Some(scan.range(at.line, a, e))
}

/// git SHA（7桁以上の16進）・コンテナ ID・UUID。
fn looks_like_hash(s: &str) -> bool {
    let n = s.chars().count();
    if s.contains('-') {
        // UUID: 8-4-4-4-12
        let parts: Vec<&str> = s.split('-').collect();
        return parts.len() == 5
            && [8, 4, 4, 4, 12] == parts.iter().map(|p| p.len()).collect::<Vec<_>>()[..]
            && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_hexdigit()));
    }
    n >= 7
        && s.chars().all(|c| c.is_ascii_hexdigit())
        && s.chars().any(|c| c.is_ascii_digit())
        && s.chars().any(|c| c.is_ascii_alphabetic())
}

fn hash(buf: &dyn Buffer, at: Pos, _around: bool) -> Option<Range> {
    let scan = Scan::of(buf, at.line);
    let i = scan.index_of(at.col)?;
    let (a, b) = scan.run(i, |c| c.is_ascii_alphanumeric() || c == '-')?;
    looks_like_hash(&scan.text(a, b)).then(|| scan.range(at.line, a, b))
}

/// `in` = 数値 / `an` = 単位込み（`1.2GB` `300ms` `45%`）。
fn number(buf: &dyn Buffer, at: Pos, around: bool) -> Option<Range> {
    let scan = Scan::of(buf, at.line);
    let i = scan.index_of(at.col)?;
    let digits = |c: char| c.is_ascii_digit();

    // カーソルが単位側に居ても数値を取れるよう、いったん英数の連なりを見る
    let (run_a, run_b) = scan.run(i, |c| c.is_ascii_alphanumeric() || c == '.' || c == '%')?;
    let a = (run_a..=run_b).find(|k| digits(scan.chars[*k]))?;
    if a > run_a && scan.chars[a - 1] != '.' {
        // `abc123` のような識別子は数値ではない
        return None;
    }

    let mut b = a;
    while scan
        .get(b + 1)
        .is_some_and(|c| digits(c) || (c == '.' && scan.get(b + 2).is_some_and(digits)))
    {
        b += 1;
    }
    let a = if a > 0 && scan.chars[a - 1] == '-' { a - 1 } else { a };

    if around {
        let mut e = b;
        while scan
            .get(e + 1)
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '%')
        {
            e += 1;
        }
        return Some(scan.range(at.line, a, e));
    }
    Some(scan.range(at.line, a, b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsg_term::{AmbiguousWidth, Terminal};

    struct Fixture {
        term: Terminal,
    }

    impl Fixture {
        fn new(feed: &str) -> Self {
            let mut term = Terminal::new(80, 24, AmbiguousWidth::Wide);
            term.feed(feed.replace('\n', "\r\n").as_bytes());
            Self { term }
        }

        fn buf(&self) -> tsg_buffer::TermBuffer<'_> {
            tsg_buffer::TermBuffer::new(&self.term.state.grid, &self.term.state.marks)
        }
    }

    /// 範囲のテキストを取り出す。
    fn got(f: &Fixture, r: Range) -> String {
        let buf = f.buf();
        tsg_buffer::extract(&buf, &r)
    }

    fn find_col(f: &Fixture, line: usize, needle: &str) -> usize {
        let buf = f.buf();
        let text = tsg_buffer::line_text(&buf, line);
        let byte = text.find(needle).expect("目印が行に無い");
        // 列は表示幅で数える
        text[..byte]
            .chars()
            .map(|c| tsg_term::char_width(c, AmbiguousWidth::Wide))
            .sum()
    }

    fn obj(f: &Fixture, line: usize, needle: &str, o: TextObject, around: bool) -> String {
        let col = find_col(f, line, needle);
        let r = range_of(&f.buf(), Pos::new(line, col), o, around)
            .unwrap_or_else(|| panic!("{o:?} が取れない（{needle:?} の上）"));
        got(f, r)
    }

    // ---- 汎用 ----

    #[test]
    fn word_stops_at_punctuation() {
        let f = Fixture::new("foo.bar baz");
        assert_eq!(obj(&f, 0, "foo", TextObject::Word { big: false }, false), "foo");
        assert_eq!(
            obj(&f, 0, "foo", TextObject::Word { big: true }, false),
            "foo.bar"
        );
    }

    #[test]
    fn aw_takes_the_trailing_space() {
        let f = Fixture::new("foo bar");
        assert_eq!(obj(&f, 0, "foo", TextObject::Word { big: false }, true), "foo ");
    }

    #[test]
    fn quotes_and_brackets() {
        let f = Fixture::new("say \"hello world\" now");
        assert_eq!(obj(&f, 0, "hello", TextObject::Quote('"'), false), "hello world");
        assert_eq!(
            obj(&f, 0, "hello", TextObject::Quote('"'), true),
            "\"hello world\""
        );

        let g = Fixture::new("call(a, b(c), d)");
        assert_eq!(obj(&g, 0, "a,", TextObject::Bracket('('), false), "a, b(c), d");
        assert_eq!(obj(&g, 0, "c)", TextObject::Bracket('('), false), "c");
    }

    #[test]
    fn a_wide_line_maps_columns_back_correctly() {
        // 全角が混ざると chars の添字と列がずれる。写像が壊れていれば範囲が飛ぶ。
        let f = Fixture::new("日本語 src/main.rs です");
        assert_eq!(obj(&f, 0, "src", TextObject::Path, false), "src/main.rs");
    }

    // ---- パス ----

    #[test]
    fn if_stops_before_the_line_number_and_af_takes_it() {
        let f = Fixture::new("error at src/main.rs:42:8 in build");
        assert_eq!(obj(&f, 0, "src", TextObject::Path, false), "src/main.rs");
        assert_eq!(obj(&f, 0, "src", TextObject::Path, true), "src/main.rs:42:8");
    }

    #[test]
    fn a_windows_drive_letter_stays_with_the_path() {
        let f = Fixture::new("open C:\\Users\\me\\a.txt now");
        assert_eq!(
            obj(&f, 0, "Users", TextObject::Path, false),
            "C:\\Users\\me\\a.txt"
        );
    }

    #[test]
    fn a_bare_word_is_not_a_path() {
        let f = Fixture::new("just words here");
        let col = find_col(&f, 0, "words");
        assert!(range_of(&f.buf(), Pos::new(0, col), TextObject::Path, false).is_none());
    }

    // ---- URL・ハッシュ・数値 ----

    #[test]
    fn url_drops_the_sentence_punctuation() {
        let f = Fixture::new("see https://example.com/a?b=1, then stop");
        assert_eq!(
            obj(&f, 0, "https", TextObject::Url, false),
            "https://example.com/a?b=1"
        );
    }

    #[test]
    fn au_takes_the_wrapping_brackets() {
        let f = Fixture::new("ref (https://example.com) end");
        assert_eq!(
            obj(&f, 0, "https", TextObject::Url, true),
            "(https://example.com)"
        );
    }

    #[test]
    fn hash_needs_to_look_like_one() {
        let f = Fixture::new("commit 3f9a2c1b8e4 by 550e8400-e29b-41d4-a716-446655440000 and deadbeef");
        assert_eq!(obj(&f, 0, "3f9a", TextObject::Hash, false), "3f9a2c1b8e4");
        assert_eq!(
            obj(&f, 0, "550e", TextObject::Hash, false),
            "550e8400-e29b-41d4-a716-446655440000"
        );

        let g = Fixture::new("the quick brown");
        let col = find_col(&g, 0, "quick");
        assert!(
            range_of(&g.buf(), Pos::new(0, col), TextObject::Hash, false).is_none(),
            "英字だけの語をハッシュ扱いしている"
        );
    }

    #[test]
    fn number_and_its_unit() {
        let f = Fixture::new("used 1.25GB in 300ms");
        assert_eq!(obj(&f, 0, "1.25", TextObject::Number, false), "1.25");
        assert_eq!(obj(&f, 0, "1.25", TextObject::Number, true), "1.25GB");
        assert_eq!(obj(&f, 0, "300", TextObject::Number, true), "300ms");
    }

    // ---- ターミナル固有 ----

    fn session() -> Fixture {
        // プロンプト -> コマンド -> 出力 -> 終了(1)
        let mut term = Terminal::new(80, 24, AmbiguousWidth::Wide);
        term.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07cargo build --release\r\n");
        term.feed(b"\x1b]133;C\x07");
        term.feed(b"   Compiling foo\r\nerror: boom\r\n\r\n");
        term.feed(b"\x1b]133;D;1\x07");
        Fixture { term }
    }

    #[test]
    fn ac_stops_before_the_next_prompt() {
        // OSC 133 の `D` は出力を書き終えた**次の行**で来る。そのまま終端に使うと
        // 次のプロンプト行まで飲む。実機でガターをクリックして踏んだ回帰。
        let mut term = Terminal::new(60, 20, AmbiguousWidth::Wide);
        term.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07first\r\n\x1b]133;C\x07out-1\r\n\x1b]133;D;0\x07");
        term.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07second\r\n\x1b]133;C\x07out-2\r\n\x1b]133;D;0\x07");
        let f = Fixture { term };

        let r = range_of(&f.buf(), Pos::new(1, 0), TextObject::CommandBlock, true).unwrap();
        let text = got(&f, r);
        assert!(text.contains("$ first"), "自分のプロンプトが無い: {text:?}");
        assert!(text.contains("out-1"), "自分の出力が無い: {text:?}");
        assert!(
            !text.contains("second"),
            "次のプロンプトまで飲んでいる: {text:?}"
        );
    }

    #[test]
    fn ic_takes_the_command_line_only() {
        let f = session();
        let r = range_of(&f.buf(), Pos::new(0, 3), TextObject::CommandBlock, false).unwrap();
        assert_eq!(got(&f, r), "cargo build --release");
    }

    #[test]
    fn ac_takes_the_prompt_command_and_output() {
        let f = session();
        let r = range_of(&f.buf(), Pos::new(1, 0), TextObject::CommandBlock, true).unwrap();
        let text = got(&f, r);
        assert!(text.starts_with("$ cargo build"), "プロンプト行から始まらない: {text:?}");
        assert!(text.contains("error: boom"), "出力が入っていない: {text:?}");
    }

    #[test]
    fn io_takes_the_output_without_the_trailing_blank() {
        let f = session();
        let r = range_of(&f.buf(), Pos::new(1, 0), TextObject::OutputBlock, false).unwrap();
        let text = got(&f, r);
        assert!(text.starts_with("   Compiling foo"));
        assert!(!text.ends_with("\n\n"), "末尾の空行を含んでいる: {text:?}");
        assert!(!text.contains("cargo build"), "コマンド行まで飲んでいる");
    }

    #[test]
    fn ae_takes_the_whole_output_of_a_failed_command() {
        let f = session();
        let r = range_of(&f.buf(), Pos::new(2, 0), TextObject::ErrorBlock, true).unwrap();
        assert!(got(&f, r).contains("error: boom"));
    }

    // ---- ダブルクリックの文脈依存 ----

    #[test]
    fn a_double_click_on_a_path_takes_the_whole_path() {
        // 既存のターミナルは `src` だけを取る。ここが決定的な差。
        let f = Fixture::new("error at src/main.rs:42 in build");
        let col = find_col(&f, 0, "main");
        let r = at_pointer(&f.buf(), Pos::new(0, col)).unwrap();
        assert_eq!(got(&f, r), "src/main.rs");
    }

    #[test]
    fn a_double_click_on_a_plain_word_still_takes_the_word() {
        let f = Fixture::new("just words here");
        let col = find_col(&f, 0, "words");
        let r = at_pointer(&f.buf(), Pos::new(0, col)).unwrap();
        assert_eq!(got(&f, r), "words");
    }

    #[test]
    fn a_double_click_on_a_bracket_takes_its_contents() {
        let f = Fixture::new("call(a, b)");
        let col = find_col(&f, 0, "(");
        let r = at_pointer(&f.buf(), Pos::new(0, col)).unwrap();
        assert_eq!(got(&f, r), "a, b");
    }

    #[test]
    fn a_double_click_on_a_url_takes_the_url() {
        let f = Fixture::new("open https://example.com/x now");
        let col = find_col(&f, 0, "example");
        let r = at_pointer(&f.buf(), Pos::new(0, col)).unwrap();
        assert_eq!(got(&f, r), "https://example.com/x");
    }
}
