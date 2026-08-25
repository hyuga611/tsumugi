//! 端末に絵を出す（Kitty graphics protocol）。
//!
//! **なぜ Kitty の形か。** いま絵を出す道具（`timg` `chafa` `matplotlib` の
//! kitty backend、`kitty +kitten icat`）が揃って話すのがこれで、
//! Sixel と違って**色数の制限が無く、転送が base64 の素の画素**で済む。
//!
//! **なぜ vte の外で拾うか。** この形は APC（`ESC _ G … ESC \`）で来るが、
//! `vte` は APC を捨てる。取り出す口が無いので、`feed` に入る前に
//! バイト列から抜き出して、残りだけを解析器へ渡す。
//!
//! **割り切り**: `a=T`（送って出す）だけを見る。画像の使い回し（`a=p`）・
//! 動画・Z 順・切り抜きは持たない。読むために要る分だけを実装した。

use base64::prelude::{BASE64_STANDARD, Engine as _};

/// 1 つの APC に許す長さ。**画面に出しただけで落ちない**ための上限。
///
/// 終端（`ESC \`）が来ない APC をそのまま溜めると、細工した出力を
/// `cat` するだけでメモリを食い尽くせる。攻撃と言うより、壊れた
/// 出力でも同じことが起きる。
const MAX_APC: usize = 8 * 1024 * 1024;
/// 1 枚の絵に許す画素数（4096x4096）。
const MAX_PIXELS: u32 = 4096 * 4096;
/// 組み立て中の絵に許す長さ。
const MAX_PAYLOAD: usize = 64 * 1024 * 1024;

/// 受け取った 1 枚。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    /// この端末の中で通し番号。ホストが載せた場所を覚えるのに使う。
    pub id: u64,
    /// 置いた場所（**ドキュメント絶対行**）。スクロールバックと一緒に動く。
    pub line: usize,
    pub col: usize,
    /// 何セルぶんを占めるか。
    pub cols: usize,
    pub rows: usize,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// 組み立て中の 1 枚。`m=1` で分割して届く。
#[derive(Default)]
pub struct Pending {
    format: u32,
    width: u32,
    height: u32,
    cols: u32,
    rows: u32,
    payload: Vec<u8>,
}

/// APC の中身を 1 つ食わせる（`G` の後ろから `ESC \` の手前まで）。
///
/// 返り値が `Some` なら 1 枚そろった。
pub fn feed_apc(
    pending: &mut Option<Pending>,
    body: &[u8],
) -> Option<(Vec<u8>, u32, u32, u32, u32)> {
    let text = String::from_utf8_lossy(body);
    let (keys, data) = match text.split_once(';') {
        Some((k, d)) => (k, d),
        None => (text.as_ref(), ""),
    };

    let mut more = false;
    let mut action = 'T';
    let p = pending.get_or_insert_with(Pending::default);
    for kv in keys.split(',') {
        let Some((k, v)) = kv.split_once('=') else {
            continue;
        };
        match k {
            "a" => action = v.chars().next().unwrap_or('T'),
            "f" => p.format = v.parse().unwrap_or(32),
            "s" => p.width = v.parse().unwrap_or(0),
            "v" => p.height = v.parse().unwrap_or(0),
            "c" => p.cols = v.parse().unwrap_or(0),
            "r" => p.rows = v.parse().unwrap_or(0),
            "m" => more = v != "0",
            _ => {}
        }
    }
    // 問い合わせ（`a=q`）や削除（`a=d`）は黙って捨てる。**知らない指示に
    // 対して勝手な絵を出さない。**
    if action != 'T' && action != 'a' {
        *pending = None;
        return None;
    }
    if let Ok(bytes) = BASE64_STANDARD.decode(data.trim()) {
        if p.payload.len() + bytes.len() > MAX_PAYLOAD {
            // 上限を超えたら捨てる。**途中まで出さない**（半分の絵は嘘）。
            *pending = None;
            return None;
        }
        p.payload.extend_from_slice(&bytes);
    }
    if more {
        return None;
    }
    let p = pending.take()?;
    let (rgba, w, h) = decode(&p)?;
    Some((rgba, w, h, p.cols, p.rows))
}

