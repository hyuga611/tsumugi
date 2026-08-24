//! 色。**画面の色は全部ここから来る。**
//!
//! 設計の要点は 2 つ。
//!
//! 1. **書くのは「素」の色だけ。** 背景・前景・カーソル・選択と、端末の 16 色。
//!    これは世の中のテーマが既に持っている形なので、手持ちの配色をそのまま
//!    移せる。ステータス行・パネル・区切り・モード帯といった飾りは、そこから
//!    決まった規則で**導く**。40 個の色を手で揃えさせない。
//! 2. **導いた値は上書きできる。** 規則は大体うまくいくが、どうしても
//!    合わない 1 個が必ず出る。そこで設計が詰むのを避ける。
//!
//! 構文強調を端末の 16 色と別に持つのは、SGR が出す色とぶつけないため。
//! 出力に色が付いている行へ上から塗ると、その色が嘘になる。

use tsg_term::attrs::indexed_rgb;

pub type Rgba = [f32; 4];

/// 画面が使う色の全部。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    /// 既定の背景。レンダラのクリア色でもあり、`Color::Default` の解決先でもある。
    pub bg: Rgba,
    pub fg: Rgba,
    /// 目立たせない字（説明・行番号・非アクティブ）。
    pub dim: Rgba,
    pub cursor: Rgba,
    pub selection: Rgba,

    pub status_bg: Rgba,
    pub status_fg: Rgba,
    /// IME の未確定文字。
    pub preedit: Rgba,

    pub divider: Rgba,
    pub divider_active: Rgba,
    pub tab_active: Rgba,

    pub panel_bg: Rgba,
    pub panel_edge: Rgba,
    pub panel_sel: Rgba,

    /// 左ガター。OSC 133 が教えるコマンドの成否。
    pub gut_ok: Rgba,
    pub gut_err: Rgba,
    pub gut_run: Rgba,
    pub gut_mark: Rgba,

    pub hover: Rgba,
    pub accent: Rgba,

    /// モードの色帯。**字を読まなくても今どちらか分かる**ための唯一の手がかり。
    pub mode_insert: Rgba,
    pub mode_normal: Rgba,
    pub mode_visual: Rgba,
    pub mode_layout: Rgba,
    pub mode_pending: Rgba,
    pub mode_fg: Rgba,

    pub syn_comment: Rgba,
    pub syn_str: Rgba,
    pub syn_num: Rgba,
    pub syn_key: Rgba,

    /// 記録中（マクロ）。
    pub rec_on: Rgba,

    pub help_bg: Rgba,
    pub help_title: Rgba,
    pub help_body: Rgba,
    pub help_note: Rgba,

    /// 端末の 16 色。SGR の `Indexed(0..16)` がここを引く。
    pub ansi: [Rgba; 16],
}

impl Default for Theme {
    fn default() -> Self {
        builtin(DEFAULT_THEME).expect("既定のテーマが無い")
    }
}

impl Theme {
    /// 色を背景へ寄せる。非アクティブなペインや薄い字に使う。
    ///
    /// 黒へ寄せずに**背景へ**寄せるのが要点。明るいテーマで黒へ寄せると、
    /// 薄くしたはずの字が逆に濃くなる。
    pub fn fade(&self, c: Rgba, t: f32) -> Rgba {
        [
            c[0] + (self.bg[0] - c[0]) * t,
            c[1] + (self.bg[1] - c[1]) * t,
            c[2] + (self.bg[2] - c[2]) * t,
            c[3],
        ]
    }

    /// SGR の色を解決する。`Default` だけは呼ぶ側（前景か背景か）が決める。
    ///
    /// 0-15 は**テーマが持つ**。16 以降（6×6×6 キューブと灰階調）は xterm と
    /// 同じ値で、テーマでは動かさない。アプリが `\x1b[38;5;208m` と書くとき
    /// 期待しているのは決まった橙で、そこを勝手に振り替えると嘘になる。
    pub fn resolve(&self, c: tsg_term::Color) -> Option<Rgba> {
        match c {
            tsg_term::Color::Default => None,
            tsg_term::Color::Rgb(r, g, b) => Some(rgba([r, g, b])),
            tsg_term::Color::Indexed(i) if (i as usize) < 16 => Some(self.ansi[i as usize]),
            tsg_term::Color::Indexed(i) => Some(rgba(indexed_rgb(i))),
        }
    }
}

// ---------------------------------------------------------------------------
// 素の色と、そこからの導出

