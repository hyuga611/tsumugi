//! wgpu によるセル描画。
//!
//! `arch.md` の依存の向きに従い、このクレートは winit を知らない。
//! ウィンドウは `HasWindowHandle + HasDisplayHandle` として受け取る。
//!
//! M0-b の範囲: 単色テキスト + 矩形。SGR の色属性は M1。

pub mod font;

use std::collections::HashMap;

use anyhow::{Context, Result};
use bytemuck::{Pod, Zeroable};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};

pub use font::{FontStack, RasterizedGlyph, Rasterizer};

const ATLAS_SIZE: u32 = 2048;

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
    /// 0 = アトラスのグリフ、1 = 単色矩形
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
    cache: HashMap<(usize, u16), Option<AtlasEntry>>,

    instances: Vec<Instance>,
    /// 合成面がプリマルチプライドか。クリア色の出し方が変わる。
    premultiplied: bool,
    pub fonts: FontStack,
    raster: Rasterizer,
    pub background: [f32; 4],
}

impl Renderer {
    pub fn new<W>(window: W, width: u32, height: u32, font_px: f32, transparent: bool) -> Result<Self>
    where
        W: HasWindowHandle + HasDisplayHandle + Send + Sync + 'static,
    {
        let fonts = FontStack::discover(font_px)?;

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
            cache: HashMap::new(),
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

    /// 書記素クラスタを1つ、指定セルに積む。
    pub fn glyph(&mut self, col: f32, row: f32, cluster: &str, color: [f32; 4]) {
        // M0-b は基底文字のみ描く（結合文字の合成は M1 のシェーピングと一緒に入れる）。
        let Some(c) = cluster.chars().next() else {
            return;
        };
        if c == ' ' {
            return;
        }
        let Some((font_idx, gid)) = self.fonts.glyph_for(c) else {
            return;
        };

        let entry = *self.cache.entry((font_idx, gid)).or_insert_with(|| {
            let px = self.fonts.px_for(font_idx);
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
  let a = textureSample(atlas_tex, atlas_smp, in.uv).r;
  return vec4<f32>(in.color.rgb, in.color.a * a);
}
"#;