/// 画素の総数。**掛け算で溢れさせない。**
///
/// `w * h * 4` を `u32` で計算すると、release では小さな値へ折り返す。
/// 折り返した先が payload の長さを下回ると、不正な寸法のまま受理される。
fn pixel_bytes(w: u32, h: u32, per: u32) -> Option<usize> {
    let n = w.checked_mul(h)?;
    (n <= MAX_PIXELS).then_some(())?;
    n.checked_mul(per).map(|v| v as usize)
}

/// 画素にする。PNG（`f=100`）と素の RGB / RGBA（`f=24` / `f=32`）。
fn decode(p: &Pending) -> Option<(Vec<u8>, u32, u32)> {
    match p.format {
        100 => {
            let dec = png::Decoder::new(std::io::Cursor::new(&p.payload));
            let mut reader = dec.read_info().ok()?;
            let mut buf = vec![0u8; reader.output_buffer_size()?];
            let info = reader.next_frame(&mut buf).ok()?;
            pixel_bytes(info.width, info.height, 4)?;
            let rgba = match info.color_type {
                png::ColorType::Rgba => buf[..info.buffer_size()].to_vec(),
                png::ColorType::Rgb => buf[..info.buffer_size()]
                    .chunks_exact(3)
                    .flat_map(|c| [c[0], c[1], c[2], 255])
                    .collect(),
                png::ColorType::Grayscale => buf[..info.buffer_size()]
                    .iter()
                    .flat_map(|g| [*g, *g, *g, 255])
                    .collect(),
                png::ColorType::GrayscaleAlpha => buf[..info.buffer_size()]
                    .chunks_exact(2)
                    .flat_map(|c| [c[0], c[0], c[0], c[1]])
                    .collect(),
                // パレットは展開しないと出せない。**間違った色で出すより出さない。**
                png::ColorType::Indexed => return None,
            };
            Some((rgba, info.width, info.height))
        }
        24 => {
            let (w, h) = (p.width, p.height);
            let need = pixel_bytes(w, h, 3)?;
            (w > 0 && h > 0 && p.payload.len() >= need).then(|| {
                let rgba = p
                    .payload
                    .chunks_exact(3)
                    .flat_map(|c| [c[0], c[1], c[2], 255])
                    .collect();
                (rgba, w, h)
            })
        }
        _ => {
            let (w, h) = (p.width, p.height);
            let need = pixel_bytes(w, h, 4)?;
            (w > 0 && h > 0 && p.payload.len() >= need).then(|| (p.payload[..need].to_vec(), w, h))
        }
    }
}

/// バイト列から APC（`ESC _ … ESC \`）を抜き出す。
///
/// 返すのは「解析器へ渡す残り」と「取り出した中身」。**途中で切れていたら
/// 持ち越す**（PTY は都合のよい位置で切れてこない）。
#[derive(Default)]
pub struct ApcSplitter {
    /// 組み立て中の APC の中身。
    buf: Option<Vec<u8>>,
    /// 直前のバイトが ESC だったか（`ESC \` をまたいで見るため）。
    esc: bool,
}

