//! 「ひとかたまり」の検出 — コピーボタンが指す範囲。
//!
//! AI の返事に混ざる「これをコピーして使ってください」を、人が選び直さずに
//! 1 クリックで取れるようにするための土台。
//!
//! **どこまでが塊かは、こちらで決めない。** 相手が画面に出したものだけを見る。
//! 使う手がかりは 2 つで、上から順に当てる。
//!
//! 1. **同じ背景色が続く行の連なり。** 背景は相手が SGR で明示的に送ってきた
//!    ものなので、字面を正規表現で当てにいくのとは違って推測ではない。
//!    コードブロックに下地を敷く描き方は広く使われていて、これが一番正確。
//! 2. **同じ列から始まる非空行の連なり。** 下地を敷かない相手のための保険。
//!    地の文より深く字下げされた塊だけを拾う。地の文そのものは塊にしない
//!    （どこにでもボタンが出ると、ボタンの意味が消える）。
//!
//! どちらの手がかりでも塊の**左端**が分かる。ここが分かることが要点で、
//! 「前に付いている余白が無駄な隙間になる」のは、この左端を捨てているから起きる。
//!
//! **数えるのは論理行である。** 画面の行は端末が幅で切った結果にすぎず、
//! 折り返しの続きは字下げを持たない。screen 行のまま数えると、折り返した
//! コマンドが塊の途中で切れる。

use tsg_term::Color;

use crate::Buffer;

/// 検出したひとかたまり。行は両端を含む（折り返しの続きも含む）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
    pub start: usize,
    pub end: usize,
    /// 塊の左端の列。写すときは、論理行の**先頭の行**だけここより左を捨てる。
    pub left: usize,
    /// 何を手がかりに取ったか。
    pub by: By,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum By {
    /// 同じ背景色が続いていた
    Background,
    /// 同じ列から始まる非空行が続いていた
    Indent,
}

impl Block {
    pub fn rows(&self) -> usize {
        self.end - self.start + 1
    }

    pub fn contains(&self, line: usize) -> bool {
        (self.start..=self.end).contains(&line)
    }
}

/// その行が属する塊。無ければ `None`。
pub fn block_at(buf: &dyn Buffer, line: usize) -> Option<Block> {
    if line >= buf.line_count() {
        return None;
    }
    by_background(buf, line).or_else(|| by_indent(buf, line))
}

// ---- 論理行 ---------------------------------------------------------------

/// 折り返しの続きの行か（前の行が続いている）。
fn is_continuation(buf: &dyn Buffer, line: usize) -> bool {
    line > 0 && buf.line_wrapped(line - 1)
}

/// その行が属する論理行の先頭。
fn logical_start(buf: &dyn Buffer, line: usize) -> usize {
    let mut l = line;
    while is_continuation(buf, l) {
        l -= 1;
    }
    l
}

/// その行が属する論理行の末尾。
fn logical_end(buf: &dyn Buffer, line: usize) -> usize {
    let last = buf.line_count().saturating_sub(1);
    let mut l = line;
    while l < last && buf.line_wrapped(l) {
        l += 1;
    }
    l
}

// ---- 1. 背景色 -------------------------------------------------------------

/// その行が敷いている下地の色。中身のあるセルが全部同じ非既定色ならその色。
///
/// **端の余白は数えない。** 下地は本文の幅ぶんしか塗られないことがあり、
/// 行末まで見ると「揃っていない」と判定してしまう。
fn background_of(buf: &dyn Buffer, line: usize) -> Option<Color> {
    let cells = buf.cells(line)?;
    let mut seen: Option<Color> = None;
    for cell in cells {
        if cell.width == 0 {
            continue;
        }
        let bg = cell.attrs.bg;
        if bg == Color::Default {
            // 塗られていないセルが本文の中に混ざるのは、下地とは呼べない。
            // 前後の余白だけは見逃す。
            if cell.text.trim().is_empty() {
                continue;
            }
            return None;
        }
        match seen {
            None => seen = Some(bg),
            Some(c) if c == bg => {}
            Some(_) => return None,
        }
    }
    seen
}

/// 下地が塗られ始める列。
fn background_left(buf: &dyn Buffer, line: usize, bg: Color) -> usize {
    let Some(cells) = buf.cells(line) else {
        return 0;
    };
    cells
        .iter()
        .position(|c| c.width != 0 && c.attrs.bg == bg)
        .unwrap_or(0)
}

