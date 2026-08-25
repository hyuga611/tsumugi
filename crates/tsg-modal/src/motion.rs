//! モーション。`modal-spec.md` §5.1（汎用）と §5.2（ターミナル固有）の実装。
//!
//! すべて `Buffer` の上だけで動くため、Term と File で二重実装しない。
//! ターミナル固有モーションは OSC 133 のマークだけを頼りにしていて、
//! 出力を正規表現で当てにいかない（`concept.md`）。

use tsg_buffer::{
    Buffer, Pos, clamp, first_non_blank, is_blank_line, last_col, next_col, prev_col,
};

use crate::search::Search;

/// 画面の見え方。`H` `M` `L` とページ送りに必要。
/// 描画クレートに依存しないよう、値として受け取る。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct View {
    /// 表示先頭のドキュメント絶対行
    pub top: usize,
    /// 表示行数
    pub height: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    WordFwd {
        big: bool,
    },
    WordBack {
        big: bool,
    },
    WordEnd {
        big: bool,
    },
    LineStart,
    FirstNonBlank,
    LineEnd,
    DocStart,
    DocEnd,
    ToLine(usize),
    ParaFwd,
    ParaBack,
    FindChar {
        c: char,
        till: bool,
        backward: bool,
    },
    RepeatFind {
        reverse: bool,
    },
    MatchPair,
    ScreenTop,
    ScreenMiddle,
    ScreenBottom,
    HalfPageDown,
    HalfPageUp,
    PageDown,
    PageUp,

    // ---- ターミナル固有（§5.2）。OSC 133 のマークが情報源 ----
    /// `[[` 前のプロンプトへ
    PrevPrompt,
    /// `]]` 次のプロンプトへ
    NextPrompt,
    /// `[e` 前の失敗したコマンドへ
    PrevError,
    /// `]e` 次の失敗したコマンドへ
    NextError,

    // ---- AI エージェント固有 ----
    //
    // OSC 133 は出ない（エージェントは TUI の中で描いていて、シェルの
    // プロンプトを出さない）。なので**行頭の印**を手がかりにする。
    // Claude Code の `⏺`、Codex の `•` のような、発話の頭に置かれる記号。
    /// `[a` 前の発話へ
    PrevAgentBlock,
    /// `]a` 次の発話へ
    NextAgentBlock,

    // ---- 検索 ----
    /// `n` 次の一致へ
    SearchNext,
    /// `N` 前の一致へ
    SearchPrev,
}

/// エージェントが自分の発話の頭に置く印。
///
/// **形が変わったら効かなくなる**が、効かなくても壊れない（動かないだけ）。
/// 画面から状態を当てにいくのとは違い、間違った答えを返す余地がない。
pub const AGENT_BULLETS: &[char] = &['⏺', '●', '◯', '✻', '✽', '❯', '▪', '·'];

/// オペレータが範囲を組むときの解釈。vim と同じ3種。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MotionKind {
    /// 行き先を含まない
    Exclusive,
    /// 行き先を含む
    Inclusive,
    /// 行単位
    Linewise,
}

impl Motion {
    pub fn kind(self) -> MotionKind {
        use Motion::*;
        match self {
            WordEnd { .. } | LineEnd | FindChar { .. } | RepeatFind { .. } | MatchPair => {
                MotionKind::Inclusive
            }
            Up | Down | DocStart | DocEnd | ToLine(_) | ScreenTop | ScreenMiddle | ScreenBottom
            | HalfPageDown | HalfPageUp | PageDown | PageUp | PrevPrompt | NextPrompt
            | PrevError | NextError | PrevAgentBlock | NextAgentBlock => MotionKind::Linewise,
            SearchNext | SearchPrev => MotionKind::Exclusive,
            _ => MotionKind::Exclusive,
        }
    }

    /// `f` `t` のように、直後の1文字を引数に取るか。
    pub fn needs_char(self) -> bool {
        matches!(self, Motion::FindChar { .. })
    }
}

// ---- セル単位の前後移動 ---------------------------------------------------

/// 次のセル先頭へ。行末なら次の行の先頭へ。
fn forward(buf: &dyn Buffer, pos: Pos) -> Option<Pos> {
    let end = last_col(buf, pos.line);
    if pos.col < end
        && let Some(c) = next_col(buf, pos.line, pos.col)
    {
        return Some(clamp(buf, Pos::new(pos.line, c.min(end))));
    }
    (pos.line + 1 < buf.line_count()).then(|| Pos::new(pos.line + 1, 0))
}