/// 設定に書く「素」の色。
#[derive(Clone, Copy, Debug)]
pub struct Seed {
    pub bg: Rgba,
    pub fg: Rgba,
    pub cursor: Rgba,
    pub selection: Rgba,
    pub ansi: [Rgba; 16],
    /// 暗いテーマか。飾りを**背景から持ち上げる向き**が変わる。
    pub dark: bool,
}

impl Seed {
    /// 規則で残りを埋める。
    pub fn build(&self) -> Theme {
        let s = self;
        // 背景から少し持ち上げる（暗ければ明るく、明るければ暗く）。
        let lift = |t: f32| mix(s.bg, if s.dark { WHITE } else { BLACK }, t);
        // 端末の色を、飾りとして使える濃さまで背景へ落とす。
        let calm = |c: Rgba, t: f32| mix(c, s.bg, t);
        // モードの帯は**塗りつぶしの上に字が載る**。背景へ寄せると、明るい
        // テーマでは帯まで明るくなって字が消える。帯だけは常に「濃くする」向きへ
        // 動かし、その上に明るい字を置く。
        let band = |c: Rgba, t: f32| mix(c, if s.dark { s.bg } else { BLACK }, t);
        let dim = mix(s.fg, s.bg, 0.45);

        Theme {
            bg: s.bg,
            fg: s.fg,
            dim,
            cursor: s.cursor,
            selection: s.selection,

            status_bg: lift(0.07),
            status_fg: mix(s.fg, s.bg, 0.28),
            preedit: s.ansi[11],

            divider: lift(0.17),
            divider_active: calm(s.ansi[12], 0.28),
            tab_active: lift(0.15),

            panel_bg: lift(0.05),
            panel_edge: lift(0.25),
            panel_sel: calm(s.ansi[4], 0.35),

            gut_ok: calm(s.ansi[2], 0.30),
            gut_err: calm(s.ansi[1], 0.10),
            gut_run: calm(s.ansi[3], 0.18),
            gut_mark: calm(s.ansi[12], 0.20),

            hover: s.ansi[12],
            accent: mix(s.ansi[12], s.fg, 0.40),

            mode_insert: band(s.ansi[4], 0.42),
            mode_normal: band(s.ansi[2], 0.48),
            mode_visual: band(s.ansi[3], 0.42),
            mode_layout: band(s.ansi[5], 0.48),
            mode_pending: mix(s.bg, if s.dark { WHITE } else { BLACK }, if s.dark { 0.30 } else { 0.62 }),
            mode_fg: if s.dark { WHITE } else { mix(s.bg, WHITE, 0.6) },

            syn_comment: mix(calm(s.ansi[2], 0.45), dim, 0.55),
            syn_str: calm(s.ansi[3], 0.12),
            syn_num: calm(s.ansi[6], 0.10),
            syn_key: calm(s.ansi[13], 0.12),

            rec_on: s.ansi[9],

            // 使い方の面は**沈める**。明るいテーマで白へ寄せると、既に白い背景と
            // 見分けが付かず、面として立たない。どちらの明暗でも暗い側へ動かす。
            help_bg: mix(s.bg, BLACK, if s.dark { 0.35 } else { 0.05 }),
            help_title: s.ansi[11],
            help_body: mix(s.fg, s.bg, 0.05),
            help_note: dim,

            ansi: s.ansi,
        }
    }
}

const WHITE: Rgba = [1.0, 1.0, 1.0, 1.0];
const BLACK: Rgba = [0.0, 0.0, 0.0, 1.0];

/// `a` を `b` へ `t` だけ寄せる。アルファは `a` のものを保つ
/// （半透明のカーソルを混ぜて不透明にしない）。
fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
        a[3],
    ]
}

pub fn rgba(v: [u8; 3]) -> Rgba {
    [
        f32::from(v[0]) / 255.0,
        f32::from(v[1]) / 255.0,
        f32::from(v[2]) / 255.0,
        1.0,
    ]
}

fn rgba_a(v: [u8; 3], a: f32) -> Rgba {
    let mut c = rgba(v);
    c[3] = a;
    c
}

fn ansi16(v: [[u8; 3]; 16]) -> [Rgba; 16] {
    v.map(rgba)
}

// ---------------------------------------------------------------------------
// 組み込みのテーマ

pub const DEFAULT_THEME: &str = "夜霧";

