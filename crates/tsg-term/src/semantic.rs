//! OSC 133 セマンティックプロンプトマーク。
//!
//! `modal-spec.md` の `[[` `]]` `[e` `]e` `ic` `ac` `io` `ao` と、
//! `mouse-parity.md` の左ガターの**唯一の情報源**。ここが取れないと製品の売りが消える。
//!
//! 対応する系列:
//! - `OSC 133 ; A` プロンプト開始
//! - `OSC 133 ; B` コマンド入力開始（プロンプト末尾）
//! - `OSC 133 ; C` コマンド実行開始（＝出力開始）
//! - `OSC 133 ; D [; 終了コード]` コマンド終了
//!
//! VSCode / PSReadLine 系が使う `OSC 633` も同じ意味づけで受ける。

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkKind {
    PromptStart,
    CommandStart,
    OutputStart,
    CommandEnd { exit_code: Option<i32> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    pub kind: MarkKind,
    /// ドキュメント絶対行番号
    pub line: usize,
    pub col: usize,
}

/// プロンプトから終了までを1単位にまとめたもの。
/// `ac`（コマンドブロック全体）/ `ic`（コマンド行）/ `io`（出力）の実体。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandBlock {
    pub prompt_line: usize,
    pub command_line: Option<usize>,
    /// コマンド文字列が始まる列（プロンプト記号の直後）。`ic` の左端。
    pub command_col: usize,
    pub output_start: Option<usize>,
    pub output_end: Option<usize>,
    pub exit_code: Option<i32>,
}

impl CommandBlock {
    /// `]e` が飛ぶ先か（非ゼロ終了）。
    pub fn is_error(&self) -> bool {
        self.exit_code.is_some_and(|c| c != 0)
    }

    /// まだ走っているか。
    pub fn is_running(&self) -> bool {
        self.output_start.is_some() && self.output_end.is_none()
    }
}

#[derive(Default, Debug, Clone)]
pub struct SemanticMarks {
    marks: Vec<Mark>,
}

impl SemanticMarks {
    pub fn push(&mut self, kind: MarkKind, line: usize, col: usize) {
        self.marks.push(Mark { kind, line, col });
    }

    pub fn all(&self) -> &[Mark] {
        &self.marks
    }

    pub fn is_empty(&self) -> bool {
        self.marks.is_empty()
    }

    /// 先頭から `n` 行が消えたので、印を同じだけ手前へ寄せる。
    ///
    /// 印は**ドキュメント絶対行番号**で持っている。行が消えたのに寄せないと、
    /// ガターの印と `[[` `]]` が実在しない行、あるいは無関係な行を指す。
    /// 消えた側へ落ちた印は捨てる（指す先がもう無い）。
    pub fn shift_up(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        self.marks.retain(|m| m.line >= n);
        for m in &mut self.marks {
            m.line -= n;
        }
    }

    /// `from..=to` の行が消えたので、その範囲の印を捨てて後ろを詰める。
    pub fn remove_lines(&mut self, from: usize, to: usize) {
        if from > to {
            return;
        }
        let n = to - from + 1;
        self.marks.retain(|m| m.line < from || m.line > to);
        for m in &mut self.marks {
            if m.line > to {
                m.line -= n;
            }
        }
    }

    pub fn clear(&mut self) {
        self.marks.clear();
    }

    /// マーク列をコマンドブロックへ畳む。
    pub fn blocks(&self) -> Vec<CommandBlock> {
        let mut out: Vec<CommandBlock> = Vec::new();
        for m in &self.marks {
            match m.kind {
                MarkKind::PromptStart => out.push(CommandBlock {
                    prompt_line: m.line,
                    command_line: None,
                    command_col: 0,
                    output_start: None,
                    output_end: None,
                    exit_code: None,
                }),
                MarkKind::CommandStart => {
                    if let Some(b) = out.last_mut() {
                        b.command_line = Some(m.line);
                        b.command_col = m.col;
                    }
                }
                MarkKind::OutputStart => {
                    if let Some(b) = out.last_mut() {
                        b.output_start = Some(m.line);
                    }
                }
                MarkKind::CommandEnd { exit_code } => {
                    if let Some(b) = out.last_mut() {
                        b.output_end = Some(m.line);
                        b.exit_code = exit_code;
                    }
                }
            }
        }
        out
    }
}