/// 前のセル先頭へ。行頭なら前の行の末尾へ。
///
/// exclusive なモーションの範囲を1セル縮めるために `engine` からも使う。
pub(crate) fn step_back(buf: &dyn Buffer, pos: Pos) -> Option<Pos> {
    backward(buf, pos)
}

fn backward(buf: &dyn Buffer, pos: Pos) -> Option<Pos> {
    if pos.col > 0
        && let Some(c) = prev_col(buf, pos.line, pos.col)
    {
        return Some(Pos::new(pos.line, c));
    }
    (pos.line > 0).then(|| Pos::new(pos.line - 1, last_col(buf, pos.line - 1)))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Blank,
    Word,
    Punct,
}

/// TODO(M3): vim は CJK を独立したクラスとして扱う。M1 では Word に含める。
fn class_of(buf: &dyn Buffer, pos: Pos, big: bool) -> Class {
    let text = tsg_buffer::cell_text(buf, pos.line, pos.col);
    let Some(c) = text.chars().next() else {
        return Class::Blank;
    };
    if c.is_whitespace() {
        Class::Blank
    } else if big || c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

fn word_fwd(buf: &dyn Buffer, pos: Pos, big: bool) -> Pos {
    let c0 = class_of(buf, pos, big);
    let mut p = pos;
    let mut crossed_line = false;

    if c0 != Class::Blank {
        loop {
            let Some(n) = forward(buf, p) else { return p };
            if n.line != p.line {
                p = n;
                crossed_line = true;
                break;
            }
            p = n;
            if class_of(buf, p, big) != c0 {
                break;
            }
        }
    }

    loop {
        // 空行は語の切れ目。vim と同じくそこで止まる。
        if crossed_line && is_blank_line(buf, p.line) {
            return Pos::new(p.line, 0);
        }
        if class_of(buf, p, big) != Class::Blank {
            return p;
        }
        let Some(n) = forward(buf, p) else { return p };
        if n.line != p.line {
            crossed_line = true;
        }
        p = n;
    }
}

fn word_back(buf: &dyn Buffer, pos: Pos, big: bool) -> Pos {
    let Some(mut p) = backward(buf, pos) else {
        return pos;
    };

    loop {
        if is_blank_line(buf, p.line) {
            return Pos::new(p.line, 0);
        }
        if class_of(buf, p, big) != Class::Blank {
            break;
        }
        let Some(n) = backward(buf, p) else { return p };
        p = n;
    }

    let c0 = class_of(buf, p, big);
    loop {
        let Some(n) = backward(buf, p) else { return p };
        if n.line != p.line || class_of(buf, n, big) != c0 {
            return p;
        }
        p = n;
    }
}

fn word_end(buf: &dyn Buffer, pos: Pos, big: bool) -> Pos {
    let Some(mut p) = forward(buf, pos) else {
        return pos;
    };

    while class_of(buf, p, big) == Class::Blank {
        let Some(n) = forward(buf, p) else { return p };
        p = n;
    }

    let c0 = class_of(buf, p, big);
    loop {
        let Some(n) = forward(buf, p) else { return p };
        if n.line != p.line || class_of(buf, n, big) != c0 {
            return p;
        }
        p = n;
    }
}

fn find_char(buf: &dyn Buffer, pos: Pos, target: char, till: bool, back: bool) -> Option<Pos> {
    let end = last_col(buf, pos.line);
    let mut p = pos;
    loop {
        p = if back {
            if p.col == 0 {
                return None;
            }
            Pos::new(p.line, prev_col(buf, p.line, p.col)?)
        } else {
            if p.col >= end {
                return None;
            }
            Pos::new(p.line, next_col(buf, p.line, p.col)?.min(end))
        };

        if tsg_buffer::char_at(buf, p.line, p.col) == Some(target) {
            if !till {
                return Some(p);
            }
            return if back {
                next_col(buf, p.line, p.col).map(|c| Pos::new(p.line, c.min(end)))
            } else {
                prev_col(buf, p.line, p.col).map(|c| Pos::new(p.line, c))
            };
        }
    }
}

const PAIRS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];

