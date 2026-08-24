//! フォント探索とグリフのラスタライズ。
//!
//! `arch.md` §3 の選定通り fontdb（探索）+ swash（ラスタライズ）+
//! rustybuzz（シェーピング）。
//!
//! フォールバックチェーンを自前で持つのが要点。CJK と絵文字は
//! 等幅欧文フォントに入っていないため、**セル単位でフォントを切り替える**。

use anyhow::{Context, Result};
use fontdb::{Database, Family, Query, Stretch, Style, Weight};
use swash::scale::{Render, ScaleContext, Source as ScaleSource};
use swash::zeno::Format;
use swash::{FontRef, GlyphId};

/// 実体を所有したフォント1本。
pub struct FontData {
    data: Vec<u8>,
    index: u32,
    pub family: String,
    /// このフォントを描くときの寸法倍率。
    ///
    /// 欧文等幅フォントと CJK フォントは em に対する送り幅の設計が違う
    /// （例: Consolas の半角は 0.55em、MS Gothic の全角は 1.0em）。
    /// 素直に同じ px で描くと全角が 2 セルに収まらない。
    /// **セル格子を正とし、フォールバック側を格子へ合わせて伸縮させる。**
    pub scale: f32,
}

impl FontData {
    fn font_ref(&self) -> Option<FontRef<'_>> {
        FontRef::from_index(&self.data, self.index as usize)
    }

    /// シェーピング用の顔。合字（`liga` / `calt`）はここが持つ。
    pub fn shaper(&self) -> Option<rustybuzz::Face<'_>> {
        rustybuzz::Face::from_slice(&self.data, self.index)
    }
}

/// ラスタライズ済みグリフ。
pub struct RasterizedGlyph {
    pub width: u32,
    pub height: u32,
    /// 描画原点からの左オフセット（px）
    pub left: i32,
    /// ベースラインからの上オフセット（px）
    pub top: i32,
    /// アルファ値（width * height バイト）
    pub data: Vec<u8>,
}

/// swash の `ScaleContext` を持つ側。`FontStack` と分けているのは、
/// `FontRef` が `FontData` を借りるため、同じ構造体に置くと借用が衝突するから。
pub struct Rasterizer {
    ctx: ScaleContext,
}

impl Default for Rasterizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Rasterizer {
    pub fn new() -> Self {
        Self {
            ctx: ScaleContext::new(),
        }
    }

    pub fn render(&mut self, font: &FontData, px: f32, gid: GlyphId) -> Option<RasterizedGlyph> {
        let font_ref = font.font_ref()?;
        let mut scaler = self.ctx.builder(font_ref).size(px).hint(true).build();
        let image = Render::new(&[ScaleSource::Outline, ScaleSource::Bitmap(
            swash::scale::StrikeWith::BestFit,
        )])
        .format(Format::Alpha)
        .render(&mut scaler, gid)?;

        Some(RasterizedGlyph {
            width: image.placement.width,
            height: image.placement.height,
            left: image.placement.left,
            top: image.placement.top,
            data: image.data,
        })
    }
}

/// 等幅の基準フォント + フォールバック群。
pub struct FontStack {
    /// `[0]` が基準（セル寸法の決定元）。以降がフォールバック。
    pub fonts: Vec<FontData>,
    pub px: f32,
    /// セル幅（基準フォントの送り幅）
    pub cell_w: f32,
    /// 行の高さ
    pub cell_h: f32,
    /// セル上端からベースラインまで
    pub ascent: f32,
}

/// プラットフォーム別の候補。前から順に、実在した最初のものを採る。
fn candidates() -> (&'static [&'static str], &'static [&'static str], &'static [&'static str]) {
    #[cfg(target_os = "windows")]
    {
        (
            &["Cascadia Mono", "Consolas", "Courier New"],
            &["MS Gothic", "Yu Gothic UI", "Meiryo", "Yu Gothic", "MS Mincho"],
            &["Segoe UI Emoji"],
        )
    }
    #[cfg(target_os = "macos")]
    {
        (
            &["SF Mono", "Menlo", "Monaco"],
            &["Hiragino Sans", "Hiragino Kaku Gothic ProN"],
            &["Apple Color Emoji"],
        )
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        (
            &["DejaVu Sans Mono", "Noto Sans Mono", "Liberation Mono"],
            &["Noto Sans CJK JP", "Noto Sans JP", "Source Han Sans JP"],
            &["Noto Color Emoji"],
        )
    }
}

fn find(db: &Database, names: &[&str]) -> Option<fontdb::ID> {
    for name in names {
        let query = Query {
            families: &[Family::Name(name)],
            weight: Weight::NORMAL,
            stretch: Stretch::Normal,
            style: Style::Normal,
        };
        if let Some(id) = db.query(&query) {
            // fontdb は家族名が無くても近縁を返すことがあるので、実際の家族名を確認する。
            let matched = db
                .face(id)
                .map(|f| f.families.iter().any(|(fam, _)| fam.eq_ignore_ascii_case(name)))
                .unwrap_or(false);
            if matched {
                return Some(id);
            }
        }
    }
    None
}

