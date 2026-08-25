//! SGR — 色と文字属性。
//!
//! セルが持つのは「意味」だけで、実際の RGB は描画側が決める。
//! `Color::Default` を残しているのはそのためで、既定色をここで確定させると
//! テーマの切り替えができなくなる。

/// セルの色。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    /// 端末の既定色。実際の値は描画側のテーマが決める。
    #[default]
    Default,
    /// 0-7 標準色 / 8-15 明色 / 16-231 6×6×6 キューブ / 232-255 灰階調
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    /// 太字のときに標準色を明色へ繰り上げる（xterm 以来の慣習）。
    pub fn brighten(self) -> Self {
        match self {
            Color::Indexed(i) if i < 8 => Color::Indexed(i + 8),
            other => other,
        }
    }
}

/// 標準 16 色の**既定値**。実際に使う 16 色はテーマが持つ
/// （`tsg::theme`）。ここに在るのは、テーマを持たない道（プローブ・テスト・
/// 行の ANSI 復元）が参照する素の表。
pub const BASE16: [[u8; 3]; 16] = [
    [0x15, 0x18, 0x1d], // 0 黒
    [0xe0, 0x5a, 0x63], // 1 赤
    [0x7f, 0xc0, 0x6e], // 2 緑
    [0xd8, 0xa6, 0x57], // 3 黄
    [0x5a, 0xa9, 0xe6], // 4 青
    [0xc6, 0x78, 0xdd], // 5 マゼンタ
    [0x56, 0xb6, 0xc2], // 6 シアン
    [0xc3, 0xc7, 0xcf], // 7 白
    [0x4b, 0x51, 0x5c], // 8 明るい黒
    [0xff, 0x7b, 0x83], // 9 明るい赤
    [0x9e, 0xde, 0x8b], // 10 明るい緑
    [0xf0, 0xc6, 0x74], // 11 明るい黄
    [0x7c, 0xc4, 0xff], // 12 明るい青
    [0xdd, 0x9b, 0xef], // 13 明るいマゼンタ
    [0x74, 0xd3, 0xde], // 14 明るいシアン
    [0xec, 0xef, 0xf4], // 15 明るい白
];

/// 6×6×6 キューブの各段の輝度（xterm と同じ）。
const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// インデックス色を RGB へ。
///
/// **16 以降はテーマで動かさない。** アプリが `[38;5;208m` と書くとき
/// 期待しているのは決まった橙で、そこを振り替えると出力が嘘になる。
/// 0-15 だけはテーマが差し替える（`tsg::theme::Theme::resolve`）。
pub fn indexed_rgb(i: u8) -> [u8; 3] {
    match i {
        0..=15 => BASE16[i as usize],
        16..=231 => {
            let n = i - 16;
            [
                CUBE[(n / 36) as usize],
                CUBE[(n / 6 % 6) as usize],
                CUBE[(n % 6) as usize],
            ]
        }
        _ => {
            let v = 8 + (i as u16 - 232) * 10;
            let v = v as u8;
            [v, v, v]
        }
    }
}

/// 下線の引き方（SGR `4:n`）。
///
/// **コンパイラの出力が読めるかどうかがここで決まる。** rustc も clang も
/// 誤りの位置を波線で指す。`4` としか読めないと、警告も誤りも同じ 1 本線に
/// なって、色でしか区別が付かなくなる。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Underline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

impl Underline {
    /// SGR の `4:n` の n。
    pub fn from_style(n: u16) -> Self {
        match n {
            1 => Self::Single,
            2 => Self::Double,
            3 => Self::Curly,
            4 => Self::Dotted,
            5 => Self::Dashed,
            _ => Self::None,
        }
    }

    fn style(self) -> u16 {
        match self {
            Self::None => 0,
            Self::Single => 1,
            Self::Double => 2,
            Self::Curly => 3,
            Self::Dotted => 4,
            Self::Dashed => 5,
        }
    }
}

/// 文字属性。ビットで持つ（`bitflags` を足すほどの規模ではない）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attrs {
    pub fg: Color,
    pub bg: Color,
    pub flags: u8,
    /// 下線の引き方。`UNDERLINE` の旗と**必ず同時に動く**
    /// （旗だけ見る古い経路が嘘をつかないように）。
    pub under: Underline,
    /// 下線の色（SGR 58）。`Default` は文字と同じ色。
    pub ul_color: Color,
}

impl Attrs {
    pub const BOLD: u8 = 1 << 0;
    pub const DIM: u8 = 1 << 1;
    pub const ITALIC: u8 = 1 << 2;
    pub const UNDERLINE: u8 = 1 << 3;
    pub const BLINK: u8 = 1 << 4;
    pub const REVERSE: u8 = 1 << 5;
    pub const HIDDEN: u8 = 1 << 6;
    pub const STRIKE: u8 = 1 << 7;

