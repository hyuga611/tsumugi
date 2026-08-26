//! wgpu によるセル描画。
//!
//! `arch.md` の依存の向きに従い、このクレートは winit を知らない。
//! ウィンドウは `HasWindowHandle + HasDisplayHandle` として受け取る。
//!
//! 合字（リガチャ）はここが持つ。端末は**格子**なので、シェーピングの結果を
//! 素直に並べるのではなく「どのセルに置くか」へ翻訳する必要がある。詳しくは
//! `shape_run`。

pub mod font;

use std::collections::HashMap;

use anyhow::{Context, Result};
use bytemuck::{Pod, Zeroable};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};

pub use font::{FontStack, RasterizedGlyph, Rasterizer};

const ATLAS_SIZE: u32 = 2048;

/// シェーピング結果を控える行数の上限。行の中身は無限にあり得るので、
/// 際限なく溜めない。溢れたら丸ごと捨てて組み直す（1 フレームぶんの取り直し）。
const SHAPE_CACHE_MAX: usize = 4096;

/// 合字になり得る字を含むか。
///
/// **含まないほうが圧倒的に多い。** 日本語の文章にも `ls` の出力にも合字は
/// 出ないので、そこでシェーピングを走らせるのは丸損になる。
/// ここで弾いて、素通しの道を軽いままにしておく。
fn may_ligate(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(
            c,
            '=' | '-'
                | '<'
                | '>'
                | '+'
                | '*'
                | '/'
                | '!'
                | '&'
                | '|'
                | ':'
                | '.'
                | '~'
                | '?'
                | '#'
                | '$'
                | '%'
                | '^'
                | ';'
                | '_'
                | '\\'
        )
    })
}

#[cfg(test)]
mod tests {
    use super::may_ligate;

    /// 合字を含まない行でシェーピングを走らせない。**そちらが圧倒的に多い。**
    #[test]
    fn ordinary_text_takes_the_cheap_path() {
        assert!(!may_ligate("hello world"), "ただの語で組みに行っている");
        assert!(!may_ligate("日本語のテキスト"));
        assert!(!may_ligate("total 134"));
        assert!(!may_ligate(""));
    }

    #[test]
    fn code_punctuation_goes_through_the_shaper() {
        for s in ["->", "!=", "=>", "|>", "a := b", "// note", "<$>"] {
            assert!(may_ligate(s), "{s:?} を素通ししている");
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    screen: [f32; 2],
    _pad: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Instance {
    pos: [f32; 2],
    size: [f32; 2],
    uv0: [f32; 2],
    uv1: [f32; 2],
    color: [f32; 4],
    /// 0 = アトラスのグリフ、1 = 単色矩形、2 = 画像
    mode: u32,
    _pad: [u32; 3],
}

/// アトラス上のグリフの位置と、描画時のオフセット。
#[derive(Clone, Copy)]
struct AtlasEntry {
    uv0: [f32; 2],
    uv1: [f32; 2],
    width: f32,
    height: f32,
    left: f32,
    top: f32,
}

/// 単純な棚（shelf）割り付けのグリフアトラス。
/// M0-b では解放しない（1セッションで枯れるほど字種は出ない）。
struct Atlas {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    x: u32,
    y: u32,
    row_h: u32,
    full: bool,
}

impl Atlas {
    /// 棚を先頭へ戻す。字の大きさを変えたときに使う。
    ///
    /// 中身は消さない（消す必要が無い）。**呼ぶ側がグリフのキャッシュも
    /// 空にすること**が条件で、そうしないと古い場所を指したまま上書きされる。
    fn reset(&mut self) {
        self.x = 0;
        self.y = 0;
        self.row_h = 0;
        self.full = false;
    }

    fn new(device: &wgpu::Device) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("tsg glyph atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture,
            view,
            x: 0,
            y: 0,
            row_h: 0,
            full: false,
        }
    }

    fn alloc(&mut self, queue: &wgpu::Queue, g: &RasterizedGlyph) -> Option<([f32; 2], [f32; 2])> {
        if g.width == 0 || g.height == 0 || self.full {
            return None;
        }
        if self.x + g.width > ATLAS_SIZE {
            self.x = 0;
            self.y += self.row_h + 1;
            self.row_h = 0;
        }
        if self.y + g.height > ATLAS_SIZE {
            self.full = true;
            return None;
        }

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: self.x,
                    y: self.y,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            &g.data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(g.width),
                rows_per_image: Some(g.height),
            },
            wgpu::Extent3d {
                width: g.width,
                height: g.height,
                depth_or_array_layers: 1,
            },
        );

        let uv0 = [
            self.x as f32 / ATLAS_SIZE as f32,
            self.y as f32 / ATLAS_SIZE as f32,
        ];
        let uv1 = [
            (self.x + g.width) as f32 / ATLAS_SIZE as f32,
            (self.y + g.height) as f32 / ATLAS_SIZE as f32,
        ];

        self.x += g.width + 1;
        self.row_h = self.row_h.max(g.height);
        Some((uv0, uv1))
    }
}