fn load(db: &Database, id: fontdb::ID) -> Option<FontData> {
    let family = db
        .face(id)
        .and_then(|f| f.families.first().map(|(n, _)| n.clone()))
        .unwrap_or_else(|| "?".to_string());
    db.with_face_data(id, |data, index| FontData {
        data: data.to_vec(),
        index,
        family: family.clone(),
        scale: 1.0,
    })
}

/// フォールバックフォントの寸法をセル格子へ合わせる倍率を求める。
///
/// そのフォントが持つ「全角であるべき文字」を1つ測り、`2 * cell_w` になるよう伸縮する。
/// 実測（M0-b）: Consolas 9.9px に対し MS Gothic の全角が 18.0px で、
/// そのままでは 1.80 セルにしかならなかった。
fn fit_scale(font: &FontData, px: f32, cell_w: f32) -> f32 {
    const PROBES: [char; 6] = ['日', '一', 'あ', '漢', '\u{1F415}', '\u{263A}'];
    let Some(fr) = font.font_ref() else {
        return 1.0;
    };
    let charmap = fr.charmap();
    let gm = fr.glyph_metrics(&[]).scale(px);
    for c in PROBES {
        let gid = charmap.map(c);
        if gid == 0 {
            continue;
        }
        let advance = gm.advance_width(gid);
        if advance > 0.1 {
            return (cell_w * 2.0) / advance;
        }
    }
    1.0
}

impl FontStack {
    /// システムフォントから等幅 + CJK + 絵文字のチェーンを組む。
    pub fn discover(px: f32) -> Result<Self> {
        let mut db = Database::new();
        db.load_system_fonts();

        let (mono, cjk, emoji) = candidates();

        let mono_id = find(&db, mono)
            .or_else(|| {
                db.query(&Query {
                    families: &[Family::Monospace],
                    ..Default::default()
                })
            })
            .context("等幅フォントが1つも見つかりません")?;

        let mut fonts = vec![load(&db, mono_id).context("基準フォントの読み込みに失敗")?];
        for group in [cjk, emoji] {
            if let Some(id) = find(&db, group)
                && let Some(f) = load(&db, id)
            {
                fonts.push(f);
            }
        }

        let (cell_w, cell_h, ascent) = {
            let base = fonts[0].font_ref().context("基準フォントを解釈できません")?;
            let metrics = base.metrics(&[]).scale(px);
            let gm = base.glyph_metrics(&[]).scale(px);
            let gid = base.charmap().map('M');
            let advance = gm.advance_width(gid);
            let w = if advance > 0.0 { advance } else { px * 0.6 };
            let h = metrics.ascent + metrics.descent + metrics.leading;
            (w.ceil(), h.ceil().max(1.0), metrics.ascent)
        };

        // 基準フォント以外を、セル格子に合うよう伸縮させる。
        for i in 1..fonts.len() {
            fonts[i].scale = fit_scale(&fonts[i], px, cell_w);
        }

        Ok(Self {
            fonts,
            px,
            cell_w,
            cell_h,
            ascent,
        })
    }

    /// 大きさだけ取り直す（Ctrl＋ホイール）。
    ///
    /// `discover` をやり直さないのは、システムフォントの走査が重く、
    /// ホイールの 1 目盛りごとに走らせると目に見えて引っかかるため。
    /// 読み込んだ字体はそのままで、**セル寸法と伸縮率だけ**を計算し直す。
    pub fn rescale(&mut self, px: f32) -> bool {
        if px <= 0.0 || (px - self.px).abs() < 0.05 {
            return false;
        }
        let Some(base) = self.fonts.first().and_then(FontData::font_ref) else {
            return false;
        };
        let metrics = base.metrics(&[]).scale(px);
        let gm = base.glyph_metrics(&[]).scale(px);
        let advance = gm.advance_width(base.charmap().map('M'));
        let cell_w = if advance > 0.0 { advance } else { px * 0.6 }.ceil();
        let cell_h = (metrics.ascent + metrics.descent + metrics.leading)
            .ceil()
            .max(1.0);
        let ascent = metrics.ascent;

        self.px = px;
        self.cell_w = cell_w;
        self.cell_h = cell_h;
        self.ascent = ascent;
        for i in 1..self.fonts.len() {
            self.fonts[i].scale = fit_scale(&self.fonts[i], px, cell_w);
        }
        true
    }

    /// そのフォントを実際にラスタライズするときの px。
    pub fn px_for(&self, font_idx: usize) -> f32 {
        self.px * self.fonts.get(font_idx).map_or(1.0, |f| f.scale)
    }

    /// その文字を持つ最初のフォントと、そのグリフ ID。
    pub fn glyph_for(&self, c: char) -> Option<(usize, GlyphId)> {
        for (i, f) in self.fonts.iter().enumerate() {
            if let Some(fr) = f.font_ref() {
                let gid = fr.charmap().map(c);
                if gid != 0 {
                    return Some((i, gid));
                }
            }
        }
        None
    }