/// 名前で引ける組み込みテーマ。日本語名と英語名のどちらでも引ける
/// （設定ファイルを英語環境で書く人が居る）。
pub const BUILTIN: &[(&str, &str)] = &[
    ("夜霧", "yogiri"),
    ("墨", "sumi"),
    ("白磁", "hakuji"),
];

pub fn names() -> Vec<&'static str> {
    BUILTIN.iter().map(|(ja, _)| *ja).collect()
}

/// 組み込みテーマ。知らない名前なら `None`。
pub fn builtin(name: &str) -> Option<Theme> {
    let key = BUILTIN
        .iter()
        .find(|(ja, en)| name.eq_ignore_ascii_case(en) || name == *ja)
        .map(|(ja, _)| *ja)?;
    Some(match key {
        "夜霧" => yogiri(),
        "墨" => sumi(),
        "白磁" => hakuji(),
        _ => return None,
    })
}

/// 夜霧 — 既定。青みの暗い背景に、彩度を落とした 16 色。
fn yogiri() -> Theme {
    Seed {
        bg: rgba([0x0f, 0x12, 0x17]),
        fg: rgba([0xde, 0xe1, 0xe6]),
        cursor: rgba_a([0x66, 0xb8, 0xf2], 0.55),
        selection: rgba_a([0x4c, 0x73, 0x9e], 0.45),
        dark: true,
        ansi: ansi16([
            [0x15, 0x18, 0x1d],
            [0xe0, 0x5a, 0x63],
            [0x7f, 0xc0, 0x6e],
            [0xd8, 0xa6, 0x57],
            [0x5a, 0xa9, 0xe6],
            [0xc6, 0x78, 0xdd],
            [0x56, 0xb6, 0xc2],
            [0xc3, 0xc7, 0xcf],
            [0x4b, 0x51, 0x5c],
            [0xff, 0x7b, 0x83],
            [0x9e, 0xde, 0x8b],
            [0xf0, 0xc6, 0x74],
            [0x7c, 0xc4, 0xff],
            [0xdd, 0x9b, 0xef],
            [0x74, 0xd3, 0xde],
            [0xec, 0xef, 0xf4],
        ]),
    }
    .build()
}

/// 墨 — 色味を抜いた高コントラスト。背景が真っ黒に近いので、
/// 透過を切って使うとよく締まる。
fn sumi() -> Theme {
    Seed {
        bg: rgba([0x08, 0x08, 0x09]),
        fg: rgba([0xed, 0xed, 0xef]),
        cursor: rgba_a([0xff, 0xff, 0xff], 0.60),
        selection: rgba_a([0x55, 0x58, 0x60], 0.55),
        dark: true,
        ansi: ansi16([
            [0x14, 0x14, 0x16],
            [0xf2, 0x6d, 0x6d],
            [0x7a, 0xd6, 0x8a],
            [0xe6, 0xc0, 0x6a],
            [0x74, 0xb4, 0xf5],
            [0xd0, 0x92, 0xe8],
            [0x69, 0xcf, 0xd6],
            [0xd6, 0xd6, 0xda],
            [0x5c, 0x5c, 0x64],
            [0xff, 0x92, 0x92],
            [0x9c, 0xed, 0xa8],
            [0xff, 0xda, 0x8a],
            [0x9a, 0xcd, 0xff],
            [0xe4, 0xb1, 0xf7],
            [0x8e, 0xe6, 0xed],
            [0xff, 0xff, 0xff],
        ]),
    }
    .build()
}

/// 白磁 — 明るいところで使うための白地。
///
/// **暗いテーマだけを配るのは「昼に使えない」と同じ。** 屋外や明るい
/// オフィスでは白地でないと読めない。
fn hakuji() -> Theme {
    Seed {
        bg: rgba([0xfa, 0xf9, 0xf6]),
        fg: rgba([0x24, 0x28, 0x2e]),
        cursor: rgba_a([0x1f, 0x6f, 0xb2], 0.45),
        selection: rgba_a([0x8a, 0xb4, 0xdc], 0.50),
        dark: false,
        ansi: ansi16([
            [0x2a, 0x2e, 0x34],
            [0xc0, 0x39, 0x3f],
            [0x2f, 0x7d, 0x3a],
            [0x9a, 0x6c, 0x12],
            [0x1f, 0x62, 0xa8],
            [0x86, 0x3c, 0xa8],
            [0x11, 0x76, 0x86],
            [0x6b, 0x70, 0x78],
            [0x4c, 0x51, 0x59],
            [0xd8, 0x4c, 0x52],
            [0x3d, 0x94, 0x4a],
            [0xb0, 0x80, 0x1c],
            [0x2c, 0x77, 0xc4],
            [0x9c, 0x4f, 0xc0],
            [0x18, 0x8b, 0x9d],
            [0x1a, 0x1d, 0x22],
        ]),
    }
    .build()
}