/// 画像の置き場所。**グリフとは別の面**に持つ。
///
/// グリフのアトラスは 1 チャンネル（濃さだけ）で、色は頂点から渡している。
/// 画像は色そのものを持つので同じ面には載らない。棚に横へ並べ、
/// 入らなくなったら**古いものから捨てる**（端末に出た絵は流れて消えるもの）。
struct ImageStore {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    x: u32,
    y: u32,
    row_h: u32,
    /// 積んだ順。溢れたら先頭から無効になる。
    generation: u32,
}

/// 置いた画像の場所（アトラス上の uv と、元の大きさ）。
#[derive(Clone, Copy, Debug)]
pub struct ImageSlot {
    uv0: [f32; 2],
    uv1: [f32; 2],
    pub width: u32,
    pub height: u32,
    generation: u32,
}

const IMAGE_ATLAS: u32 = 2048;

impl ImageStore {
    fn new(device: &wgpu::Device) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("tsg images"),
            size: wgpu::Extent3d {
                width: IMAGE_ATLAS,
                height: IMAGE_ATLAS,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture,
            view,
            x: 0,
            y: 0,
            row_h: 0,
            generation: 0,
        }
    }

    fn alloc(&mut self, queue: &wgpu::Queue, rgba: &[u8], w: u32, h: u32) -> Option<ImageSlot> {
        if w == 0 || h == 0 || w > IMAGE_ATLAS || h > IMAGE_ATLAS {
            return None;
        }
        if rgba.len() < (w as usize) * (h as usize) * 4 {
            return None;
        }
        if self.x + w > IMAGE_ATLAS {
            self.x = 0;
            self.y += self.row_h + 1;
            self.row_h = 0;
        }
        if self.y + h > IMAGE_ATLAS {
            // 一周した。**古いものは無効になる**（世代で見分ける）。
            self.x = 0;
            self.y = 0;
            self.row_h = 0;
            self.generation += 1;
        }
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: self.x,
                    y: self.y,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        let slot = ImageSlot {
            uv0: [
                self.x as f32 / IMAGE_ATLAS as f32,
                self.y as f32 / IMAGE_ATLAS as f32,
            ],
            uv1: [
                (self.x + w) as f32 / IMAGE_ATLAS as f32,
                (self.y + h) as f32 / IMAGE_ATLAS as f32,
            ],
            width: w,
            height: h,
            generation: self.generation,
        };
        self.x += w + 1;
        self.row_h = self.row_h.max(h);
        Some(slot)
    }
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,

    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    uniform_buf: wgpu::Buffer,
    instance_buf: wgpu::Buffer,
    instance_cap: usize,

    atlas: Atlas,
    images: ImageStore,
    cache: HashMap<(usize, u16, u16), Option<AtlasEntry>>,
    /// East Asian Ambiguous を 2 セルで数えるか（端末側の設定と揃える）。
    amb_wide: bool,
    /// 合字を組むか。切ると `glyph_run` は 1 セル 1 文字で積む。
    ligatures: bool,
    /// シェーピングの結果。**同じ行を毎フレーム組み直さない**ための控え。
    shaped: HashMap<Box<str>, Box<[(u16, u16)]>>,

    instances: Vec<Instance>,
    /// 合成面がプリマルチプライドか。クリア色の出し方が変わる。
    premultiplied: bool,
    pub fonts: FontStack,
    raster: Rasterizer,
    pub background: [f32; 4],
}