fn match_pair(buf: &dyn Buffer, pos: Pos) -> Pos {
    let end = last_col(buf, pos.line);

    // カーソル位置から行末に向かって最初の括弧を探す（vim と同じ）。
    let mut p = pos;
    let (open, close, forward_dir) = loop {
        let c = tsg_buffer::char_at(buf, p.line, p.col);
        if let Some(c) = c {
            if let Some(&(o, cl)) = PAIRS.iter().find(|(o, _)| *o == c) {
                break (o, cl, true);
            }
            if let Some(&(o, cl)) = PAIRS.iter().find(|(_, cl)| *cl == c) {
                break (o, cl, false);
            }
        }
        if p.col >= end {
            return pos;
        }
        match next_col(buf, p.line, p.col) {
            Some(c) => p = Pos::new(p.line, c.min(end)),
            None => return pos,
        }
    };

    let mut depth = 0i32;
    let mut q = p;
    loop {
        let c = tsg_buffer::char_at(buf, q.line, q.col);
        if c == Some(open) {
            depth += if forward_dir { 1 } else { -1 };
        } else if c == Some(close) {
            depth += if forward_dir { -1 } else { 1 };
        }
        if depth == 0 {
            return q;
        }
        let next = if forward_dir {
            forward(buf, q)
        } else {
            backward(buf, q)
        };
        match next {
            Some(n) => q = n,
            None => return pos,
        }
    }
}

fn para_fwd(buf: &dyn Buffer, pos: Pos) -> Pos {
    let last = buf.line_count().saturating_sub(1);
    let mut line = pos.line;
    // 現在の段落を抜ける
    while line < last && !is_blank_line(buf, line) {
        line += 1;
    }
    // 空行が続くなら、その先頭で止まる
    while line < last && is_blank_line(buf, line) && line == pos.line {
        line += 1;
    }
    Pos::new(line, 0)
}

fn para_back(buf: &dyn Buffer, pos: Pos) -> Pos {
    let mut line = pos.line;
    while line > 0 && !is_blank_line(buf, line) {
        line -= 1;
    }
    while line > 0 && is_blank_line(buf, line) && line == pos.line {
        line -= 1;
    }
    Pos::new(line, 0)
}

// ---- 適用 -----------------------------------------------------------------

/// 1 行を「小文字にした文字列」と「i 文字目がどの桁か」の対にする。
///
/// **文字数と桁数は同じではない。** 全角は 1 文字で 2 桁を占め、
/// 格子の上では次の 1 桁が詰め物になる。ここを混ぜると、検索で
/// 見つけた場所へ飛んだときにカーソルが 1 桁ずれる。
fn line_and_cols(buf: &dyn Buffer, line: usize) -> (String, Vec<usize>) {
    let mut text = String::new();
    let mut cols = Vec::new();
    let Some(cells) = buf.cells(line) else {
        return (text, cols);
    };
    for (col, cell) in cells.iter().enumerate() {
        if cell.width == 0 {
            continue;
        }
        for c in cell.text.chars() {
            for lc in c.to_lowercase() {
                text.push(lc);
                cols.push(col);
            }
        }
    }
    (text, cols)
}

/// 文書の中の一致を 1 つ進める / 戻る。**端まで行ったら反対の端へ回る。**
///
/// 端で止まると「無い」のか「そこまで」なのかが区別できない。回るほうが、
/// 打ち間違えたときにすぐ分かる。大小は区別しない（打つ側の負担を減らす）。
pub fn find_match(buf: &dyn Buffer, from: Pos, search: &Search, back: bool) -> Option<Pos> {
    let n = buf.line_count();
    if n == 0 || search.is_empty() {
        return None;
    }

    let hit = |line: usize, before: Option<usize>, after: Option<usize>| -> Option<Pos> {
        let (text, cols) = line_and_cols(buf, line);
        let mut found: Option<usize> = None;
        for (byte, _) in search.ranges(&text) {
            let idx = text[..byte].chars().count();
            let Some(col) = cols.get(idx).copied() else {
                continue;
            };
            let ok = match (before, after) {
                (Some(b), _) => col < b,
                (_, Some(a)) => col > a,
                _ => true,
            };
            if ok {
                found = Some(col);
                if after.is_some() {
                    break; // 前から見て最初の 1 つ
                }
            }
        }
        found.map(|col| Pos::new(line, col))
    };

    // まず同じ行の残り / 手前。次の行から順に回る。
    if back {
        if let Some(p) = hit(from.line, Some(from.col), None) {
            return Some(p);
        }
        for step in 1..=n {
            let l = (from.line + n - step) % n;
            if let Some(p) = hit(l, None, None) {
                return Some(p);
            }
        }
    } else {
        if let Some(p) = hit(from.line, None, Some(from.col)) {
            return Some(p);
        }
        for step in 1..=n {
            let l = (from.line + step) % n;
            if let Some(p) = hit(l, None, None) {
                return Some(p);
            }
        }
    }
    None
}