/// `133;A` / `633;D;1` などのパラメータ列を解釈する。
///
/// `params[0]` は既に `133` / `633` であることが確認済みの前提で、
/// `params[1..]` を渡す。
pub fn parse_mark(rest: &[&[u8]]) -> Option<MarkKind> {
    let tag = rest.first()?;
    match tag.first()? {
        b'A' => Some(MarkKind::PromptStart),
        b'B' => Some(MarkKind::CommandStart),
        b'C' => Some(MarkKind::OutputStart),
        b'D' => {
            // `D` 単独 / `D;0` / `D;1` / `D;err=1` のいずれもありうる
            let exit_code = rest.get(1).and_then(|p| {
                let s = std::str::from_utf8(p).ok()?;
                let s = s.rsplit('=').next().unwrap_or(s);
                s.trim().parse::<i32>().ok()
            });
            Some(MarkKind::CommandEnd { exit_code })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shifting_moves_the_marks_and_drops_the_ones_that_fell_off() {
        let mut m = SemanticMarks::default();
        m.push(MarkKind::PromptStart, 2, 0);
        m.push(MarkKind::PromptStart, 10, 0);
        m.shift_up(5);
        let lines: Vec<usize> = m.all().iter().map(|x| x.line).collect();
        assert_eq!(lines, vec![5], "寄せた後の行がずれている（印が別の行を指す）");
    }

    #[test]
    fn removing_lines_drops_the_marks_inside_and_pulls_the_rest_up() {
        let mut m = SemanticMarks::default();
        for line in [0usize, 3, 4, 9] {
            m.push(MarkKind::PromptStart, line, 0);
        }
        m.remove_lines(3, 4);
        let lines: Vec<usize> = m.all().iter().map(|x| x.line).collect();
        assert_eq!(lines, vec![0, 7], "消した範囲の印が残る / 後ろが詰まっていない");
    }

    #[test]
    fn parses_each_tag() {
        assert_eq!(parse_mark(&[b"A"]), Some(MarkKind::PromptStart));
        assert_eq!(parse_mark(&[b"B"]), Some(MarkKind::CommandStart));
        assert_eq!(parse_mark(&[b"C"]), Some(MarkKind::OutputStart));
        assert_eq!(
            parse_mark(&[b"D", b"0"]),
            Some(MarkKind::CommandEnd { exit_code: Some(0) })
        );
        assert_eq!(
            parse_mark(&[b"D", b"127"]),
            Some(MarkKind::CommandEnd {
                exit_code: Some(127)
            })
        );
        assert_eq!(
            parse_mark(&[b"D"]),
            Some(MarkKind::CommandEnd { exit_code: None })
        );
    }

    #[test]
    fn parses_key_value_exit_code() {
        // 一部のシェル統合は `D;err=2` の形で出す
        assert_eq!(
            parse_mark(&[b"D", b"err=2"]),
            Some(MarkKind::CommandEnd { exit_code: Some(2) })
        );
    }

    #[test]
    fn ignores_extra_attributes_on_a() {
        // `A;aid=1;cl=m` のような付加属性が付いても A は A
        assert_eq!(parse_mark(&[b"A", b"aid=1"]), Some(MarkKind::PromptStart));
    }

    #[test]
    fn folds_into_blocks() {
        let mut m = SemanticMarks::default();
        m.push(MarkKind::PromptStart, 0, 0);
        m.push(MarkKind::CommandStart, 0, 2);
        m.push(MarkKind::OutputStart, 1, 0);
        m.push(MarkKind::CommandEnd { exit_code: Some(0) }, 3, 0);
        m.push(MarkKind::PromptStart, 4, 0);
        m.push(MarkKind::CommandStart, 4, 2);
        m.push(MarkKind::OutputStart, 5, 0);
        m.push(MarkKind::CommandEnd { exit_code: Some(1) }, 7, 0);

        let blocks = m.blocks();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].prompt_line, 0);
        assert_eq!(blocks[0].output_start, Some(1));
        assert_eq!(blocks[0].output_end, Some(3));
        assert!(!blocks[0].is_error());
        assert!(blocks[1].is_error(), "終了コード 1 はエラーブロック");
    }

    #[test]
    fn running_block_has_no_end() {
        let mut m = SemanticMarks::default();
        m.push(MarkKind::PromptStart, 0, 0);
        m.push(MarkKind::OutputStart, 1, 0);
        let blocks = m.blocks();
        assert!(blocks[0].is_running());
    }
}