fn by_background(buf: &dyn Buffer, line: usize) -> Option<Block> {
    let bg = background_of(buf, line)?;
    let mut start = line;
    while start > 0 && background_of(buf, start - 1) == Some(bg) {
        start -= 1;
    }
    let mut end = line;
    let last = buf.line_count().saturating_sub(1);
    while end < last && background_of(buf, end + 1) == Some(bg) {
        end += 1;
    }
    // 折り返しの続きまで含める。塊の最後の行が「続く」まま終わると、
    // コマンドの尻尾が切れたものが手に入る。
    let end = logical_end(buf, end);
    // 左端は行ごとにぶれない前提だが、ぶれていたら浅いほうへ合わせる。
    // 深いほうへ合わせると、字のある列を切り落とす。
    let left = (start..=end)
        .filter(|l| !is_continuation(buf, *l))
        .map(|l| background_left(buf, l, bg))
        .min()
        .unwrap_or(0);
    Some(Block {
        start,
        end,
        left,
        by: By::Background,
    })
}

// ---- 2. 左端（字下げ）------------------------------------------------------

/// 中身のある最初の列。空行なら `None`。
fn indent_of(buf: &dyn Buffer, line: usize) -> Option<usize> {
    let cells = buf.cells(line)?;
    cells
        .iter()
        .position(|c| c.width != 0 && !c.text.trim().is_empty())
}

/// 論理行としての字下げ。続きの行は先頭の行の字下げを名乗る。
fn logical_indent(buf: &dyn Buffer, line: usize) -> Option<usize> {
    indent_of(buf, logical_start(buf, line))
}

fn by_indent(buf: &dyn Buffer, line: usize) -> Option<Block> {
    let head = logical_start(buf, line);
    let indent = indent_of(buf, head)?;
    // 左端に貼り付いている行は地の文とみなす。ここを塊にすると、
    // 画面のどこにポインタを置いてもボタンが出る。
    if indent == 0 {
        return None;
    }

    let mut start = head;
    while start > 0 {
        let prev = logical_start(buf, start - 1);
        if indent_of(buf, prev) != Some(indent) {
            break;
        }
        start = prev;
    }

    let last = buf.line_count().saturating_sub(1);
    let mut end = logical_end(buf, head);
    while end < last {
        let next = end + 1;
        if indent_of(buf, next) != Some(indent) {
            break;
        }
        end = logical_end(buf, next);
    }

    // 上下の「近くにある本文」より深く下がっているか。
    // 同じ深さの段落が並んでいるだけなら、それは地の文であって塊ではない。
    if !deeper_than_neighbours(buf, start, end, indent) {
        return None;
    }
    Some(Block {
        start,
        end,
        left: indent,
        by: By::Indent,
    })
}

/// 塊の外側にある本文より、**上下とも**深く下がっているか。
///
/// 片側だけで判定すると、地の文が塊になる。AI の返事は地の文そのものが
/// 一段下がっているので、「上のプロンプトより深い」は地の文でも成り立つ。
/// 下にあるコードブロックより浅い、という事実まで見て初めて地の文だと分かる。
///
/// **同じ深さの隣は飛ばす。** 空行で割れた同じ深さの塊は互いに隣人だが、
/// あれは仲間であって「周り」ではない。数えると、並んだコードブロックが
/// 両方とも塊から外れる。
///
/// **比べる相手が無いときは塊にしない。** 「周りより下がっている」は
/// 周りがあって初めて言えることで、画面が丸ごと字下げされているだけの状態を
/// 塊と呼ぶと、どこにポインタを置いてもボタンが出る。
fn deeper_than_neighbours(buf: &dyn Buffer, start: usize, end: usize, indent: usize) -> bool {
    let above = (0..start)
        .rev()
        .filter_map(|l| logical_indent(buf, l))
        .find(|x| *x != indent);
    let below = (end + 1..buf.line_count())
        .filter_map(|l| logical_indent(buf, l))
        .find(|x| *x != indent);
    if above.is_none() && below.is_none() {
        return false;
    }
    above.is_none_or(|x| x < indent) && below.is_none_or(|x| x < indent)
}

// ---- 写す -----------------------------------------------------------------