/// その行の一致（開始桁, 桁数）。強調に使う。
pub fn matches_in(buf: &dyn Buffer, line: usize, search: &Search) -> Vec<(usize, usize)> {
    if search.is_empty() {
        return Vec::new();
    }
    let (text, cols) = line_and_cols(buf, line);
    let mut out = Vec::new();
    for (from, to) in search.ranges(&text) {
        let idx = text[..from].chars().count();
        let end_idx = text[..to].chars().count();
        let Some(start) = cols.get(idx).copied() else {
            continue;
        };
        // 行末で終わるときの終端桁。**最後のセルが全角なら 2 桁**を
        // 数える。`+1` で済ませると、全角で終わる一致の強調が 1 桁足りない。
        let end = match cols.get(end_idx).copied() {
            Some(e) => e,
            None => {
                let last = cols.last().copied().unwrap_or(start);
                let width = buf
                    .cells(line)
                    .and_then(|c| c.get(last))
                    .map_or(1usize, |c| usize::from(c.width).max(1));
                last + width
            }
        };
        out.push((start, end.saturating_sub(start).max(1)));
    }
    out
}

/// モーションを `count` 回適用した行き先。
/// モーションが要る周りの事情。
///
/// **引数で持ち回さない。** 増えるたびに呼ぶ側を全部書き換えることになる
/// （実際に 8 個目で行き詰まった）。
#[derive(Clone, Copy, Default)]
pub struct Ctx<'a> {
    /// 画面の見え方（`H` `M` `L` とページ送りに要る）。
    pub view: View,
    /// 直前の `f` / `t`（`;` `,` が繰り返す）。
    pub last_find: Option<(char, bool, bool)>,
    /// 探しているもの（`n` `N`）。
    pub search: Option<&'a Search>,
    /// 言語サーバが言ってきた誤りの行（`[e` `]e`）。
    ///
    /// **端末には無い**（あちらは OSC 133 の失敗したコマンドが誤り）ので、
    /// ファイルを開いているときだけ入る。
    pub error_lines: &'a [usize],
}