impl ApcSplitter {
    /// `bytes` を通して、(解析器へ渡す分, 取り出した APC の中身たち) を返す。
    pub fn split(&mut self, bytes: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
        let mut rest = Vec::with_capacity(bytes.len());
        let mut found = Vec::new();
        for &b in bytes {
            if let Some(buf) = self.buf.as_mut() {
                // 中身を集めている最中。`ESC \` で終わり。
                if self.esc {
                    self.esc = false;
                    if b == b'\\' {
                        found.push(self.buf.take().expect("いま持っている"));
                        continue;
                    }
                    buf.push(0x1b);
                }
                if b == 0x1b {
                    self.esc = true;
                } else if buf.len() < MAX_APC {
                    buf.push(b);
                } else {
                    // 終端が来ない。**溜め続けない。** 捨てて素へ戻る。
                    self.buf = None;
                    self.esc = false;
                }
                continue;
            }
            if self.esc {
                self.esc = false;
                if b == b'_' {
                    self.buf = Some(Vec::new());
                    continue;
                }
                rest.push(0x1b);
                rest.push(b);
                continue;
            }
            if b == 0x1b {
                self.esc = true;
            } else {
                rest.push(b);
            }
        }
        // 末尾の ESC は次の呼びへ持ち越す（`ESC _` が分かれて届く）。
        (rest, found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_apc_is_taken_out_and_the_rest_goes_on() {
        let mut s = ApcSplitter::default();
        let (rest, found) = s.split(b"ab\x1b_Ghello\x1b\\cd");
        assert_eq!(rest, b"abcd");
        assert_eq!(found, vec![b"Ghello".to_vec()]);
    }

    /// PTY は都合のよい位置で切れてこない。**またいでも同じ結果**でなければ、
    /// 大きな絵は必ず壊れる。
    #[test]
    fn an_apc_split_across_reads_still_comes_out_whole() {
        let whole = b"x\x1b_Gabcdef\x1b\\y";
        for cut in 1..whole.len() {
            let mut s = ApcSplitter::default();
            let (mut rest, mut found) = s.split(&whole[..cut]);
            let (rest2, found2) = s.split(&whole[cut..]);
            rest.extend_from_slice(&rest2);
            found.extend(found2);
            assert_eq!(rest, b"xy", "cut={cut}");
            assert_eq!(found, vec![b"Gabcdef".to_vec()], "cut={cut}");
        }
    }

    /// ESC が来ても APC の始まりでなければ、そのまま解析器へ渡す。
    #[test]
    fn other_escapes_are_left_alone() {
        let mut s = ApcSplitter::default();
        let (rest, found) = s.split(b"\x1b[31mred\x1b[0m");
        assert_eq!(rest, b"\x1b[31mred\x1b[0m");
        assert!(found.is_empty());
    }

    #[test]
    fn a_raw_rgba_image_comes_through() {
        let mut pending = None;
        let px = [255u8, 0, 0, 255, 0, 255, 0, 255];
        let body = format!("f=32,s=2,v=1;{}", BASE64_STANDARD.encode(px));
        let got = feed_apc(&mut pending, body.as_bytes()).expect("そろわなかった");
        assert_eq!(got.1, 2);
        assert_eq!(got.2, 1);
        assert_eq!(got.0, px);
    }

    /// 分けて届いても 1 枚にまとまる（`m=1`）。
    #[test]
    fn a_chunked_image_is_assembled() {
        let mut pending = None;
        let px = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let a = BASE64_STANDARD.encode(&px[..4]);
        let b = BASE64_STANDARD.encode(&px[4..]);
        assert!(feed_apc(&mut pending, format!("f=32,s=2,v=1,m=1;{a}").as_bytes()).is_none());
        let got = feed_apc(&mut pending, format!("m=0;{b}").as_bytes()).expect("そろわなかった");
        assert_eq!(got.0, px);
    }

    /// 知らない指示で勝手な絵を出さない。
    /// **画面に出しただけで落ちない。** 終端の来ない APC を溜め続けると、
    /// 細工した出力を `cat` するだけでメモリを食い尽くせる。
    #[test]
    fn an_unterminated_apc_does_not_grow_without_bound() {
        let mut s = ApcSplitter::default();
        s.split(b"_G");
        for _ in 0..20 {
            s.split(&vec![b'x'; 1024 * 1024]);
        }
        // 上限で捨てて素へ戻る。次の素のバイトはちゃんと通る。
        let (rest, _) = s.split(b"hello");
        assert_eq!(rest, b"hello", "捨てたあと素へ戻っていない");
    }

    /// 寸法の掛け算で溢れさせない。release では小さな値へ折り返し、
    /// 不正な寸法のまま受理されてしまう。
    #[test]
    fn absurd_dimensions_are_refused() {
        let mut pending = None;
        let body = format!("f=32,s=65536,v=65536;{}", BASE64_STANDARD.encode([0u8; 16]));
        assert!(feed_apc(&mut pending, body.as_bytes()).is_none());
    }

    #[test]
    fn a_query_does_not_produce_an_image() {
        let mut pending = None;
        assert!(feed_apc(&mut pending, b"a=q,f=32,s=1,v=1;AAAA").is_none());
        assert!(pending.is_none());
    }
}