impl Renderer {
    pub fn new<W>(
        window: W,
        width: u32,
        height: u32,
        font_px: f32,
        transparent: bool,
        family: Option<&str>,
    ) -> Result<Self>
    where
        W: HasWindowHandle + HasDisplayHandle + Send + Sync + 'static,
    {
        let fonts = FontStack::discover_with(font_px, family)?;

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            flags: wgpu::InstanceFlags::from_build_config(),
            memory_budget_thresholds: Default::default(),
            backend_options: Default::default(),
            display: None,
        });
        let surface = instance
            .create_surface(window)
            .context("wgpu サーフェスの作成に失敗")?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .context("GPU アダプタが見つかりません")?;

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("tsg device"),
            ..Default::default()
        }))
        .context("GPU デバイスの取得に失敗")?;

        let caps = surface.get_capabilities(&adapter);
        let mut config = surface
            .get_default_config(&adapter, width.max(1), height.max(1))
            .context("サーフェスの既定設定を取得できません")?;
        // sRGB のフォーマットを優先する（アンチエイリアスの見えが素直になる）。
        if let Some(srgb) = caps.formats.iter().copied().find(|f| f.is_srgb()) {
            config.format = srgb;
        }
        config.present_mode = wgpu::PresentMode::AutoVsync;
        // 透過を要求されたときだけアルファ合成の面を選ぶ。
        // 不透明でよいなら選ばない（合成の手間をただ払うことになる）。
        if transparent {
            for mode in [
                wgpu::CompositeAlphaMode::PreMultiplied,
                wgpu::CompositeAlphaMode::PostMultiplied,
            ] {
                if caps.alpha_modes.contains(&mode) {
                    config.alpha_mode = mode;
                    break;
                }
            }
        }
        let format = config.format;
        let premultiplied = config.alpha_mode == wgpu::CompositeAlphaMode::PreMultiplied;
        surface.configure(&device, &config);

        let atlas = Atlas::new(&device);
        let images = ImageStore::new(&device);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tsg cell shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tsg uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("tsg atlas sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tsg bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tsg bind group"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&atlas.view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&images.view),
                },
            ],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tsg layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tsg pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Instance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x2,  // pos
                        1 => Float32x2,  // size
                        2 => Float32x2,  // uv0
                        3 => Float32x2,  // uv1
                        4 => Float32x4,  // color
                        5 => Uint32      // mode
                    ],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let instance_cap = 8192;
        let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tsg instances"),
            size: (instance_cap * std::mem::size_of::<Instance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            bind_group,
            uniform_buf,
            instance_buf,
            instance_cap,
            atlas,
            images,
            cache: HashMap::new(),
            ligatures: true,
            shaped: HashMap::new(),
            amb_wide: false,
            premultiplied,
            instances: Vec::new(),
            fonts,
            raster: Rasterizer::new(),
            background: [0.06, 0.07, 0.09, 1.0],
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    pub fn cell_size(&self) -> (f32, f32) {
        (self.fonts.cell_w, self.fonts.cell_h)
    }

    pub fn font_size(&self) -> f32 {
        self.fonts.px
    }

    /// 字の大きさを変える（Ctrl＋ホイール）。変わったら `true`。
    ///
    /// **キャッシュを消さずに寸法だけ変えてはいけない。** 前の大きさで焼いた
    /// グリフがそのまま引き伸ばされて出る。
    pub fn set_font_size(&mut self, px: f32) -> bool {
        let px = px.clamp(8.0, 48.0);
        if !self.fonts.rescale(px) {
            return false;
        }
        self.cache.clear();
        self.atlas.reset();
        // 組み方は大きさに依らないので控えは残せるが、アトラスを畳んだ以上
        // 参照先が消えている。一緒に捨てる。
        self.shaped.clear();
        true
    }

    /// 現在のウィンドウに収まる桁数・行数。
    pub fn grid_size(&self) -> (usize, usize) {
        let cols = (self.config.width as f32 / self.fonts.cell_w).floor() as usize;
        let rows = (self.config.height as f32 / self.fonts.cell_h).floor() as usize;
        (cols.max(1), rows.max(1))
    }

    pub fn begin(&mut self) {
        self.instances.clear();
    }

    /// セル座標で単色矩形を積む。
    pub fn rect(&mut self, col: f32, row: f32, w_cells: f32, h_cells: f32, color: [f32; 4]) {
        let (cw, ch) = (self.fonts.cell_w, self.fonts.cell_h);
        self.instances.push(Instance {
            pos: [col * cw, row * ch],
            size: [w_cells * cw, h_cells * ch],
            uv0: [0.0, 0.0],
            uv1: [0.0, 0.0],
            color,
            mode: 1,
            _pad: [0; 3],
        });
    }

    /// 画像を載せる。**入らなくなったら古いものから捨てる。**
    /// 端末に出た絵は流れて消えるものなので、無理に全部を抱えない。
    pub fn upload_image(&mut self, rgba: &[u8], w: u32, h: u32) -> Option<ImageSlot> {
        self.images.alloc(&self.queue, rgba, w, h)
    }

    /// 載せた画像をセル座標に積む。**流れて無効になったものは黙って出さない。**
    pub fn image(&mut self, col: f32, row: f32, w_cells: f32, h_cells: f32, slot: ImageSlot) {
        if slot.generation != self.images.generation {
            return;
        }
        let (cw, ch) = (self.fonts.cell_w, self.fonts.cell_h);
        self.instances.push(Instance {
            pos: [col * cw, row * ch],
            size: [w_cells * cw, h_cells * ch],
            uv0: slot.uv0,
            uv1: slot.uv1,
            color: [1.0, 1.0, 1.0, 1.0],
            mode: 2,
            _pad: [0; 3],
        });
    }

    /// 書記素クラスタを1つ、指定セルに積む。
    pub fn glyph(&mut self, col: f32, row: f32, cluster: &str, color: [f32; 4]) {
        // M0-b は基底文字のみ描く（結合文字の合成は M1 のシェーピングと一緒に入れる）。
        let Some(c) = cluster.chars().next() else {
            return;
        };
        if c == ' ' {
            return;
        }
        // 面で出来ている字は自分で塗る。字体に任せると隙間が空くか、
        // そもそも持っていない（`block_shape` の説明）。
        if let Some(b) = block_shape(c) {
            let color = [color[0], color[1], color[2], color[3] * b.alpha];
            for r in b.rects {
                self.rect(col + r[0], row + r[1], r[2], r[3], color);
            }
            return;
        }
        let Some((font_idx, gid)) = self.fonts.glyph_for(c) else {
            return;
        };
        self.place_glyph(col, row, font_idx, gid, color, self.cells_of(c));
    }

    /// その文字に割り当てられるセル数。**格子が正**なので、フォールバックの
    /// 字形はこの幅へ押し込める。
    fn cells_of(&self, c: char) -> f32 {
        let w = if self.amb_wide {
            unicode_advance_cjk(c)
        } else {
            unicode_advance(c)
        };
        w.max(1) as f32
    }

    /// East Asian Ambiguous を 2 セルとして数えるか（`[font] ambiguous_width`）。
    ///
    /// **セル数の数え方は端末側と揃える。** ずれると、フォールバックの字形を
    /// 押し込める幅を間違える。
    pub fn set_ambiguous_wide(&mut self, wide: bool) {
        if self.amb_wide != wide {
            self.amb_wide = wide;
            self.cache.clear();
        }
    }

    /// アトラスに載せて 1 つ積む。文字ではなく**グリフ ID** で指す
    /// （合字は元の文字に対応するグリフを持たない）。
    fn place_glyph(
        &mut self,
        col: f32,
        row: f32,
        font_idx: usize,
        gid: u16,
        color: [f32; 4],
        cells: f32,
    ) {
        // **はみ出す字形は縮めて入れる。** フォールバックのフォントは
        // 等幅ではないので、1 セルの記号（`✳` `✔` `⚠`）が 1.5〜2 セルの
        // 幅で来る。そのまま置くと隣の字に重なって読めなくなる。
        // 基準フォントは触らない（セル寸法の出どころで、合字は
        // 何セルぶんかの幅を持つのが正しい）。
        let squeeze = if font_idx == 0 {
            1.0
        } else {
            let allot = cells * self.fonts.cell_w;
            squeeze_to_fit(self.fonts.advance_of_gid(font_idx, gid), allot)
        };
        let key = (font_idx, gid, (squeeze * 64.0) as u16);
        let entry = *self.cache.entry(key).or_insert_with(|| {
            let px = self.fonts.px_for(font_idx) * squeeze;
            let g = self.raster.render(&self.fonts.fonts[font_idx], px, gid)?;
            let (uv0, uv1) = self.atlas.alloc(&self.queue, &g)?;
            Some(AtlasEntry {
                uv0,
                uv1,
                width: g.width as f32,
                height: g.height as f32,
                left: g.left as f32,
                top: g.top as f32,
            })
        });
        let Some(e) = entry else { return };

        let (cw, ch) = (self.fonts.cell_w, self.fonts.cell_h);
        let origin_x = col * cw;
        let baseline_y = row * ch + self.fonts.ascent;

        self.instances.push(Instance {
            pos: [origin_x + e.left, baseline_y - e.top],
            size: [e.width, e.height],
            uv0: e.uv0,
            uv1: e.uv1,
            color,
            mode: 0,
            _pad: [0; 3],
        });
    }

    /// 合字を組むか（設定から）。切り替えたら控えは捨てる。
    pub fn set_ligatures(&mut self, on: bool) {
        if self.ligatures != on {
            self.ligatures = on;
            self.shaped.clear();
        }
    }

    pub fn ligatures(&self) -> bool {
        self.ligatures
    }

    /// **1 セル 1 文字**の並びを、合字を組みつつ積む。
    ///
    /// 端末は格子なので、シェーピングの送り幅をそのまま使ってはいけない。
    /// 合字の字形は「入力の何文字ぶんか」を 1 つのクラスタとして返すので、
    /// **クラスタの先頭が何セル目か**を見て置く。等幅フォントの合字は
    /// その字数ぶんの幅に設計されているので、これで格子と合う。
    ///
    /// 呼ぶ側の責任:
    /// - `text` は 1 文字 1 セル（全角・結合文字を混ぜない）
    /// - 色が変わるところ、カーソルの居るところで**run を切る**
    ///   （合字は 1 つの字形なので、途中で色を変えられない。カーソルの下では
    ///   元の字が見えたほうが直せる）
    pub fn glyph_run(&mut self, col: f32, row: f32, text: &str, color: [f32; 4]) {
        if text.is_empty() {
            return;
        }
        if !self.ligatures || !may_ligate(text) {
            for (i, c) in text.chars().enumerate() {
                self.glyph(col + i as f32, row, c.encode_utf8(&mut [0u8; 4]), color);
            }
            return;
        }
        let Some(placed) = self.shape_run(text) else {
            for (i, c) in text.chars().enumerate() {
                self.glyph(col + i as f32, row, c.encode_utf8(&mut [0u8; 4]), color);
            }
            return;
        };
        for (gid, cell) in placed {
            self.place_glyph(col + f32::from(cell), row, 0, gid, color, 1.0);
        }
    }

    /// 基準フォントで組んだ (グリフ, 何セル目) の列。組めなければ `None`。
    ///
    /// 組むこと自体は `FontStack` の仕事で、ここは**同じ行を毎フレーム
    /// 組み直さない**ための控えを持つだけ。
    fn shape_run(&mut self, text: &str) -> Option<Vec<(u16, u16)>> {
        if let Some(hit) = self.shaped.get(text) {
            return Some(hit.to_vec());
        }
        let placed = self.fonts.shape_cells(text)?;
        // 控えが際限なく増えないようにする。行の中身は無限にあり得る。
        if self.shaped.len() >= SHAPE_CACHE_MAX {
            self.shaped.clear();
        }
        self.shaped
            .insert(text.into(), placed.clone().into_boxed_slice());
        Some(placed)
    }

    /// 文字列を左から積む。CJK は 2 セル進める。
    pub fn text(&mut self, col: f32, row: f32, s: &str, color: [f32; 4], amb_wide: bool) {
        let mut x = col;
        for c in s.chars() {
            let w = if amb_wide {
                unicode_advance_cjk(c)
            } else {
                unicode_advance(c)
            };
            if w > 0 {
                self.glyph(x, row, &c.to_string(), color);
                x += w as f32;
            }
        }
    }

    pub fn present(&mut self) -> Result<()> {
        if self.instances.len() > self.instance_cap {
            self.instance_cap = self.instances.len().next_power_of_two();
            self.instance_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("tsg instances"),
                size: (self.instance_cap * std::mem::size_of::<Instance>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }

        self.queue.write_buffer(
            &self.uniform_buf,
            0,
            bytemuck::bytes_of(&Uniforms {
                screen: [self.config.width as f32, self.config.height as f32],
                _pad: [0.0; 2],
            }),
        );
        if !self.instances.is_empty() {
            self.queue
                .write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(&self.instances));
        }

        use wgpu::CurrentSurfaceTexture as Cst;
        let frame = match self.surface.get_current_texture() {
            Cst::Success(f) | Cst::Suboptimal(f) => f,
            Cst::Outdated | Cst::Lost => {
                // サーフェスを作り直して1回だけ再挑戦する。
                self.surface.configure(&self.device, &self.config);
                match self.surface.get_current_texture() {
                    Cst::Success(f) | Cst::Suboptimal(f) => f,
                    other => return Err(anyhow::anyhow!("サーフェスの再取得に失敗: {other:?}")),
                }
            }
            // このフレームは諦めてよい（最小化・別ウィンドウの背後など）。
            Cst::Timeout | Cst::Occluded => return Ok(()),
            other => return Err(anyhow::anyhow!("サーフェスの取得に失敗: {other:?}")),
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("tsg encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("tsg pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(self.clear_color()),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if !self.instances.is_empty() {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, self.instance_buf.slice(..));
                pass.draw(0..6, 0..self.instances.len() as u32);
            }
        }
        self.queue.submit(Some(encoder.finish()));
        self.queue.present(frame);
        Ok(())
    }
}

