//! Sixel（DCS `q`）。古い道具が絵を出すときの形。
//!
//! Kitty graphics に対応していても、`gnuplot` `img2sixel` `lsix` のように
//! **Sixel しか話さない道具**がある。数十行で読めるので、持っておく。
//!
//! # 形
//!
//! 1 バイトが**縦 6 画素**を表す（`?` = 0x3F を 0 として、下位 6 ビットが
//! 上から下の 6 画素）。横へ進みながら 6 画素ずつ描き、`-` で次の 6 画素へ、
//! `$` で行頭へ戻る（同じ帯を別の色で重ね塗りする）。`#n;2;r;g;b` で色を定義し、
//! `#n` で色を選ぶ。`!123` は直後の 1 バイトを 123 回繰り返す。
//!
//! **割り切り**: マクロ（`DECGCI`）と、縦横比の指定（`"` の引数）は見ない。
//! 出す絵の形が変わるものではないため。

/// 1 枚に許す大きさ。**画面に字を出しただけで落ちない**ための上限。
const MAX_W: usize = 4096;
const MAX_H: usize = 4096;

/// 読み取り中の状態。
#[derive(Default)]
pub struct Decoder {
    /// 色表。既定は VT340 の 16 色に近いもの。
    palette: Vec<(u8, u8, u8)>,
    /// 画素（RGBA）。行ごとに必要な分だけ伸ばす。
    px: Vec<u8>,
    width: usize,
    height: usize,
    /// いま書いている位置。
    x: usize,
    /// いまの帯の上端（6 画素ごと）。
    band: usize,
    color: usize,
    /// `!` の繰り返し数を読んでいる最中。
    repeat: Option<usize>,
    /// `#` や `"` の引数を読んでいる最中。
    params: Vec<usize>,
    mode: Mode,
    /// 大きすぎて諦めた。
    gave_up: bool,
}

#[derive(Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Data,
    Color,
    Raster,
}

