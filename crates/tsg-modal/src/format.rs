//! `=` オペレータ。`modal-spec.md` §7。
//!
//! 外部コマンドを呼ばない。ターミナルの出力を整えるのに、
//! `jq` が入っているかどうかに依存させたくないため。
//! 判定できなかったものは**そのまま返す**（勝手に壊さない）。

/// 範囲のテキストを整える。
pub fn format(text: &str) -> String {
    if looks_like_json(text) {
        return json(text);
    }
    align_columns(text)
}

fn looks_like_json(text: &str) -> bool {
    let t = text.trim();
    (t.starts_with('{') && t.ends_with('}')) || (t.starts_with('[') && t.ends_with(']'))
}

/// JSON を字下げし直す。
///
/// 構造体へ読み込まず、文字を走りながら改行と字下げを入れる。
/// 数値や文字列の表現をこちらの都合で書き換えないので、
/// 「整形したら値が変わっていた」が起きない。
fn json(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut chars = text.trim().chars().peekable();

    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' if !escaped => escaped = true,
                '"' if !escaped => in_string = false,
                _ => escaped = false,
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '{' | '[' => {
                out.push(c);
                // 空の入れ物は開いたまま閉じる
                if matches!(chars.peek(), Some('}') | Some(']')) {
                    out.push(chars.next().unwrap());
                } else {
                    depth += 1;
                    newline(&mut out, depth);
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(c);
                newline(&mut out, depth);
            }
            ':' => out.push_str(": "),
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    out
}

fn newline(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

/// 空白区切りの表を桁で揃える。
///
/// `ls -l` や `docker ps` のような出力を読みやすくするためのもの。
/// 列数が行ごとに違う場合は揃えられる範囲だけ揃える。
fn align_columns(text: &str) -> String {
    let rows: Vec<Vec<&str>> = text
        .lines()
        .map(|l| l.split_whitespace().collect())
        .collect();
    // 1 列しか無い、あるいは行が 1 本しか無いなら表ではない
    if rows.len() < 2 || rows.iter().all(|r| r.len() < 2) {
        return text.to_string();
    }

    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(width(cell));
        }
    }

    let mut out = String::new();
    for (n, row) in rows.iter().enumerate() {
        if n > 0 {
            out.push('\n');
        }
        for (i, cell) in row.iter().enumerate() {
            out.push_str(cell);
            // 最後の列は詰め物を付けない（行末に空白を残さない）
            if i + 1 < row.len() {
                let pad = widths[i].saturating_sub(width(cell)) + 1;
                for _ in 0..pad {
                    out.push(' ');
                }
            }
        }
    }
    out
}

fn width(s: &str) -> usize {
    s.chars()
        .map(tsg_buffer::char_display_width)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_gets_indented() {
        let got = format(r#"{"a":1,"b":[2,3],"c":{"d":"x"}}"#);
        assert_eq!(
            got,
            "{\n  \"a\": 1,\n  \"b\": [\n    2,\n    3\n  ],\n  \"c\": {\n    \"d\": \"x\"\n  }\n}"
        );
    }

    #[test]
    fn punctuation_inside_strings_is_left_alone() {
        // 文字列の中の `,` や `{` で改行してはいけない
        let got = format(r#"{"msg":"a, b {c}","p":"C:\\dev"}"#);
        assert!(got.contains(r#""a, b {c}""#), "文字列を壊している: {got}");
        assert!(got.contains(r#""C:\\dev""#), "退避文字を壊している: {got}");
    }

    #[test]
    fn empty_containers_stay_on_one_line() {
        assert_eq!(format(r#"{"a":{},"b":[]}"#), "{\n  \"a\": {},\n  \"b\": []\n}");
    }

    #[test]
    fn a_table_gets_its_columns_aligned() {
        let got = format("NAME SIZE\nlongname 1\nx 22222");
        assert_eq!(got, "NAME     SIZE\nlongname 1\nx        22222");
    }

    #[test]
    fn wide_characters_count_as_two_columns() {
        let got = format("名前 値\nx 1");
        // 「名前」は 4 桁ぶん
        assert_eq!(got, "名前 値\nx    1");
    }

    #[test]
    fn prose_is_returned_untouched() {
        // 整形できないものを壊さない
        let src = "これは表ではありません";
        assert_eq!(format(src), src);
        let one = "just one line with words";
        assert_eq!(format(one), one);
    }

    #[test]
    fn no_trailing_spaces_are_introduced() {
        let got = format("a bb\nccc d");
        for line in got.lines() {
            assert_eq!(line, line.trim_end(), "行末に空白が残っている: {line:?}");
        }
    }
}