/// 罫線ではなく**面**で出来ている字（U+2580..U+259F）。
///
/// **字体から採らない。** 半分・八分の一・四分割は幾何そのものなので、
/// セルに対する割合で塗れば必ず隙間なく敷き詰まる。字体に任せると
/// ①持っていない字がある（Consolas と MS Gothic には四分割が無く、
/// Claude Code のロゴが崩れた）②持っていても字体の em に合わせて
/// 設計されているのでセル格子とずれる、の 2 つを同時に踏む。
struct BlockShape {
    /// セルを 1x1 としたときの (x, y, 幅, 高さ)。
    rects: &'static [[f32; 4]],
    /// 網かけ（░▒▓）の濃さ。面の字は 1.0。
    alpha: f32,
}

const T: f32 = 1.0 / 8.0;

fn block_shape(c: char) -> Option<BlockShape> {
    let solid = |rects| Some(BlockShape { rects, alpha: 1.0 });
    match c {
        // 上半分・下から八分の n
        '\u{2580}' => solid(&[[0.0, 0.0, 1.0, 0.5]]),
        '\u{2581}' => solid(&[[0.0, 7.0 * T, 1.0, T]]),
        '\u{2582}' => solid(&[[0.0, 6.0 * T, 1.0, 2.0 * T]]),
        '\u{2583}' => solid(&[[0.0, 5.0 * T, 1.0, 3.0 * T]]),
        '\u{2584}' => solid(&[[0.0, 0.5, 1.0, 0.5]]),
        '\u{2585}' => solid(&[[0.0, 3.0 * T, 1.0, 5.0 * T]]),
        '\u{2586}' => solid(&[[0.0, 2.0 * T, 1.0, 6.0 * T]]),
        '\u{2587}' => solid(&[[0.0, T, 1.0, 7.0 * T]]),
        '\u{2588}' => solid(&[[0.0, 0.0, 1.0, 1.0]]),
        // 左から八分の n
        '\u{2589}' => solid(&[[0.0, 0.0, 7.0 * T, 1.0]]),
        '\u{258a}' => solid(&[[0.0, 0.0, 6.0 * T, 1.0]]),
        '\u{258b}' => solid(&[[0.0, 0.0, 5.0 * T, 1.0]]),
        '\u{258c}' => solid(&[[0.0, 0.0, 0.5, 1.0]]),
        '\u{258d}' => solid(&[[0.0, 0.0, 3.0 * T, 1.0]]),
        '\u{258e}' => solid(&[[0.0, 0.0, 2.0 * T, 1.0]]),
        '\u{258f}' => solid(&[[0.0, 0.0, T, 1.0]]),
        '\u{2590}' => solid(&[[0.5, 0.0, 0.5, 1.0]]),
        // 網かけ。点の模様ではなく濃さで出す（拡大しても模様が壊れない）。
        '\u{2591}' => Some(BlockShape {
            rects: &[[0.0, 0.0, 1.0, 1.0]],
            alpha: 0.25,
        }),
        '\u{2592}' => Some(BlockShape {
            rects: &[[0.0, 0.0, 1.0, 1.0]],
            alpha: 0.5,
        }),
        '\u{2593}' => Some(BlockShape {
            rects: &[[0.0, 0.0, 1.0, 1.0]],
            alpha: 0.75,
        }),
        '\u{2594}' => solid(&[[0.0, 0.0, 1.0, T]]),
        '\u{2595}' => solid(&[[7.0 * T, 0.0, T, 1.0]]),
        // 四分割
        '\u{2596}' => solid(&[[0.0, 0.5, 0.5, 0.5]]),
        '\u{2597}' => solid(&[[0.5, 0.5, 0.5, 0.5]]),
        '\u{2598}' => solid(&[[0.0, 0.0, 0.5, 0.5]]),
        '\u{2599}' => solid(&[[0.0, 0.0, 0.5, 0.5], [0.0, 0.5, 1.0, 0.5]]),
        '\u{259a}' => solid(&[[0.0, 0.0, 0.5, 0.5], [0.5, 0.5, 0.5, 0.5]]),
        '\u{259b}' => solid(&[[0.0, 0.0, 1.0, 0.5], [0.0, 0.5, 0.5, 0.5]]),
        '\u{259c}' => solid(&[[0.0, 0.0, 1.0, 0.5], [0.5, 0.5, 0.5, 0.5]]),
        '\u{259d}' => solid(&[[0.5, 0.0, 0.5, 0.5]]),
        '\u{259e}' => solid(&[[0.5, 0.0, 0.5, 0.5], [0.0, 0.5, 0.5, 0.5]]),
        '\u{259f}' => solid(&[[0.5, 0.0, 0.5, 0.5], [0.0, 0.5, 1.0, 0.5]]),
        _ => None,
    }
}