// ---------------------------------------------------------------------------
// 設定からの上書き

/// `#rgb` / `#rrggbb` / `#rrggbbaa` を読む。読めなければ `None`。
pub fn parse_color(s: &str) -> Option<Rgba> {
    let h = s.strip_prefix('#')?;
    let n = |i: usize, len: usize| -> Option<f32> {
        let part = h.get(i..i + len)?;
        let v = u8::from_str_radix(&part.repeat(3 - len), 16).ok()?;
        Some(f32::from(v) / 255.0)
    };
    match h.len() {
        3 => Some([n(0, 1)?, n(1, 1)?, n(2, 1)?, 1.0]),
        6 => Some([n(0, 2)?, n(2, 2)?, n(4, 2)?, 1.0]),
        8 => Some([n(0, 2)?, n(2, 2)?, n(4, 2)?, n(6, 2)?]),
        _ => None,
    }
}

/// 名前を付けた 1 色を上書きする。知らない名前なら `false`。
///
/// 名前を文字列で引くのは、設定ファイルの綴りをコードの綴りに合わせるため。
/// 表を 2 つ持つと、片方だけ足して**設定に書いたのに効かない**が起きる。
pub fn set_named(t: &mut Theme, key: &str, c: Rgba) -> bool {
    let slot: &mut Rgba = match key {
        "background" | "bg" => &mut t.bg,
        "foreground" | "fg" => &mut t.fg,
        "dim" => &mut t.dim,
        "cursor" => &mut t.cursor,
        "selection" => &mut t.selection,
        "status_bg" => &mut t.status_bg,
        "status_fg" => &mut t.status_fg,
        "preedit" => &mut t.preedit,
        "divider" => &mut t.divider,
        "divider_active" => &mut t.divider_active,
        "tab_active" => &mut t.tab_active,
        "panel_bg" => &mut t.panel_bg,
        "panel_edge" => &mut t.panel_edge,
        "panel_sel" => &mut t.panel_sel,
        "gutter_ok" => &mut t.gut_ok,
        "gutter_error" => &mut t.gut_err,
        "gutter_running" => &mut t.gut_run,
        "gutter_mark" => &mut t.gut_mark,
        "hover" => &mut t.hover,
        "accent" => &mut t.accent,
        "mode_insert" => &mut t.mode_insert,
        "mode_normal" => &mut t.mode_normal,
        "mode_visual" => &mut t.mode_visual,
        "mode_layout" => &mut t.mode_layout,
        "mode_pending" => &mut t.mode_pending,
        "mode_fg" => &mut t.mode_fg,
        "syntax_comment" => &mut t.syn_comment,
        "syntax_string" => &mut t.syn_str,
        "syntax_number" => &mut t.syn_num,
        "syntax_keyword" => &mut t.syn_key,
        "recording" => &mut t.rec_on,
        "help_bg" => &mut t.help_bg,
        "help_title" => &mut t.help_title,
        "help_body" => &mut t.help_body,
        "help_note" => &mut t.help_note,
        _ => {
            // ansi0 … ansi15
            let Some(n) = key.strip_prefix("ansi") else {
                return false;
            };
            let Ok(i) = n.parse::<usize>() else {
                return false;
            };
            if i >= 16 {
                return false;
            }
            t.ansi[i] = c;
            return true;
        }
    };
    *slot = c;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_name_resolves_in_both_spellings() {
        for (ja, en) in BUILTIN {
            assert!(builtin(ja).is_some(), "{ja} が引けない");
            assert!(builtin(en).is_some(), "{en} が引けない");
            assert!(builtin(&en.to_uppercase()).is_some(), "{en} は大文字で引けない");
        }
        assert!(builtin("そんな名前は無い").is_none());
    }

    /// 前景と背景が近すぎると**字が読めない**。テーマを足したときに
    /// ここで落ちるようにしておく。
    #[test]
    fn text_stands_out_from_the_background_in_every_theme() {
        for (name, _) in BUILTIN {
            let t = builtin(name).expect("引けない");
            let d = contrast(t.fg, t.bg);
            assert!(d > 0.55, "{name}: 前景と背景が近すぎる（{d:.2}）");
            let s = contrast(t.status_fg, t.status_bg);
            assert!(s > 0.25, "{name}: ステータス行が読めない（{s:.2}）");
            for (i, c) in t.ansi.iter().enumerate().skip(1) {
                let d = contrast(*c, t.bg);
                assert!(d > 0.15, "{name}: ansi{i} が背景に沈む（{d:.2}）");
            }
        }
    }

    /// モード帯は塗りつぶしの上に字が載る。帯と字が近いと読めない。
    #[test]
    fn the_mode_band_can_be_read_in_every_theme() {
        for (name, _) in BUILTIN {
            let t = builtin(name).expect("引けない");
            for (label, band) in [
                ("挿入", t.mode_insert),
                ("通常", t.mode_normal),
                ("選択", t.mode_visual),
                ("配置", t.mode_layout),
                ("待ち", t.mode_pending),
            ] {
                let d = contrast(t.mode_fg, band);
                assert!(d > 0.35, "{name}/{label}: 帯の字が読めない（{d:.2}）");
            }
        }
    }

    /// 使い方の面が背景と**見分けられる**こと。明るいテーマで白へ寄せると
    /// 面が消える（実際に白磁でそうなった）。
    #[test]
    fn the_help_panel_is_a_distinct_surface_in_every_theme() {
        for (name, _) in BUILTIN {
            let t = builtin(name).expect("引けない");
            let d = contrast(t.help_bg, t.bg);
            assert!(d > 0.008, "{name}: 使い方の面が背景に溶けている（{d:.4}）");
            let text = contrast(t.help_body, t.help_bg);
            assert!(text > 0.55, "{name}: 使い方の字が読めない（{text:.2}）");
        }
    }

    #[test]
    fn fading_moves_toward_the_background_not_toward_black() {
        let light = builtin("白磁").expect("引けない");
        let faded = light.fade(light.fg, 0.5);
        assert!(
            luma(faded) > luma(light.fg),
            "明るいテーマで薄くしたのに濃くなっている"
        );
    }

    #[test]
    fn colors_are_read_in_the_forms_people_actually_write() {
        assert_eq!(parse_color("#000000"), Some([0.0, 0.0, 0.0, 1.0]));
        assert_eq!(parse_color("#fff"), Some([1.0, 1.0, 1.0, 1.0]));
        assert_eq!(parse_color("#ffffff80").map(|c| c[3]), Some(128.0 / 255.0));
        assert_eq!(parse_color("ffffff"), None, "# が無いのに通っている");
        assert_eq!(parse_color("#gggggg"), None);
        assert_eq!(parse_color("#12345"), None);
    }

    #[test]
    fn a_named_color_can_be_overridden_and_a_typo_is_reported() {
        let mut t = Theme::default();
        assert!(set_named(&mut t, "background", [1.0, 0.0, 0.0, 1.0]));
        assert_eq!(t.bg, [1.0, 0.0, 0.0, 1.0]);
        assert!(set_named(&mut t, "ansi5", [0.0, 1.0, 0.0, 1.0]));
        assert_eq!(t.ansi[5], [0.0, 1.0, 0.0, 1.0]);
        assert!(!set_named(&mut t, "ansi16", [0.0; 4]), "16 番は無い");
        assert!(!set_named(&mut t, "backgruond", [0.0; 4]), "綴り違いが通っている");
    }

    /// 16 以降のインデックス色はテーマで動かさない。アプリが
    /// `\x1b[38;5;208m` と書くとき期待しているのは決まった橙。
    #[test]
    fn the_extended_palette_is_the_same_in_every_theme() {
        let a = builtin("夜霧").expect("引けない");
        let b = builtin("白磁").expect("引けない");
        for i in [16u8, 100, 208, 240] {
            let c = tsg_term::Color::Indexed(i);
            assert_eq!(a.resolve(c), b.resolve(c), "拡張パレット {i} がテーマで動いた");
        }
        // 0-15 は逆に、テーマごとに違わないとテーマの意味が無い。
        assert_ne!(
            a.resolve(tsg_term::Color::Indexed(1)),
            b.resolve(tsg_term::Color::Indexed(1))
        );
    }

    fn luma(c: Rgba) -> f32 {
        0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
    }

    fn contrast(a: Rgba, b: Rgba) -> f32 {
        (luma(a) - luma(b)).abs()
    }
}