/// 塊を「そのまま貼れる」文字列にする。
///
/// - 折り返しの継ぎ目には改行を入れない（1 本のコマンドを 2 行に割らない）
/// - 論理行の先頭で左端より手前の余白を捨てる（貼った先で無駄な字下げにならない）
/// - 各行の末尾の詰め物を捨てる
/// - 上下の空行を捨てる
pub fn copy_text(buf: &dyn Buffer, block: &Block) -> String {
    let mut rows: Vec<String> = Vec::new();
    let mut joining = false;
    for line in block.start..=block.end.min(buf.line_count().saturating_sub(1)) {
        let Some(cells) = buf.cells(line) else {
            continue;
        };
        // 続きの行は左端を持たない（端末が幅で切った先頭なので必ず 0 列目から）。
        // ここで左端ぶん飛ばすと、折り返した先の頭が 2 文字消える。
        let skip = if is_continuation(buf, line) {
            0
        } else {
            block.left
        };
        let mut s = String::new();
        for cell in cells.iter().skip(skip) {
            if cell.width != 0 {
                s.push_str(&cell.text);
            }
        }
        let wrapped = buf.line_wrapped(line);
        // 続きがある行の末尾は詰め物ではなく本文なので削らない。
        if !wrapped {
            s = s.trim_end().to_string();
        }
        if joining && let Some(prev) = rows.last_mut() {
            prev.push_str(&s);
        } else {
            rows.push(s);
        }
        joining = wrapped;
    }
    while rows.first().is_some_and(|r| r.trim().is_empty()) {
        rows.remove(0);
    }
    while rows.last().is_some_and(|r| r.trim().is_empty()) {
        rows.pop();
    }
    rows.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsg_term::{AmbiguousWidth, Terminal};

    use crate::TermBuffer;

    fn term(cols: usize, rows: usize, text: &str) -> Terminal {
        let mut t = Terminal::new(cols, rows, AmbiguousWidth::Wide);
        t.feed(text.as_bytes());
        t
    }

    /// 下地つきのコードブロック。前後は地の文。
    const SHADED: &str = concat!(
        "こうしてください:\r\n",
        "\x1b[48;5;236m  npm install --save-dev tsg  \x1b[0m\r\n",
        "\x1b[48;5;236m  npx tsg --version           \x1b[0m\r\n",
        "これで入ります\r\n",
    );

    #[test]
    fn a_run_of_the_same_background_is_one_block() {
        let t = term(40, 8, SHADED);
        let buf = TermBuffer::new(&t.state.grid, &t.state.marks);
        let b = block_at(&buf, 1).expect("下地の行が塊にならない");
        assert_eq!((b.start, b.end), (1, 2), "下地の続く範囲が取れていない");
        assert_eq!(b.by, By::Background);
        assert_eq!(b.left, 0, "下地は行頭から塗られている");
    }

    #[test]
    fn the_shaded_block_copies_without_the_padding() {
        let t = term(40, 8, SHADED);
        let buf = TermBuffer::new(&t.state.grid, &t.state.marks);
        let b = block_at(&buf, 1).unwrap();
        assert_eq!(
            copy_text(&buf, &b),
            "  npm install --save-dev tsg\n  npx tsg --version",
            "末尾の詰め物が残っている"
        );
    }

    #[test]
    fn prose_around_the_shaded_block_is_not_a_block() {
        let t = term(40, 8, SHADED);
        let buf = TermBuffer::new(&t.state.grid, &t.state.marks);
        assert!(block_at(&buf, 0).is_none(), "地の文にボタンが出てしまう");
        assert!(block_at(&buf, 3).is_none(), "地の文にボタンが出てしまう");
    }

    /// 下地なし。字下げの深さだけが手がかり。
    const INDENTED: &str = concat!(
        "こうしてください:\r\n",
        "\r\n",
        "    npm install --save-dev tsg\r\n",
        "    npx tsg --version\r\n",
        "\r\n",
        "これで入ります\r\n",
    );

    #[test]
    fn a_deeper_indent_run_is_one_block() {
        let t = term(40, 10, INDENTED);
        let buf = TermBuffer::new(&t.state.grid, &t.state.marks);
        let b = block_at(&buf, 2).expect("字下げされた塊が取れない");
        assert_eq!((b.start, b.end), (2, 3));
        assert_eq!(b.left, 4);
        assert_eq!(b.by, By::Indent);
    }

    #[test]
    fn the_indented_block_copies_without_the_leading_spaces() {
        let t = term(40, 10, INDENTED);
        let buf = TermBuffer::new(&t.state.grid, &t.state.marks);
        let b = block_at(&buf, 2).unwrap();
        assert_eq!(
            copy_text(&buf, &b),
            "npm install --save-dev tsg\nnpx tsg --version",
            "先頭の余白が残っている"
        );
    }

    #[test]
    fn text_at_the_left_edge_is_never_a_block() {
        let t = term(40, 10, INDENTED);
        let buf = TermBuffer::new(&t.state.grid, &t.state.marks);
        assert!(block_at(&buf, 0).is_none());
        assert!(block_at(&buf, 5).is_none());
    }

    #[test]
    fn a_paragraph_indented_the_same_as_its_neighbours_is_not_a_block() {
        // 地の文がまるごと 2 桁下がっている画面。ここを塊にすると
        // どこにポインタを置いてもボタンが出る。
        let t = term(
            40,
            10,
            "  ひとつめの段落\r\n  ふたつめの段落\r\n  みっつめ\r\n",
        );
        let buf = TermBuffer::new(&t.state.grid, &t.state.marks);
        assert!(block_at(&buf, 1).is_none());
    }

    #[test]
    fn the_shape_ai_answers_actually_have() {
        // AI の返事は地の文が浅く下がり、その中のコードブロックがもう一段深い。
        // 幅を狭くしてコマンドを折り返させる（利用者が困っているのはここ）。
        let t = term(
            44,
            12,
            concat!(
                "> ここに入れてください\r\n",
                "\r\n",
                "  これをコピーして使ってください:\r\n",
                "\r\n",
                "    npm install --save-dev @hyuga/tsumugi --registry https://example.test\r\n",
                "\r\n",
                "  入ったら確認できます\r\n",
            ),
        );
        let buf = TermBuffer::new(&t.state.grid, &t.state.marks);
        // コードの行を指す（折り返しているので画面では 2 行）。
        let b = block_at(&buf, 4).expect("コードブロックが塊にならない");
        assert_eq!(b.left, 4, "左端が本文の頭になっていない");
        assert_eq!(
            copy_text(&buf, &b),
            "npm install --save-dev @hyuga/tsumugi --registry https://example.test",
            "折り返しの改行か、先頭の余白が残っている"
        );
        // 地の文にはボタンを出さない。
        assert!(block_at(&buf, 2).is_none(), "地の文が塊になっている");
        assert!(block_at(&buf, 0).is_none(), "プロンプト行が塊になっている");
    }

    #[test]
    fn two_blocks_at_the_same_depth_are_both_blocks() {
        // 空行で割れた同じ深さの塊は互いに「周り」ではない。仲間として飛ばさないと、
        // 並んだコードブロックが両方ともボタンを失う。
        let t = term(
            44,
            12,
            concat!(
                "まず:\r\n",
                "    npm install\r\n",
                "\r\n",
                "    npm run build\r\n",
                "おわり\r\n",
            ),
        );
        let buf = TermBuffer::new(&t.state.grid, &t.state.marks);
        let a = block_at(&buf, 1).expect("前の塊が取れない");
        let b = block_at(&buf, 3).expect("後ろの塊が取れない");
        assert_eq!(copy_text(&buf, &a), "npm install");
        assert_eq!(copy_text(&buf, &b), "npm run build");
    }

    #[test]
    fn a_wrapped_command_copies_as_one_line() {
        // 幅 24 で折り返させる。画面では 2 行だが、打った人は 1 行しか打っていない。
        let t = term(24, 6, "$ run\r\n  echo hi && ls -la /very/long/path\r\n");
        let buf = TermBuffer::new(&t.state.grid, &t.state.marks);
        assert!(
            t.state.grid.document_line(1).unwrap().wrapped,
            "折り返していない前提が崩れた"
        );
        let b = block_at(&buf, 1).expect("折り返した塊が取れない");
        let text = copy_text(&buf, &b);
        assert!(
            !text.contains('\n'),
            "折り返しの継ぎ目に改行が入っている: {text:?}"
        );
        assert_eq!(text, "echo hi && ls -la /very/long/path");
    }

    #[test]
    fn the_tail_of_a_wrapped_line_is_not_cut_by_the_left_edge() {
        // 続きの行は 0 列目から始まる。左端ぶん飛ばすと頭が消える。
        let t = term(24, 6, "$ run\r\n  echo hi && ls -la /very/long/path\r\n");
        let buf = TermBuffer::new(&t.state.grid, &t.state.marks);
        let b = block_at(&buf, 1).unwrap();
        assert!(
            copy_text(&buf, &b).ends_with("/very/long/path"),
            "折り返した先の頭が削られている"
        );
    }
}
