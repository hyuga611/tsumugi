//! キー入力 -> PTY へ送るバイト列。
//!
//! M0-b の範囲。本来はここが `tsg-modal` の `Command` を生む場所になる
//! （`arch.md` の不変条件 1「単一コマンドバス」）が、スパイクでは
//! Insert モードの素通しに必要な最小限だけを持つ。

use winit::keyboard::{Key, ModifiersState, NamedKey};

/// 押されたキーを、Insert モードで PTY へ送るバイト列に変換する。
pub fn encode(
    key: &Key,
    text: Option<&str>,
    mods: ModifiersState,
    app_cursor: bool,
) -> Option<Vec<u8>> {
    let ctrl = mods.control_key();
    let alt = mods.alt_key();

    // 矢印などは、アプリケーションカーソルキーモードで SS3 に変わる。
    let cursor = |c: u8| -> Vec<u8> {
        if app_cursor {
            vec![0x1b, b'O', c]
        } else {
            vec![0x1b, b'[', c]
        }
    };

    let bytes = match key {
        Key::Named(named) => match named {
            // Shift＋Enter は**改行**。送るのは `ESC CR`。
            //
            // 端末には「送信」と「改行」を区別する仕組みが元から無いので、
            // 受け取る側（Claude Code・Codex・多くの REPL）が決めた合図に
            // 合わせる。iTerm2 や VS Code が `/terminal-setup` で入れるのも
            // この 2 バイトで、素の `CR` を送ると 1 行で送信されてしまう。
            NamedKey::Enter if mods.shift_key() => vec![0x1b, b'\r'],
            NamedKey::Enter => vec![b'\r'],
            NamedKey::Backspace => vec![0x7f],
            NamedKey::Tab => vec![b'\t'],
            NamedKey::Escape => vec![0x1b],
            NamedKey::Space => vec![b' '],
            NamedKey::ArrowUp => cursor(b'A'),
            NamedKey::ArrowDown => cursor(b'B'),
            NamedKey::ArrowRight => cursor(b'C'),
            NamedKey::ArrowLeft => cursor(b'D'),
            NamedKey::Home => cursor(b'H'),
            NamedKey::End => cursor(b'F'),
            NamedKey::Delete => b"\x1b[3~".to_vec(),
            NamedKey::Insert => b"\x1b[2~".to_vec(),
            NamedKey::PageUp => b"\x1b[5~".to_vec(),
            NamedKey::PageDown => b"\x1b[6~".to_vec(),
            NamedKey::F1 => b"\x1bOP".to_vec(),
            NamedKey::F2 => b"\x1bOQ".to_vec(),
            NamedKey::F3 => b"\x1bOR".to_vec(),
            NamedKey::F4 => b"\x1bOS".to_vec(),
            _ => return None,
        },
        Key::Character(s) => {
            if ctrl {
                // Ctrl+A..Z -> 0x01..0x1a、Ctrl+[ \ ] ^ _ もまとめて扱う。
                let c = s.chars().next()?;
                let upper = c.to_ascii_uppercase();
                match upper {
                    'A'..='Z' => vec![upper as u8 - b'A' + 1],
                    '[' => vec![0x1b],
                    '\\' => vec![0x1c],
                    ']' => vec![0x1d],
                    '^' => vec![0x1e],
                    '_' => vec![0x1f],
                    ' ' => vec![0x00],
                    _ => return None,
                }
            } else {
                text.unwrap_or(s.as_str()).as_bytes().to_vec()
            }
        }
        _ => return None,
    };

    // Alt 修飾は ESC 前置（メタ）。
    if alt && !bytes.is_empty() && bytes[0] != 0x1b {
        let mut v = vec![0x1b];
        v.extend_from_slice(&bytes);
        return Some(v);
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Enter は送信、Shift＋Enter は改行。**
    ///
    /// 端末には元々この区別が無いので、受け取る側（Claude Code・Codex・
    /// 多くの REPL）が決めた `ESC CR` に合わせる。素の `CR` を送ると、
    /// 改行のつもりが 1 行で送信されてしまう。
    #[test]
    fn shift_enter_sends_a_newline_not_a_submit() {
        let enter = Key::Named(NamedKey::Enter);
        assert_eq!(
            encode(&enter, None, ModifiersState::empty(), false),
            Some(vec![b'\r'])
        );
        assert_eq!(
            encode(&enter, None, ModifiersState::SHIFT, false),
            Some(vec![0x1b, b'\r'])
        );
    }
}