/// 割り当てた幅に収めるための倍率。**入っているものは触らない。**
///
/// 2% の遊びは、丸めで 1 画素はみ出しただけの字まで縮めないため。
fn squeeze_to_fit(advance: Option<f32>, allot: f32) -> f32 {
    match advance {
        Some(a) if a > allot * 1.02 && a > 0.0 => allot / a,
        _ => 1.0,
    }
}

fn unicode_advance(c: char) -> usize {
    use unicode_width::UnicodeWidthChar;
    c.width().unwrap_or(0)
}

fn unicode_advance_cjk(c: char) -> usize {
    use unicode_width::UnicodeWidthChar;
    c.width_cjk().unwrap_or(0)
}

impl Renderer {
    /// 背景のクリア色。
    ///
    /// プリマルチプライドの面では RGB に不透明度を掛けて渡す。掛けないと
    /// 半透明にしたときだけ背景が白茶ける（sRGB のときと同じ種類の間違い）。
    fn clear_color(&self) -> wgpu::Color {
        let a = f64::from(self.background[3]);
        let scale = if self.premultiplied { a } else { 1.0 };
        wgpu::Color {
            r: srgb_to_linear(self.background[0]) * scale,
            g: srgb_to_linear(self.background[1]) * scale,
            b: srgb_to_linear(self.background[2]) * scale,
            a,
        }
    }
}