pub fn apply(motion: Motion, from: Pos, count: usize, buf: &dyn Buffer, ctx: &Ctx<'_>) -> Pos {
    let Ctx {
        view,
        last_find,
        search,
        error_lines,
    } = *ctx;
    let count = count.max(1);
    let last_line = buf.line_count().saturating_sub(1);
    let half = (view.height / 2).max(1);
    let page = view.height.saturating_sub(2).max(1);

    // プロンプト行の並び。`[[` `]]` `[e` `]e` はここだけを見る。
    let prompts = |errors_only: bool| -> Vec<usize> {
        let from_marks: Vec<usize> = buf
            .marks()
            .blocks()
            .into_iter()
            .filter(|b| !errors_only || b.is_error())
            .map(|b| b.prompt_line)
            .collect();
        // 印が無いのは、ファイルを開いているとき。**そこでの「誤り」は
        // 言語サーバが言ってきたもの。** 同じキーで同じことをする。
        if errors_only && from_marks.is_empty() {
            let mut lines = error_lines.to_vec();
            lines.sort_unstable();
            lines.dedup();
            return lines;
        }
        from_marks
    };

    // 発話の頭。行頭（空白は飛ばす）が印で始まる行。
    let agent_lines = || -> Vec<usize> {
        (0..=last_line)
            .filter(|l| {
                buf.cells(*l)
                    .unwrap_or(&[])
                    .iter()
                    .map(|c| c.text.chars().next().unwrap_or(' '))
                    .find(|c| !c.is_whitespace())
                    .is_some_and(|c| AGENT_BULLETS.contains(&c))
            })
            .collect()
    };

    let target = match motion {
        Motion::Left => {
            let mut p = from;
            for _ in 0..count {
                match prev_col(buf, p.line, p.col) {
                    Some(c) => p = Pos::new(p.line, c),
                    None => break,
                }
            }
            p
        }
        Motion::Right => {
            let end = last_col(buf, from.line);
            let mut p = from;
            for _ in 0..count {
                match next_col(buf, p.line, p.col) {
                    Some(c) if c <= end => p = Pos::new(p.line, c),
                    _ => break,
                }
            }
            p
        }
        Motion::PrevPrompt | Motion::PrevError => {
            let lines = prompts(matches!(motion, Motion::PrevError));
            let mut p = from.line;
            for _ in 0..count {
                match lines.iter().rev().find(|l| **l < p) {
                    Some(l) => p = *l,
                    None => break,
                }
            }
            Pos::new(p, 0)
        }
        Motion::NextPrompt | Motion::NextError => {
            let lines = prompts(matches!(motion, Motion::NextError));
            let mut p = from.line;
            for _ in 0..count {
                match lines.iter().find(|l| **l > p) {
                    Some(l) => p = *l,
                    None => break,
                }
            }
            Pos::new(p, 0)
        }

        Motion::PrevAgentBlock => {
            let lines = agent_lines();
            let mut p = from.line;
            for _ in 0..count {
                match lines.iter().rev().find(|l| **l < p) {
                    Some(l) => p = *l,
                    None => break,
                }
            }
            Pos::new(p, 0)
        }
        Motion::NextAgentBlock => {
            let lines = agent_lines();
            let mut p = from.line;
            for _ in 0..count {
                match lines.iter().find(|l| **l > p) {
                    Some(l) => p = *l,
                    None => break,
                }
            }
            Pos::new(p, 0)
        }

        Motion::SearchNext | Motion::SearchPrev => {
            let Some(q) = search.filter(|q| !q.is_empty()) else {
                return from;
            };
            let back = motion == Motion::SearchPrev;
            let mut p = from;
            for _ in 0..count {
                match find_match(buf, p, q, back) {
                    Some(next) => p = next,
                    None => break,
                }
            }
            p
        }

        Motion::Up => Pos::new(from.line.saturating_sub(count), from.col),
        Motion::Down => Pos::new((from.line + count).min(last_line), from.col),

        Motion::WordFwd { big } => {
            let mut p = from;
            for _ in 0..count {
                p = word_fwd(buf, p, big);
            }
            p
        }
        Motion::WordBack { big } => {
            let mut p = from;
            for _ in 0..count {
                p = word_back(buf, p, big);
            }
            p
        }
        Motion::WordEnd { big } => {
            let mut p = from;
            for _ in 0..count {
                p = word_end(buf, p, big);
            }
            p
        }

        Motion::LineStart => Pos::new(from.line, 0),
        Motion::FirstNonBlank => Pos::new(from.line, first_non_blank(buf, from.line)),
        Motion::LineEnd => Pos::new(from.line, last_col(buf, from.line)),

        Motion::DocStart => Pos::new(0, first_non_blank(buf, 0)),
        Motion::DocEnd => Pos::new(last_line, first_non_blank(buf, last_line)),
        Motion::ToLine(n) => {
            let line = n.saturating_sub(1).min(last_line);
            Pos::new(line, first_non_blank(buf, line))
        }

        Motion::ParaFwd => {
            let mut p = from;
            for _ in 0..count {
                p = para_fwd(buf, p);
            }
            p
        }
        Motion::ParaBack => {
            let mut p = from;
            for _ in 0..count {
                p = para_back(buf, p);
            }
            p
        }

        Motion::FindChar { c, till, backward } => {
            let mut p = from;
            for _ in 0..count {
                match find_char(buf, p, c, till, backward) {
                    Some(n) => p = n,
                    None => break,
                }
            }
            p
        }
        Motion::RepeatFind { reverse } => {
            let Some((c, till, back)) = last_find else {
                return from;
            };
            let back = if reverse { !back } else { back };
            let mut p = from;
            for _ in 0..count {
                match find_char(buf, p, c, till, back) {
                    Some(n) => p = n,
                    None => break,
                }
            }
            p
        }

        Motion::MatchPair => match_pair(buf, from),

        Motion::ScreenTop => {
            let line = view.top.min(last_line);
            Pos::new(line, first_non_blank(buf, line))
        }
        Motion::ScreenMiddle => {
            let line = (view.top + view.height / 2).min(last_line);
            Pos::new(line, first_non_blank(buf, line))
        }
        Motion::ScreenBottom => {
            let line = (view.top + view.height.saturating_sub(1)).min(last_line);
            Pos::new(line, first_non_blank(buf, line))
        }

        Motion::HalfPageDown => Pos::new((from.line + half * count).min(last_line), from.col),
        Motion::HalfPageUp => Pos::new(from.line.saturating_sub(half * count), from.col),
        Motion::PageDown => Pos::new((from.line + page * count).min(last_line), from.col),
        Motion::PageUp => Pos::new(from.line.saturating_sub(page * count), from.col),
    };

    clamp(buf, target)
}