    /// その文字の送り幅（px）。伸縮を反映した実際の値を返す。CJK 幅の検証に使う。
    pub fn advance_of(&self, c: char) -> Option<f32> {
        let (idx, gid) = self.glyph_for(c)?;
        let fr = self.fonts[idx].font_ref()?;
        Some(
            fr.glyph_metrics(&[])
                .scale(self.px_for(idx))
                .advance_width(gid),
        )
    }

    /// **1 セル 1 文字**の並びを基準フォントで組み、(グリフ, 何セル目) を返す。
    ///
    /// 端末は格子なので、シェーピングが返す送り幅をそのまま使ってはいけない。
    /// 合字の字形は入力の何文字ぶんかを 1 つの**クラスタ**として返すので、
    /// クラスタの先頭が何セル目かを見て置く。等幅フォントの合字はその字数ぶんの
    /// 幅に設計されているので、これで格子と合う。
    ///
    /// `text` に全角や結合文字が混ざっていると格子と対応が取れないので、
    /// 呼ぶ側が 1 文字 1 セルであることを保証する。保証できなければ `None`。
    pub fn shape_cells(&self, text: &str) -> Option<Vec<(u16, u16)>> {
        let face = self.fonts.first()?.shaper()?;

        let mut buf = rustybuzz::UnicodeBuffer::new();
        buf.push_str(text);
        buf.set_direction(rustybuzz::Direction::LeftToRight);
        let out = rustybuzz::shape(&face, &[], buf);

        // クラスタはバイト位置で返る。バイト位置 → 何文字目（＝何セル目）の対応表。
        let mut cell_of = vec![u16::MAX; text.len() + 1];
        for (i, (byte, _)) in text.char_indices().enumerate() {
            cell_of[byte] = u16::try_from(i).ok()?;
        }

        let mut placed = Vec::with_capacity(out.len());
        for info in out.glyph_infos() {
            // クラスタが文字の途中を指したら、格子との対応が壊れている。
            // 当てずっぽうに置かず、呼ぶ側に 1 文字ずつ描かせる。
            let cell = *cell_of.get(info.cluster as usize)?;
            if cell == u16::MAX {
                return None;
            }
            let gid = u16::try_from(info.glyph_id).ok()?;
            if gid != 0 {
                placed.push((gid, cell));
            }
        }
        Some(placed)
    }

    /// 各フォントに適用した伸縮倍率。診断用。
    pub fn scales(&self) -> Vec<f32> {
        self.fonts.iter().map(|f| f.scale).collect()
    }

    pub fn families(&self) -> Vec<&str> {
        self.fonts.iter().map(|f| f.family.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 組んだ結果が**格子と 1 対 1 で対応する**こと。
    ///
    /// 合字を持つフォントかどうかは環境次第なので、ここで見るのは
    /// 「どのグリフも、実在するセルに、左から順に置かれる」という不変条件。
    /// ここが崩れると字が重なる・行が右へずれる。
    #[test]
    fn shaping_places_every_glyph_on_a_real_cell() {
        let Ok(stack) = FontStack::discover(18.0) else {
            eprintln!("フォントが見つからないためスキップ");
            return;
        };
        for text in ["->", "===", "!=", "a -> b", "x", "|||>", "fn main() {"] {
            let cells = text.chars().count();
            let Some(placed) = stack.shape_cells(text) else {
                panic!("{text:?} を組めない");
            };
            assert!(!placed.is_empty(), "{text:?} で何も出ない");
            assert!(
                placed.len() <= cells,
                "{text:?}: グリフがセル数より多い（{} > {cells}）",
                placed.len()
            );
            let mut last = None;
            for (_, cell) in &placed {
                assert!(
                    (*cell as usize) < cells,
                    "{text:?}: 実在しないセル {cell} に置いた"
                );
                if let Some(prev) = last {
                    assert!(*cell > prev, "{text:?}: 左から順になっていない");
                }
                last = Some(*cell);
            }
        }
    }

    /// 空の入力で落ちない（行末で必ず通る道）。
    #[test]
    fn shaping_an_empty_run_is_not_an_error() {
        let Ok(stack) = FontStack::discover(18.0) else {
            return;
        };
        assert_eq!(stack.shape_cells(""), Some(Vec::new()));
    }

    /// M0-b が実際に踏んだ不整合の回帰テスト。
    ///
    /// Consolas の半角送り幅（0.55em）と MS Gothic の全角（1.0em）は一致しないため、
    /// 伸縮を入れないと全角が 1.80 セルにしかならない。
    /// システムフォントに依存するので、見つからない環境ではスキップする。
    #[test]
    fn fallback_fonts_fit_the_cell_grid() {
        let Ok(stack) = FontStack::discover(18.0) else {
            eprintln!("フォントが見つからないためスキップ");
            return;
        };
        if stack.fonts.len() < 2 {
            eprintln!("フォールバックフォントが無いためスキップ");
            return;
        }
        let Some(advance) = stack.advance_of('日') else {
            eprintln!("全角グリフが無いためスキップ");
            return;
        };
        let ratio = advance / stack.cell_w;
        assert!(
            (ratio - 2.0).abs() < 0.15,
            "全角がセル格子 2 セルに収まっていない: {ratio:.2} セル ({:?})",
            stack.families()
        );
    }
}
