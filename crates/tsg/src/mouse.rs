//! マウス操作層。`mouse-parity.md` §3 のポインタ語彙と §5 の委譲。
//!
//! ここに置くのは**判断だけ**で、状態は持たない（クリック連打の計数を除く）。
//! 実際の効果はすべて `Command` としてエンジンへ渡すので、
//! キーボードと同じディスパッチャを通る（`arch.md` の不変条件 1）。

use std::time::{Duration, Instant};

use tsg_mux::protocol::Dir;
use tsg_term::{MouseEncoding, MouseTracking};

/// 連続クリックとみなす間隔。
pub const MULTI_CLICK: Duration = Duration::from_millis(450);

/// ドラッグで範囲を伸ばす単位。クリック回数で決まる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grain {
    Cell,
    Word,
    Line,
}

impl Grain {
    /// クリック回数から。4連クリック以上は行のまま（`mouse-parity.md` §7 の未決事項 2）。
    pub fn of(clicks: u32) -> Self {
        match clicks {
            1 => Grain::Cell,
            2 => Grain::Word,
            _ => Grain::Line,
        }
    }
}

/// ドラッグ中の状態。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drag {
    /// 本文の範囲選択
    Select { pane: u32, grain: Grain },
    /// ペイン境界のリサイズ
    Divider {
        pane: u32,
        dir: Dir,
        /// 掴んだ位置（列 or 行）
        from: usize,
    },
}

/// 連続クリックの計数。
#[derive(Default)]
pub struct Clicks {
    at: Option<Instant>,
    cell: (usize, usize),
    count: u32,
}

impl Clicks {
    /// 押下を記録し、何連クリック目かを返す。
    pub fn press(&mut self, cell: (usize, usize), now: Instant) -> u32 {
        // 1 セルのぶれは許す。ダブルクリックのたびに指が動くのが普通で、
        // 厳密一致にすると「たまに単クリックになる」という最悪の挙動になる。
        let near = self.cell.0.abs_diff(cell.0) <= 1 && self.cell.1 == cell.1;
        self.count = match self.at {
            Some(t) if now.duration_since(t) < MULTI_CLICK && near => self.count + 1,
            _ => 1,
        };
        self.at = Some(now);
        self.cell = cell;
        self.count
    }
}

/// ボタン番号。SGR/レガシー両方で共通の下位ビット。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

impl Button {
    fn code(self) -> u8 {
        match self {
            Button::Left => 0,
            Button::Middle => 1,
            Button::Right => 2,
            Button::WheelUp => 64,
            Button::WheelDown => 65,
        }
    }

