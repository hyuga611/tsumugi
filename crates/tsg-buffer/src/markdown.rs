//! Markdown を「読む形」にする。
//!
//! **出力は ANSI 付きのテキスト**にしてある。tsumugi は既に端末の解析器を
//! 持っているので、色や太字を自前の構造で持ち回るより、端末に食わせて
//! セルにしてもらうほうが道が 1 本で済む。描画・選択・コピー・検索は
//! ふつうのペインとまったく同じ経路に乗る。
//!
//! **割り切り**: 完全な CommonMark ではない。見出し・強調・箇条書き・引用・
//! コードブロック・リンク・水平線・表の桁揃えまで。入れ子のリストは
//! 字下げとして扱い、HTML は素通しする。読むために要る分だけを実装した。

/// SGR。名前で書くと読める。
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[90m";
const ITALIC: &str = "\x1b[3m";
const UNDER: &str = "\x1b[4m";
const H1: &str = "\x1b[1;93m";
const H2: &str = "\x1b[1;96m";
const H3: &str = "\x1b[1;95m";
const CODE: &str = "\x1b[92m";
const LINK: &str = "\x1b[94m";
const MARK: &str = "\x1b[93m";

/// 読む形にした行を返す。`width` は本文を折り返す桁。
pub fn render(text: &str, width: usize) -> String {
    let width = width.clamp(20, 200);
    let mut out = String::new();
    let mut lines = text.lines().peekable();
    let mut in_code: Option<String> = None;

    while let Some(raw) = lines.next() {
        let line = raw.trim_end();

        // コードブロック。中は一切解釈しない（**そこが要点**）。
        if let Some(rest) = fence_of(line) {
            match in_code.take() {
                Some(_) => {} // 閉じた
                None => in_code = Some(rest.to_string()),
            }
            continue;
        }
        if in_code.is_some() {
            out.push_str(&format!("{DIM}│{RESET} {CODE}{line}{RESET}\r\n"));
            continue;
        }

        // 水平線
        if is_rule(line) {
            out.push_str(&format!("{DIM}{}{RESET}\r\n", "─".repeat(width)));
            continue;
        }

        // 見出し
        if let Some((level, title)) = heading_of(line) {
            let color = match level {
                1 => H1,
                2 => H2,
                _ => H3,
            };
            if level <= 2 {
                out.push_str("\r\n");
            }
            out.push_str(&format!("{color}{}{RESET}\r\n", inline_plain(title)));
            if level <= 2 {
                let n = display_len(title).min(width);
                out.push_str(&format!("{DIM}{}{RESET}\r\n", "─".repeat(n.max(4))));
            }
            continue;
        }

        // 表。`|` で区切られた行が続くあいだ、桁をそろえる。
        if is_table_row(line) {
            let mut rows = vec![line.to_string()];
            while lines.peek().is_some_and(|l| is_table_row(l.trim_end())) {
                rows.push(lines.next().unwrap().trim_end().to_string());
            }
            out.push_str(&table(&rows, width));
            continue;
        }

        // 引用
        if let Some(rest) = line.trim_start().strip_prefix("> ").or_else(|| {
            (line.trim() == ">").then_some("")
        }) {
            out.push_str(&format!("{DIM}│{RESET} {ITALIC}{}{RESET}\r\n", inline(rest)));
            continue;
        }

        // 箇条書き
        if let Some((indent, marker, rest)) = list_of(line) {
            let pad = " ".repeat(indent);
            let head = format!("{pad}{MARK}{marker}{RESET} ");
            let body_width = width.saturating_sub(indent + 2).max(10);
            for (i, chunk) in wrap(&inline(rest), body_width).into_iter().enumerate() {
                if i == 0 {
                    out.push_str(&format!("{head}{chunk}\r\n"));
                } else {
                    out.push_str(&format!("{pad}  {chunk}\r\n"));
                }
            }
            continue;
        }

        if line.trim().is_empty() {
            out.push_str("\r\n");
            continue;
        }

        for chunk in wrap(&inline(line), width) {
            out.push_str(&chunk);
            out.push_str("\r\n");
        }
    }
    out
}

/// ``` または ~~~ で始まるか。返すのは言語名。
fn fence_of(line: &str) -> Option<&str> {
    let t = line.trim_start();
    for f in ["```", "~~~"] {
        if let Some(rest) = t.strip_prefix(f) {
            return Some(rest.trim());
        }
    }
    None
}

fn is_rule(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 3
        && (t.chars().all(|c| c == '-') || t.chars().all(|c| c == '*') || t.chars().all(|c| c == '_'))
}

fn heading_of(line: &str) -> Option<(usize, &str)> {
    let t = line.trim_start();
    let level = t.chars().take_while(|c| *c == '#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = t[level..].strip_prefix(' ')?;
    Some((level, rest.trim()))
}

/// 箇条書きなら (字下げ, 出す印, 残り)。
fn list_of(line: &str) -> Option<(usize, String, &str)> {
    let indent = line.len() - line.trim_start().len();
    let t = line.trim_start();
    for m in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(m) {
            // チェックボックスは印そのものを変える。
            if let Some(r) = rest.strip_prefix("[ ] ") {
                return Some((indent, "☐".into(), r));
            }
            if let Some(r) = rest
                .strip_prefix("[x] ")
                .or_else(|| rest.strip_prefix("[X] "))
            {
                return Some((indent, "☑".into(), r));
            }
            return Some((indent, "•".into(), rest));
        }
    }
    // 番号付き
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    if digits > 0
        && let Some(rest) = t[digits..].strip_prefix(". ")
    {
        return Some((indent, format!("{}.", &t[..digits]), rest));
    }
    None
}

fn is_table_row(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.matches('|').count() >= 2
}

/// 表の桁をそろえる。**読むための表なので、区切り行は線に変える。**
fn table(rows: &[String], width: usize) -> String {
    let split = |r: &str| -> Vec<String> {
        r.trim()
            .trim_matches('|')
            .split('|')
            .map(|c| c.trim().to_string())
            .collect()
    };
    let is_sep = |r: &str| {
        r.trim()
            .trim_matches('|')
            .split('|')
            .all(|c| !c.trim().is_empty() && c.trim().chars().all(|ch| ch == '-' || ch == ':'))
    };

    let cells: Vec<Vec<String>> = rows.iter().map(|r| split(r)).collect();
    let cols = cells.iter().map(Vec::len).max().unwrap_or(0);
    let mut w = vec![0usize; cols];
    for (r, row) in cells.iter().enumerate() {
        if is_sep(&rows[r]) {
            continue;
        }
        for (i, c) in row.iter().enumerate() {
            w[i] = w[i].max(display_len(&strip_inline(c)));
        }
    }

    let mut out = String::new();
    for (r, row) in cells.iter().enumerate() {
        if is_sep(&rows[r]) {
            let line: Vec<String> = w.iter().map(|n| "─".repeat(n + 2)).collect();
            out.push_str(&format!("{DIM}{}{RESET}\r\n", line.join("┼")));
            continue;
        }
        let head = r == 0;
        let mut line = String::new();
        for (i, c) in row.iter().enumerate() {
            let pad = w[i].saturating_sub(display_len(&strip_inline(c)));
            if i > 0 {
                line.push_str(&format!("{DIM}│{RESET}"));
            }
            let body = if head {
                format!("{BOLD}{}{RESET}", inline(c))
            } else {
                inline(c)
            };
            line.push_str(&format!(" {body}{} ", " ".repeat(pad)));
        }
        out.push_str(&truncate(&line, width));
        out.push_str("\r\n");
    }
    out
}

/// 行内の記法。`**強調**` `` `コード` `` `[名前](url)`。
fn inline(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while !rest.is_empty() {
        // コード。**中は一切解釈しない。**
        if let Some(after) = rest.strip_prefix('`')
            && let Some(end) = after.find('`')
        {
            out.push_str(&format!("{CODE}{}{RESET}", &after[..end]));
            rest = &after[end + 1..];
            continue;
        }
        if let Some(after) = rest.strip_prefix("**")
            && let Some(end) = after.find("**")
        {
            out.push_str(&format!("{BOLD}{}{RESET}", &after[..end]));
            rest = &after[end + 2..];
            continue;
        }
        if let Some(after) = rest.strip_prefix('*')
            && let Some(end) = after.find('*')
            && !after.starts_with('*')
        {
            out.push_str(&format!("{ITALIC}{}{RESET}", &after[..end]));
            rest = &after[end + 1..];
            continue;
        }
        // リンク。**URL は捨てない。** Ctrl＋クリックで開けるように、
        // 名前のあとに薄く残す。
        if let Some(after) = rest.strip_prefix('[')
            && let Some(close) = after.find(']')
            && let Some(tail) = after[close + 1..].strip_prefix('(')
            && let Some(paren) = tail.find(')')
        {
            let (name, url) = (&after[..close], &tail[..paren]);
            out.push_str(&format!("{LINK}{UNDER}{name}{RESET} {DIM}{url}{RESET}"));
            rest = &tail[paren + 1..];
            continue;
        }
        let c = rest.chars().next().expect("空でないことは上で確かめた");
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// 見出し用。色は外で付けるので、記法だけ落とす。
fn inline_plain(s: &str) -> String {
    strip_inline(s)
}

/// 記法を落とした素のテキスト（桁の勘定用）。
fn strip_inline(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("**")
            && let Some(end) = after.find("**")
        {
            out.push_str(&after[..end]);
            rest = &after[end + 2..];
            continue;
        }
        if let Some(after) = rest.strip_prefix('`')
            && let Some(end) = after.find('`')
        {
            out.push_str(&after[..end]);
            rest = &after[end + 1..];
            continue;
        }
        if let Some(after) = rest.strip_prefix('[')
            && let Some(close) = after.find(']')
            && let Some(tail) = after[close + 1..].strip_prefix('(')
            && let Some(paren) = tail.find(')')
        {
            out.push_str(&after[..close]);
            rest = &tail[paren + 1..];
            continue;
        }
        let c = rest.chars().next().expect("空でないことは上で確かめた");
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// 表示幅。全角を 2 で数える。
fn display_len(s: &str) -> usize {
    s.chars().map(tsg_term::width_of).sum()
}

/// エスケープを跨がずに `width` 桁で切る。
fn truncate(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut w = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            out.push(c);
            for e in chars.by_ref() {
                out.push(e);
                if e.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        let cw = tsg_term::width_of(c);
        if w + cw > width {
            break;
        }
        w += cw;
        out.push(c);
    }
    out
}

/// 折り返す。**エスケープは桁に数えない。**
fn wrap(s: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut line = String::new();
    let mut w = 0usize;
    let mut word = String::new();
    let mut word_w = 0usize;

    let flush_word = |line: &mut String, w: &mut usize, word: &mut String, word_w: &mut usize| {
        line.push_str(word);
        *w += *word_w;
        word.clear();
        *word_w = 0;
    };

    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            word.push(c);
            for e in chars.by_ref() {
                word.push(e);
                if e.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        let cw = tsg_term::width_of(c);
        // 空白で語が切れる。CJK は 1 文字ごとに折り返してよい。
        if c == ' ' {
            flush_word(&mut line, &mut w, &mut word, &mut word_w);
            if w + 1 > width {
                out.push(std::mem::take(&mut line));
                w = 0;
            } else {
                line.push(' ');
                w += 1;
            }
            continue;
        }
        if cw == 2 {
            flush_word(&mut line, &mut w, &mut word, &mut word_w);
            if w + cw > width {
                out.push(std::mem::take(&mut line));
                w = 0;
            }
            line.push(c);
            w += cw;
            continue;
        }
        if w + word_w + cw > width && w > 0 {
            out.push(std::mem::take(&mut line));
            w = 0;
        }
        word.push(c);
        word_w += cw;
    }
    flush_word(&mut line, &mut w, &mut word, &mut word_w);
    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 素の文字だけ取り出す（見た目の確認用）。
    fn plain(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c != '\u{1b}' {
                out.push(c);
                continue;
            }
            for e in chars.by_ref() {
                if e.is_ascii_alphabetic() {
                    break;
                }
            }
        }
        out
    }

    #[test]
    fn a_heading_gets_a_rule_under_it() {
        let got = plain(&render("# 題名\n", 20));
        assert!(got.contains("題名"), "{got}");
        assert!(got.contains('─'), "見出しの下に線が無い: {got}");
    }

    #[test]
    fn the_markup_itself_does_not_show() {
        let got = plain(&render("**太字** と `コード` と *斜体*\n", 60));
        assert!(!got.contains('*'), "記法が残っている: {got}");
        assert!(!got.contains('`'), "記法が残っている: {got}");
        assert!(got.contains("太字"), "{got}");
        assert!(got.contains("コード"), "{got}");
    }

    #[test]
    fn a_link_keeps_its_url_so_you_can_still_open_it() {
        let got = plain(&render("[ここ](https://example.com) を見て\n", 60));
        assert!(got.contains("ここ"), "{got}");
        assert!(
            got.contains("https://example.com"),
            "URL を捨てた: {got}"
        );
        assert!(!got.contains(']'), "記法が残っている: {got}");
    }

    #[test]
    fn a_bullet_becomes_a_bullet() {
        let got = plain(&render("- 一つ目\n- 二つ目\n", 40));
        assert_eq!(got.matches('•').count(), 2, "{got}");
    }

    #[test]
    fn a_checkbox_shows_whether_it_is_checked() {
        let got = plain(&render("- [ ] まだ\n- [x] もう\n", 40));
        assert!(got.contains('☐') && got.contains('☑'), "{got}");
    }

    /// **コードブロックの中は解釈しない。** ここを解釈すると、
    /// マークダウンの説明を書いた文書が読めなくなる。
    #[test]
    fn a_code_block_is_left_alone() {
        let got = plain(&render("```\n# これは見出しではない\n- これも印にしない\n```\n", 40));
        assert!(got.contains("# これは見出しではない"), "{got}");
        assert!(got.contains("- これも印にしない"), "{got}");
    }

    #[test]
    fn a_table_lines_its_columns_up() {
        let src = "| 名前 | 数 |\n|---|---|\n| あ | 1 |\n| いいい | 22 |\n";
        let got = plain(&render(src, 60));
        let rows: Vec<&str> = got.lines().filter(|l| l.contains('│')).collect();
        assert_eq!(rows.len(), 3, "行が足りない: {got}");
        let widths: Vec<usize> = rows.iter().map(|r| r.find('│').unwrap_or(0)).collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "桁がそろっていない: {got}"
        );
    }

    #[test]
    fn long_lines_wrap_to_the_width() {
        let src = "aaa bbb ccc ddd eee fff ggg hhh iii jjj\n";
        for line in plain(&render(src, 12)).lines() {
            assert!(display_len(line) <= 12, "{line:?} が 12 桁を超えた");
        }
    }

    #[test]
    fn japanese_wraps_without_overflowing() {
        let src = "あいうえおかきくけこさしすせそたちつてと\n";
        for line in plain(&render(src, 10)).lines() {
            assert!(display_len(line) <= 10, "{line:?} が 10 桁を超えた");
        }
    }
}