    pub fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }

    pub fn set(&mut self, flag: u8) {
        self.flags |= flag;
    }

    pub fn unset(&mut self, flag: u8) {
        self.flags &= !flag;
    }

    /// 下線を引き方ごと決める。旗も揃える。
    pub fn set_underline(&mut self, under: Underline) {
        self.under = under;
        if under == Underline::None {
            self.unset(Self::UNDERLINE);
        } else {
            self.set(Self::UNDERLINE);
        }
    }

    /// 消去したセルが引き継ぐもの。
    ///
    /// 背景色だけを持ち越す。xterm 以来の「背景で消す」挙動で、
    /// これが無いと色付きの `clear` が抜けて見える。
    pub fn erased(&self) -> Self {
        Self {
            fg: Color::Default,
            bg: self.bg,
            flags: 0,
            under: Underline::None,
            ul_color: Color::Default,
        }
    }

    /// 反転を解決した (前景, 背景)。描画側が毎セル呼ぶ。
    pub fn resolved(&self) -> (Color, Color) {
        if self.has(Self::REVERSE) {
            (self.bg, self.fg)
        } else {
            (self.fg, self.bg)
        }
    }

    /// この属性を再現する SGR シーケンス。
    ///
    /// 再アタッチのスナップショットで使う。サーバが吐き、クライアントは
    /// 同じ `tsg-term` で解析する。差分の直列化を実装しないための往復路。
    pub fn sgr(&self) -> String {
        let mut out = String::from("\x1b[0");
        for (flag, code) in [
            (Self::BOLD, 1),
            (Self::DIM, 2),
            (Self::ITALIC, 3),
            (Self::UNDERLINE, 4),
            (Self::BLINK, 5),
            (Self::REVERSE, 7),
            (Self::HIDDEN, 8),
            (Self::STRIKE, 9),
        ] {
            if self.has(flag) {
                out.push(';');
                out.push_str(&code.to_string());
            }
        }
        // 下線は引き方まで書く。**1 本線に丸めない**（波線が消えると
        // 誤りと警告が見分けられなくなる）。
        if self.under != Underline::None && self.under != Underline::Single {
            out.push_str(&format!(";4:{}", self.under.style()));
        }
        push_color(&mut out, self.fg, 38);
        push_color(&mut out, self.bg, 48);
        push_color(&mut out, self.ul_color, 58);
        out.push('m');
        out
    }
}

fn push_color(out: &mut String, color: Color, base: u8) {
    match color {
        Color::Default => {}
        Color::Indexed(i) => out.push_str(&format!(";{base};5;{i}")),
        Color::Rgb(r, g, b) => out.push_str(&format!(";{base};2;{r};{g};{b}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cube_and_grayscale_resolve() {
        assert_eq!(indexed_rgb(16), [0, 0, 0]);
        assert_eq!(indexed_rgb(231), [255, 255, 255]);
        assert_eq!(indexed_rgb(232), [8, 8, 8]);
        assert_eq!(indexed_rgb(255), [238, 238, 238]);
        // 196 = 赤（キューブの r=5, g=0, b=0）
        assert_eq!(indexed_rgb(196), [255, 0, 0]);
    }

    #[test]
    fn bold_promotes_only_the_standard_eight() {
        assert_eq!(Color::Indexed(1).brighten(), Color::Indexed(9));
        assert_eq!(Color::Indexed(9).brighten(), Color::Indexed(9));
        assert_eq!(Color::Rgb(1, 2, 3).brighten(), Color::Rgb(1, 2, 3));
    }

    #[test]
    fn reverse_swaps_at_resolution_time_not_in_storage() {
        let mut a = Attrs {
            fg: Color::Indexed(1),
            bg: Color::Indexed(4),
            ..Attrs::default()
        };
        assert_eq!(a.resolved(), (Color::Indexed(1), Color::Indexed(4)));
        a.set(Attrs::REVERSE);
        assert_eq!(a.resolved(), (Color::Indexed(4), Color::Indexed(1)));
        assert_eq!(a.fg, Color::Indexed(1), "保存された色は入れ替えない");
    }

    #[test]
    fn erase_keeps_the_background_only() {
        let a = Attrs {
            fg: Color::Indexed(3),
            bg: Color::Rgb(10, 20, 30),
            flags: Attrs::BOLD | Attrs::UNDERLINE,
            ..Attrs::default()
        };
        let e = a.erased();
        assert_eq!(e.bg, Color::Rgb(10, 20, 30));
        assert_eq!(e.fg, Color::Default);
        assert_eq!(e.flags, 0);
    }

    #[test]
    fn sgr_round_trips_through_the_parser() {
        // 実際の往復は lib.rs 側のテストで見る。ここは形だけ。
        let a = Attrs {
            fg: Color::Indexed(12),
            bg: Color::Rgb(1, 2, 3),
            flags: Attrs::BOLD | Attrs::UNDERLINE,
            under: Underline::Single,
            ..Attrs::default()
        };
        assert_eq!(a.sgr(), "\x1b[0;1;4;38;5;12;48;2;1;2;3m");
        assert_eq!(Attrs::default().sgr(), "\x1b[0m");
    }

    /// **引き方も色も往復する。** 波線が 1 本線に戻ると、
    /// 再アタッチしただけで誤りの印が消える。
    #[test]
    fn a_curly_underline_survives_the_round_trip() {
        let mut a = Attrs::default();
        a.set_underline(Underline::Curly);
        a.ul_color = Color::Rgb(255, 0, 0);
        assert_eq!(a.sgr(), "[0;4;4:3;58;2;255;0;0m");
        assert!(a.has(Attrs::UNDERLINE), "旗が揃っていない");

        a.set_underline(Underline::None);
        assert!(!a.has(Attrs::UNDERLINE), "消したのに旗が残っている");
    }
}