/// sRGB -> リニア。シェーダ側の `to_linear` と同じ式。
///
/// クリア色だけは頂点を通らないので、CPU 側でも同じ変換が要る。
/// 片方だけ直すと、文字の載っていない領域と載っている領域で背景色が食い違う。
pub fn srgb_to_linear(c: f32) -> f64 {
    let c = f64::from(c);
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

const SHADER: &str = r#"
struct Uniforms { screen: vec2<f32>, pad: vec2<f32> };
@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var atlas_tex: texture_2d<f32>;
@group(0) @binding(2) var atlas_smp: sampler;
@group(0) @binding(3) var img_tex: texture_2d<f32>;

struct InstanceIn {
  @location(0) pos:   vec2<f32>,
  @location(1) size:  vec2<f32>,
  @location(2) uv0:   vec2<f32>,
  @location(3) uv1:   vec2<f32>,
  @location(4) color: vec4<f32>,
  @location(5) mode:  u32,
};

struct VsOut {
  @builtin(position) clip: vec4<f32>,
  @location(0) uv: vec2<f32>,
  @location(1) color: vec4<f32>,
  @location(2) @interpolate(flat) mode: u32,
};

// sRGB -> リニア。
//
// サーフェスは sRGB フォーマットなので、GPU は書き込み時にリニア -> sRGB を
// 自動で行う。色を sRGB のまま渡すと二重に明るくなる（背景 #0F1217 が #454B54 に化けた）。
// 色は人が読める sRGB で書き続けたいので、変換はここ 1 箇所で吸収する。
fn to_linear(c: vec3<f32>) -> vec3<f32> {
  let lo = c / 12.92;
  let hi = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
  return select(hi, lo, c <= vec3<f32>(0.04045));
}

@vertex
fn vs(@builtin(vertex_index) vi: u32, inst: InstanceIn) -> VsOut {
  var corners = array<vec2<f32>, 6>(
    vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
    vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0)
  );
  let c = corners[vi];
  let p = inst.pos + c * inst.size;
  var out: VsOut;
  out.clip  = vec4<f32>(p.x / u.screen.x * 2.0 - 1.0, 1.0 - p.y / u.screen.y * 2.0, 0.0, 1.0);
  out.uv    = mix(inst.uv0, inst.uv1, c);
  out.color = vec4<f32>(to_linear(inst.color.rgb), inst.color.a);
  out.mode  = inst.mode;
  return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
  if (in.mode == 1u) {
    return in.color;
  }
  if (in.mode == 2u) {
    // 画像。色は面が持っているので、頂点の色は透過だけに使う。
    let px = textureSample(img_tex, atlas_smp, in.uv);
    return vec4<f32>(px.rgb, px.a * in.color.a);
  }
  let a = textureSample(atlas_tex, atlas_smp, in.uv).r;
  return vec4<f32>(in.color.rgb, in.color.a * a);
}
"#;