/// VT340 の既定色（近似）。定義されない色番号が来ても黒一色にしない。
fn default_palette() -> Vec<(u8, u8, u8)> {
    vec![
        (0, 0, 0),
        (51, 51, 204),
        (204, 36, 36),
        (51, 204, 51),
        (204, 51, 204),
        (51, 204, 204),
        (204, 204, 51),
        (135, 135, 135),
        (66, 66, 66),
        (84, 84, 153),
        (153, 66, 66),
        (84, 153, 84),
        (153, 84, 153),
        (84, 153, 153),
        (153, 153, 84),
        (204, 204, 204),
    ]
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            palette: default_palette(),
            ..Default::default()
        }
    }

    /// 1 バイト食わせる。
    pub fn put(&mut self, b: u8) {
        if self.gave_up {
            return;
        }
        match self.mode {
            Mode::Color | Mode::Raster => {
                match b {
                    b'0'..=b'9' => {
                        let last = self.params.last_mut().expect("引数は 1 つ以上ある");
                        *last = last.saturating_mul(10) + usize::from(b - b'0');
                        return;
                    }
                    b';' => {
                        self.params.push(0);
                        return;
                    }
                    _ => {
                        self.finish_params();
                        // 区切りの次は、そのまま本文として読み直す
                    }
                }
            }
            Mode::Data => {}
        }

        match b {
            b'#' => {
                self.mode = Mode::Color;
                self.params = vec![0];
            }
            b'"' => {
                self.mode = Mode::Raster;
                self.params = vec![0];
            }
            b'!' => self.repeat = Some(0),
            b'0'..=b'9' if self.repeat.is_some() => {
                let n = self.repeat.get_or_insert(0);
                *n = (*n).saturating_mul(10) + usize::from(b - b'0');
            }
            b'$' => {
                self.x = 0;
                self.repeat = None;
            }
            b'-' => {
                self.x = 0;
                self.band += 6;
                self.repeat = None;
            }
            0x3f..=0x7e => {
                let bits = b - 0x3f;
                let n = self.repeat.take().unwrap_or(1).clamp(1, MAX_W);
                for _ in 0..n {
                    self.plot(bits);
                }
            }
            _ => {}
        }
    }

    fn finish_params(&mut self) {
        let p = std::mem::take(&mut self.params);
        match std::mem::take(&mut self.mode) {
            Mode::Color => {
                let n = p.first().copied().unwrap_or(0);
                // `#n;2;r;g;b` は 0〜100 で来る。`#n` だけなら色を選ぶだけ。
                if p.len() >= 5 && p[1] == 2 {
                    let to = |v: usize| ((v.min(100) * 255) / 100) as u8;
                    let rgb = (to(p[2]), to(p[3]), to(p[4]));
                    if n < 256 {
                        if self.palette.len() <= n {
                            self.palette.resize(n + 1, (0, 0, 0));
                        }
                        self.palette[n] = rgb;
                    }
                }
                self.color = n;
            }
            Mode::Raster => {
                // `"pan;pad;width;height`。大きさが分かるなら先に確保する。
                if p.len() >= 4 {
                    let (w, h) = (p[2], p[3]);
                    if w > MAX_W || h > MAX_H {
                        self.gave_up = true;
                        return;
                    }
                    self.reserve(w, h);
                }
            }
            Mode::Data => {}
        }
    }

    fn reserve(&mut self, w: usize, h: usize) {
        if w > MAX_W || h > MAX_H {
            self.gave_up = true;
            return;
        }
        if w <= self.width && h <= self.height {
            return;
        }
        let (nw, nh) = (self.width.max(w), self.height.max(h));
        let mut next = vec![0u8; nw * nh * 4];
        for y in 0..self.height {
            let from = y * self.width * 4;
            let to = y * nw * 4;
            next[to..to + self.width * 4].copy_from_slice(&self.px[from..from + self.width * 4]);
        }
        self.px = next;
        self.width = nw;
        self.height = nh;
    }

    fn plot(&mut self, bits: u8) {
        let (x, band) = (self.x, self.band);
        self.x += 1;
        if x >= MAX_W || band + 6 > MAX_H {
            self.gave_up = true;
            return;
        }
        if x >= self.width || band + 6 > self.height {
            // 少し多めに伸ばす。1 画素ずつ伸ばすと確保しなおしが増える。
            let w = (x + 1).max(self.width).next_power_of_two().min(MAX_W);
            let h = (band + 6).max(self.height).next_power_of_two().min(MAX_H);
            self.reserve(w, h);
            if self.gave_up {
                return;
            }
        }
        let (r, g, b) = self
            .palette
            .get(self.color)
            .copied()
            .unwrap_or((255, 255, 255));
        for row in 0..6 {
            if bits & (1 << row) == 0 {
                continue;
            }
            let y = band + row;
            let i = (y * self.width + x) * 4;
            if i + 4 <= self.px.len() {
                self.px[i] = r;
                self.px[i + 1] = g;
                self.px[i + 2] = b;
                self.px[i + 3] = 255;
            }
        }
    }

    /// 終わり。画素と大きさを返す。何も描かれていなければ `None`。
    pub fn finish(mut self) -> Option<(Vec<u8>, u32, u32)> {
        if self.gave_up || self.width == 0 || self.height == 0 {
            return None;
        }
        // 実際に描いた高さまで切り詰める（帯の切りのいいところまで伸びている）。
        let used_h = (self.band + 6).min(self.height);
        self.px.truncate(self.width * used_h * 4);
        Some((self.px, self.width as u32, used_h as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(s: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
        let mut d = Decoder::new();
        for b in s {
            d.put(*b);
        }
        d.finish()
    }

    /// 1 バイトが縦 6 画素。`@` (0x40) は最上段だけ。
    #[test]
    fn one_byte_is_six_pixels_tall() {
        let (px, w, h) = decode(b"#1;2;100;0;0@").expect("読めない");
        assert_eq!((w, h), (1, 6));
        assert_eq!(&px[0..4], &[255, 0, 0, 255], "最上段が塗られていない");
        assert_eq!(px[4 * w as usize + 3], 0, "2 段目まで塗ってしまった");
    }

    /// `!n` は直後の 1 バイトを n 回。
    #[test]
    fn a_repeat_count_repeats_the_next_byte() {
        let (_, w, _) = decode(b"#1;2;0;100;0!5@").expect("読めない");
        assert!(w >= 5, "繰り返しが効いていない: {w}");
    }

    /// `-` で次の帯へ。
    #[test]
    fn a_dash_moves_to_the_next_band() {
        let (_, _, h) = decode(b"#1;2;0;0;100@-@").expect("読めない");
        assert!(h >= 12, "帯が進んでいない: {h}");
    }

    /// **画面に字を出しただけで落ちない。** 大きすぎる指定は諦める。
    #[test]
    fn an_absurd_size_is_refused() {
        assert!(decode(b"\"1;1;99999;99999@").is_none());
    }
}