#[cfg(test)]
mod tests {
    /// 検索は**桁**で答える。全角が混じると文字数と桁数はずれる。
    #[test]
    fn a_match_is_reported_in_columns_not_characters() {
        let buf = crate::FileBuffer::from_text("あいうzzz", tsg_term::AmbiguousWidth::Wide);
        // 「あいう」で 6 桁ぶん進んだところに z がある
        assert_eq!(
            matches_in(&buf, 0, &Search::new("zzz", false)),
            vec![(6, 3)]
        );
        let p = find_match(&buf, Pos::new(0, 0), &Search::new("zzz", false), false)
            .expect("見つからない");
        assert_eq!(p, Pos::new(0, 6));
    }

    #[test]
    fn searching_wraps_around_the_document() {
        let buf = crate::FileBuffer::from_text("aaa\nbbb\nccc", tsg_term::AmbiguousWidth::Narrow);
        // 末尾から前へ探すと先頭へ回る
        let p = find_match(&buf, Pos::new(2, 0), &Search::new("aaa", false), false)
            .expect("回らなかった");
        assert_eq!(p.line, 0);
        // 先頭から後ろ向きに探すと末尾へ回る
        let p = find_match(&buf, Pos::new(0, 0), &Search::new("ccc", false), true)
            .expect("回らなかった");
        assert_eq!(p.line, 2);
    }

    #[test]
    fn searching_ignores_case() {
        let buf = crate::FileBuffer::from_text("Hello World", tsg_term::AmbiguousWidth::Narrow);
        assert_eq!(
            find_match(&buf, Pos::new(0, 0), &Search::new("world", false), false),
            Some(Pos::new(0, 6))
        );
    }

    #[test]
    fn stepping_forward_then_back_returns_to_the_same_place() {
        let buf = crate::FileBuffer::from_text("x a x a x", tsg_term::AmbiguousWidth::Narrow);
        let first =
            find_match(&buf, Pos::new(0, 0), &Search::new("a", false), false).expect("1 つ目");
        let second = find_match(&buf, first, &Search::new("a", false), false).expect("2 つ目");
        assert_ne!(first, second);
        assert_eq!(
            find_match(&buf, second, &Search::new("a", false), true),
            Some(first)
        );
    }

    use super::*;
    use tsg_buffer::TermBuffer;
    use tsg_term::{AmbiguousWidth, Terminal};

    fn term(text: &str) -> Terminal {
        let mut t = Terminal::new(40, 10, AmbiguousWidth::Wide);
        t.feed(text.as_bytes());
        t
    }

    fn go(t: &Terminal, motion: Motion, from: Pos, count: usize) -> Pos {
        let buf = TermBuffer::new(&t.state.grid, &t.state.marks);
        apply(
            motion,
            from,
            count,
            &buf,
            &Ctx {
                view: View { top: 0, height: 10 },
                ..Ctx::default()
            },
        )
    }

    #[test]
    fn hjkl_moves_one_cell() {
        let t = term("abc\r\ndef\r\n");
        assert_eq!(go(&t, Motion::Right, Pos::new(0, 0), 1), Pos::new(0, 1));
        assert_eq!(go(&t, Motion::Left, Pos::new(0, 2), 1), Pos::new(0, 1));
        assert_eq!(go(&t, Motion::Down, Pos::new(0, 1), 1), Pos::new(1, 1));
        assert_eq!(go(&t, Motion::Up, Pos::new(1, 1), 1), Pos::new(0, 1));
    }