#[cfg(test)]
mod block_tests {
    use super::*;

    /// **面の字は隙間なく敷き詰まる。**
    ///
    /// 四分割（`▛▜▝▖`）は Consolas にも MS Gothic にも無く、Claude Code の
    /// ロゴが崩れていた。字体に任せず自分で塗るので、ここでは「割合が
    /// 正しいか」だけを見る。
    #[test]
    fn the_quadrants_and_halves_cover_exactly_their_share_of_the_cell() {
        let area = |c: char| -> f32 {
            block_shape(c)
                .map(|b| b.rects.iter().map(|r| r[2] * r[3]).sum::<f32>() * b.alpha)
                .unwrap_or(0.0)
        };
        assert_eq!(area('\u{2588}'), 1.0, "█ が全面でない");
        assert_eq!(area('\u{2580}'), 0.5, "▀ が半分でない");
        assert_eq!(area('\u{2584}'), 0.5, "▄ が半分でない");
        assert_eq!(area('\u{258c}'), 0.5, "▌ が半分でない");
        assert_eq!(area('\u{2590}'), 0.5, "▐ が半分でない");
        assert_eq!(area('\u{259d}'), 0.25, "▝ が四分の一でない");
        assert_eq!(area('\u{2596}'), 0.25, "▖ が四分の一でない");
        assert_eq!(area('\u{259b}'), 0.75, "▛ が四分の三でない");
        assert_eq!(area('\u{259f}'), 0.75, "▟ が四分の三でない");
        assert_eq!(area('\u{259e}'), 0.5, "▞ が半分でない");
        assert_eq!(area('\u{2592}'), 0.5, "▒ の濃さが半分でない");

        // 上下・左右の対は、重ならずにセルを埋める。
        for (a, b) in [
            ('\u{2580}', '\u{2584}'),
            ('\u{258c}', '\u{2590}'),
            ('\u{259b}', '\u{2597}'),
            ('\u{259e}', '\u{259a}'),
        ] {
            assert_eq!(area(a) + area(b), 1.0, "{a}{b} で埋まらない");
        }

        // 面ではない字（罫線・普通の字）はここへ来ない。
        assert!(block_shape('\u{2500}').is_none(), "罫線を塗ってしまう");
        assert!(block_shape('a').is_none());
        assert!(block_shape('日').is_none());
    }