    fn is_wheel(self) -> bool {
        matches!(self, Button::WheelUp | Button::WheelDown)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Press,
    Release,
    /// ボタンを押したままの移動
    Drag,
}

/// 子プロセスへ渡すマウスレポート。`mouse-parity.md` §5。
///
/// 何も返さないのは「その段階では報告しない」という意味で、握り潰しではない。
/// 例: 1000（押下と解放のみ）で移動が来ても送らないのが正しい。
pub fn report(
    tracking: MouseTracking,
    encoding: MouseEncoding,
    button: Button,
    phase: Phase,
    col: usize,
    row: usize,
    modifiers: u8,
) -> Option<Vec<u8>> {
    if tracking == MouseTracking::Off {
        return None;
    }
    match phase {
        Phase::Release if tracking == MouseTracking::X10 => return None,
        Phase::Drag => match tracking {
            MouseTracking::ButtonEvent | MouseTracking::AnyEvent => {}
            _ => return None,
        },
        _ => {}
    }
    // ホイールは解放を持たない
    if button.is_wheel() && phase != Phase::Press {
        return None;
    }

    let mut cb = button.code() | modifiers;
    if phase == Phase::Drag {
        cb |= 32;
    }
    // 1-origin
    let (x, y) = (col + 1, row + 1);

    Some(match encoding {
        MouseEncoding::Sgr => {
            let end = if phase == Phase::Release { 'm' } else { 'M' };
            format!("\x1b[<{cb};{x};{y}{end}").into_bytes()
        }
        MouseEncoding::Urxvt => {
            // 解放はボタン 3 で表す（レガシーと同じ規約）
            let cb = if phase == Phase::Release { 3 | modifiers } else { cb };
            format!("\x1b[{};{x};{y}M", cb + 32).into_bytes()
        }
        MouseEncoding::Utf8 => {
            let cb = if phase == Phase::Release { 3 | modifiers } else { cb };
            let mut out = b"\x1b[M".to_vec();
            for v in [u32::from(cb) + 32, x as u32 + 32, y as u32 + 32] {
                let mut buf = [0u8; 4];
                out.extend_from_slice(
                    char::from_u32(v).unwrap_or('\u{fffd}').encode_utf8(&mut buf).as_bytes(),
                );
            }
            out
        }
        MouseEncoding::Default => {
            let cb = if phase == Phase::Release { 3 | modifiers } else { cb };
            // 223 を超えると表現できない。座標を送らずに黙るより、端で止める。
            let clip = |v: usize| (v.min(223) + 32) as u8;
            vec![0x1b, b'[', b'M', cb + 32, clip(x), clip(y)]
        }
    })
}

/// 修飾キーのビット（Shift 4 / Alt 8 / Ctrl 16）。
pub fn modifier_bits(shift: bool, alt: bool, ctrl: bool) -> u8 {
    u8::from(shift) * 4 + u8::from(alt) * 8 + u8::from(ctrl) * 16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clicks_accumulate_then_reset() {
        let mut c = Clicks::default();
        let t0 = Instant::now();
        assert_eq!(c.press((5, 5), t0), 1);
        assert_eq!(c.press((5, 5), t0 + Duration::from_millis(100)), 2);
        assert_eq!(c.press((6, 5), t0 + Duration::from_millis(200)), 3, "1 列のぶれは許す");
        assert_eq!(
            c.press((5, 5), t0 + Duration::from_millis(900)),
            1,
            "間隔が空いたら数え直す"
        );
        assert_eq!(c.press((40, 5), t0 + Duration::from_millis(950)), 1, "離れた位置は別のクリック");
    }

    #[test]
    fn grain_follows_the_click_count() {
        assert_eq!(Grain::of(1), Grain::Cell);
        assert_eq!(Grain::of(2), Grain::Word);
        assert_eq!(Grain::of(3), Grain::Line);
        assert_eq!(Grain::of(4), Grain::Line);
    }

    #[test]
    fn sgr_encoding_matches_the_1006_form() {
        let got = report(
            MouseTracking::Normal,
            MouseEncoding::Sgr,
            Button::Left,
            Phase::Press,
            0,
            0,
            0,
        );
        assert_eq!(got.as_deref(), Some(&b"\x1b[<0;1;1M"[..]));

        let up = report(
            MouseTracking::Normal,
            MouseEncoding::Sgr,
            Button::Left,
            Phase::Release,
            9,
            4,
            0,
        );
        assert_eq!(up.as_deref(), Some(&b"\x1b[<0;10;5m"[..]), "解放は小文字の m");
    }

    #[test]
    fn legacy_encoding_offsets_by_32() {
        let got = report(
            MouseTracking::Normal,
            MouseEncoding::Default,
            Button::Right,
            Phase::Press,
            2,
            3,
            0,
        )
        .unwrap();
        assert_eq!(got, vec![0x1b, b'[', b'M', 32 + 2, 32 + 3, 32 + 4]);
    }

    #[test]
    fn tracking_level_decides_what_is_reported() {
        let call = |t: MouseTracking, p: Phase| {
            report(t, MouseEncoding::Sgr, Button::Left, p, 0, 0, 0).is_some()
        };

        assert!(!call(MouseTracking::Off, Phase::Press), "無効なら何も送らない");

        assert!(call(MouseTracking::X10, Phase::Press));
        assert!(!call(MouseTracking::X10, Phase::Release), "X10 は押下だけ");

        assert!(call(MouseTracking::Normal, Phase::Release));
        assert!(!call(MouseTracking::Normal, Phase::Drag), "1000 は移動を送らない");

        assert!(call(MouseTracking::ButtonEvent, Phase::Drag));
        assert!(call(MouseTracking::AnyEvent, Phase::Drag));
    }

    #[test]
    fn drag_sets_the_motion_bit() {
        let got = report(
            MouseTracking::ButtonEvent,
            MouseEncoding::Sgr,
            Button::Left,
            Phase::Drag,
            0,
            0,
            0,
        )
        .unwrap();
        assert_eq!(got, b"\x1b[<32;1;1M");
    }

    #[test]
    fn the_wheel_reports_only_a_press() {
        assert!(
            report(
                MouseTracking::Normal,
                MouseEncoding::Sgr,
                Button::WheelDown,
                Phase::Press,
                0,
                0,
                0
            )
            .is_some()
        );
        assert!(
            report(
                MouseTracking::Normal,
                MouseEncoding::Sgr,
                Button::WheelDown,
                Phase::Release,
                0,
                0,
                0
            )
            .is_none(),
            "ホイールに解放は無い"
        );
    }

    #[test]
    fn modifiers_ride_in_the_button_byte() {
        let got = report(
            MouseTracking::Normal,
            MouseEncoding::Sgr,
            Button::Left,
            Phase::Press,
            0,
            0,
            modifier_bits(true, false, true),
        )
        .unwrap();
        assert_eq!(got, b"\x1b[<20;1;1M", "Shift(4) + Ctrl(16)");
    }
}