    #[test]
    fn horizontal_motion_steps_over_wide_chars() {
        let t = term("a日b");
        // a(0) -> 日(1) -> b(3)
        assert_eq!(go(&t, Motion::Right, Pos::new(0, 0), 1), Pos::new(0, 1));
        assert_eq!(go(&t, Motion::Right, Pos::new(0, 1), 1), Pos::new(0, 3));
        assert_eq!(go(&t, Motion::Left, Pos::new(0, 3), 1), Pos::new(0, 1));
    }

    #[test]
    fn counts_multiply() {
        let t = term("abcdef");
        assert_eq!(go(&t, Motion::Right, Pos::new(0, 0), 3), Pos::new(0, 3));
    }

    #[test]
    fn word_motions() {
        let t = term("foo bar  baz");
        assert_eq!(
            go(&t, Motion::WordFwd { big: false }, Pos::new(0, 0), 1),
            Pos::new(0, 4)
        );
        assert_eq!(
            go(&t, Motion::WordFwd { big: false }, Pos::new(0, 0), 2),
            Pos::new(0, 9)
        );
        assert_eq!(
            go(&t, Motion::WordBack { big: false }, Pos::new(0, 9), 1),
            Pos::new(0, 4)
        );
        assert_eq!(
            go(&t, Motion::WordEnd { big: false }, Pos::new(0, 0), 1),
            Pos::new(0, 2)
        );
    }

    #[test]
    fn word_motion_distinguishes_punctuation() {
        let t = term("foo.bar baz");
        // w は記号で区切る: foo | . | bar | baz
        assert_eq!(
            go(&t, Motion::WordFwd { big: false }, Pos::new(0, 0), 1),
            Pos::new(0, 3)
        );
        // W は空白までを1語と見るので、記号を跨いで baz へ飛ぶ
        assert_eq!(
            go(&t, Motion::WordFwd { big: true }, Pos::new(0, 0), 1),
            Pos::new(0, 8)
        );
    }

    #[test]
    fn word_motion_stops_at_a_blank_line() {
        // 行に語が残っていなければ次の行へ。次が空行ならそこで止まる（vim と同じ）。
        let t = term("foo.bar");
        assert_eq!(
            go(&t, Motion::WordFwd { big: true }, Pos::new(0, 0), 1),
            Pos::new(1, 0)
        );
    }

    #[test]
    fn word_motion_crosses_lines() {
        let t = term("foo\r\nbar\r\n");
        assert_eq!(
            go(&t, Motion::WordFwd { big: false }, Pos::new(0, 0), 1),
            Pos::new(1, 0)
        );
    }

    #[test]
    fn line_motions() {
        let t = term("   hello   ");
        assert_eq!(go(&t, Motion::LineStart, Pos::new(0, 5), 1), Pos::new(0, 0));
        assert_eq!(
            go(&t, Motion::FirstNonBlank, Pos::new(0, 0), 1),
            Pos::new(0, 3)
        );
        assert_eq!(go(&t, Motion::LineEnd, Pos::new(0, 0), 1), Pos::new(0, 7));
    }

    #[test]
    fn document_motions() {
        let t = term("one\r\ntwo\r\nthree\r\n");
        assert_eq!(go(&t, Motion::DocStart, Pos::new(2, 0), 1), Pos::new(0, 0));
        let end = go(&t, Motion::DocEnd, Pos::new(0, 0), 1);
        assert_eq!(end.line, 9, "グリッドは 10 行なので末尾は 9 行目");
        assert_eq!(go(&t, Motion::ToLine(2), Pos::new(0, 0), 1), Pos::new(1, 0));
    }