    /// **はみ出す字形は縮めて入れる。**
    ///
    /// フォールバックのフォントは等幅ではないので、1 セルの記号
    /// （`✳` `✔` `⚠`）が 1.5〜2 セルの幅で来る。そのまま置くと隣に重なる。
    #[test]
    fn a_glyph_wider_than_its_cell_is_squeezed_into_it() {
        assert_eq!(
            squeeze_to_fit(Some(20.0), 10.0),
            0.5,
            "2 セルの字が縮まない"
        );
        assert_eq!(squeeze_to_fit(Some(15.0), 10.0), 10.0 / 15.0);
        assert_eq!(squeeze_to_fit(Some(9.9), 10.0), 1.0, "入っている字を縮めた");
        assert_eq!(squeeze_to_fit(Some(10.1), 10.0), 1.0, "丸め誤差で縮めた");
        assert_eq!(squeeze_to_fit(None, 10.0), 1.0);
        assert_eq!(squeeze_to_fit(Some(0.0), 10.0), 1.0, "幅 0 で 0 除算");
    }

    /// セルの外へはみ出す形を作らない。はみ出すと隣の字が欠ける。
    #[test]
    fn no_block_shape_reaches_outside_its_cell() {
        for code in 0x2580u32..=0x259f {
            let c = char::from_u32(code).unwrap();
            let Some(b) = block_shape(c) else { continue };
            for r in b.rects {
                assert!(r[0] >= 0.0 && r[1] >= 0.0, "U+{code:04X} が左上へはみ出す");
                assert!(
                    r[0] + r[2] <= 1.0 + f32::EPSILON && r[1] + r[3] <= 1.0 + f32::EPSILON,
                    "U+{code:04X} が右下へはみ出す"
                );
                assert!(r[2] > 0.0 && r[3] > 0.0, "U+{code:04X} に潰れた面がある");
            }
        }
    }
}