    #[test]
    fn find_char_inline() {
        let t = term("hello world");
        assert_eq!(
            go(
                &t,
                Motion::FindChar {
                    c: 'o',
                    till: false,
                    backward: false
                },
                Pos::new(0, 0),
                1
            ),
            Pos::new(0, 4)
        );
        assert_eq!(
            go(
                &t,
                Motion::FindChar {
                    c: 'o',
                    till: false,
                    backward: false
                },
                Pos::new(0, 0),
                2
            ),
            Pos::new(0, 7)
        );
        // t は1つ手前で止まる
        assert_eq!(
            go(
                &t,
                Motion::FindChar {
                    c: 'w',
                    till: true,
                    backward: false
                },
                Pos::new(0, 0),
                1
            ),
            Pos::new(0, 5)
        );
    }

    #[test]
    fn match_pair_jumps_both_ways() {
        let t = term("a(bc(d)e)f");
        assert_eq!(go(&t, Motion::MatchPair, Pos::new(0, 1), 1), Pos::new(0, 8));
        assert_eq!(go(&t, Motion::MatchPair, Pos::new(0, 8), 1), Pos::new(0, 1));
        // カーソル以降の最初の括弧を拾う
        assert_eq!(go(&t, Motion::MatchPair, Pos::new(0, 0), 1), Pos::new(0, 8));
    }

    #[test]
    fn paragraph_motion_stops_at_blank_lines() {
        let t = term("a\r\nb\r\n\r\nc\r\n");
        assert_eq!(go(&t, Motion::ParaFwd, Pos::new(0, 0), 1), Pos::new(2, 0));
    }

    #[test]
    fn screen_motions_use_the_view() {
        let t = term("l0\r\nl1\r\nl2\r\nl3\r\nl4\r\nl5\r\n");
        let buf = TermBuffer::new(&t.state.grid, &t.state.marks);
        let view = View { top: 2, height: 4 };
        assert_eq!(
            apply(
                Motion::ScreenTop,
                Pos::new(0, 0),
                1,
                &buf,
                &Ctx {
                    view,
                    ..Ctx::default()
                }
            )
            .line,
            2
        );
        assert_eq!(
            apply(
                Motion::ScreenBottom,
                Pos::new(0, 0),
                1,
                &buf,
                &Ctx {
                    view,
                    ..Ctx::default()
                },
            )
            .line,
            5
        );
    }

    #[test]
    fn motion_kinds_match_vim() {
        assert_eq!(Motion::Left.kind(), MotionKind::Exclusive);
        assert_eq!(Motion::WordFwd { big: false }.kind(), MotionKind::Exclusive);
        assert_eq!(Motion::WordEnd { big: false }.kind(), MotionKind::Inclusive);
        assert_eq!(Motion::LineEnd.kind(), MotionKind::Inclusive);
        assert_eq!(Motion::Down.kind(), MotionKind::Linewise);
        assert_eq!(Motion::DocEnd.kind(), MotionKind::Linewise);
    }
}

#[cfg(test)]
mod error_lines {
    use super::*;
    use tsg_buffer::FileBuffer;
    use tsg_term::AmbiguousWidth;

    /// ファイルには OSC 133 の印が無い。**そこでの「次の誤り」は
    /// 言語サーバが言ってきた行。** 同じキーで同じことをする。
    #[test]
    fn on_a_file_the_error_motion_follows_the_language_server() {
        let buf = FileBuffer::from_text("a\nb\nc\nd\ne\n", AmbiguousWidth::Narrow);
        let ctx = Ctx {
            error_lines: &[3, 1],
            ..Ctx::default()
        };
        // 並んでいなくても順に飛ぶ
        let at = apply(Motion::NextError, Pos::new(0, 0), 1, &buf, &ctx);
        assert_eq!(at.line, 1);
        let at = apply(Motion::NextError, at, 1, &buf, &ctx);
        assert_eq!(at.line, 3);
        // 一番下からは動かない（端で止まる）
        let at = apply(Motion::NextError, at, 1, &buf, &ctx);
        assert_eq!(at.line, 3);

        let at = apply(Motion::PrevError, Pos::new(4, 0), 1, &buf, &ctx);
        assert_eq!(at.line, 3);
    }

    /// 誤りが無ければ動かない。**当てずっぽうで飛ばない。**
    #[test]
    fn with_no_errors_nothing_moves() {
        let buf = FileBuffer::from_text("a\nb\nc\n", AmbiguousWidth::Narrow);
        let at = apply(Motion::NextError, Pos::new(0, 0), 1, &buf, &Ctx::default());
        assert_eq!(at.line, 0);
    }
}
