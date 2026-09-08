//! wgpu の初期化と、文字グリッドの描画。
//!
//! ウィンドウ全体を 1 枚の文字グリッドとして扱う。左ペインも端末も
//! 同じセル寸法で並ぶため、位置合わせの規則が 1 つで済む。
//!
//! 文字は「1 文字ぶんの `Buffer` を字ごとに作って使い回し、
//! セル座標へ正確に置く」方式で描く。行をまとめて 1 つの `Buffer` にすると、
//! 全角文字が別のフォントへ落ちたときに以降の桁がずれる。

use std::collections::HashMap;
use std::sync::Arc;

use alacritty_terminal::vte::ansi::Rgb;
use glyphon::{
    Attrs, Buffer, Cache, Color as GColor, Family, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Weight,
};
use winit::window::Window;

use crate::rect::{Rect, RectRenderer};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct GlyphKey {
    c: char,
    bold: bool,
    italic: bool,
}

struct CellDraw {
    x: f32,
    y: f32,
    key: GlyphKey,
    color: GColor,
}

/// 1 セルの寸法（物理ピクセル）。
#[derive(Clone, Copy, Debug)]
pub struct CellMetrics {
    pub width: f32,
    pub height: f32,
}

/// 画面へ出すか、オフスクリーンのテクスチャへ描くか。
struct SurfaceTarget {
    instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    window: Arc<Window>,
}

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    target: Option<SurfaceTarget>,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,

    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    rects: RectRenderer,

    glyphs: HashMap<GlyphKey, Buffer>,
    family: Option<String>,
    metrics: Metrics,
    cell: CellMetrics,
    scale: f32,
    font_size: f32,

    cell_draws: Vec<CellDraw>,
    rect_draws: Vec<Rect>,

    /// フレームの内訳を測る。`TEX_FRAME_LOG` を指定したときだけ動く。
    timing: Option<FrameTiming>,
    /// ベンチで使い回す出力先。
    bench_target: Option<(wgpu::Texture, wgpu::TextureView)>,
}

/// 1 フレームの内訳。遅さの原因を工程ごとに切り分けるために取る。
#[derive(Default)]
pub struct FrameTiming {
    pub frames: u64,
    pub prepare_us: u64,
    pub acquire_us: u64,
    pub encode_us: u64,
    pub present_us: u64,
    pub cells: u64,
    pub rects: u64,
    pub glyph_cache: u64,
    last_report: Option<std::time::Instant>,
}

impl FrameTiming {
    fn report(&mut self, force: bool) {
        let now = std::time::Instant::now();
        let due = match self.last_report {
            None => true,
            Some(t) => now.duration_since(t).as_millis() >= 1000,
        };
        if !(due || force) || self.frames == 0 {
            return;
        }
        self.last_report = Some(now);
        let n = self.frames as f64;
        log::info!(
            "frames={} 平均 prepare={:.2}ms acquire={:.2}ms encode={:.2}ms present={:.2}ms 合計={:.2}ms  cells={:.0} rects={:.0} 字形={}",
            self.frames,
            self.prepare_us as f64 / n / 1000.0,
            self.acquire_us as f64 / n / 1000.0,
            self.encode_us as f64 / n / 1000.0,
            self.present_us as f64 / n / 1000.0,
            (self.prepare_us + self.acquire_us + self.encode_us + self.present_us) as f64 / n / 1000.0,
            self.cells as f64 / n,
            self.rects as f64 / n,
            self.glyph_cache,
        );
        *self = FrameTiming {
            last_report: self.last_report,
            glyph_cache: self.glyph_cache,
            ..Default::default()
        };
    }
}

impl Renderer {
    pub async fn new(
        window: Arc<Window>,
        event_loop: &winit::event_loop::ActiveEventLoop,
        font_name: &str,
        font_size: f32,
        vsync: bool,
    ) -> Renderer {
        let physical_size = window.inner_size();
        let scale = window.scale_factor() as f32;

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(
            Box::new(event_loop.owned_display_handle()),
        ));
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .expect("GPU アダプタが見つからない");
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .expect("GPU デバイスを作れない");

        let surface = instance
            .create_surface(window.clone())
            .expect("サーフェスを作れない");
        let format = wgpu::TextureFormat::Bgra8UnormSrgb;

        // 表示待ちが遅さの主因なので、待ちの短い方式を選ぶ。
        // Mailbox は vsync に合わせつつ、投入したフレームを待たせない。
        // Fifo はどの環境でも使えるが、行列に並ぶぶん遅れる。
        let caps = surface.get_capabilities(&adapter);
        let present_mode = if !vsync && caps.present_modes.contains(&wgpu::PresentMode::Immediate) {
            wgpu::PresentMode::Immediate
        } else if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };
        log::info!(
            "表示方式 {present_mode:?}（使えるもの: {:?}）",
            caps.present_modes
        );

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: physical_size.width.max(1),
            height: physical_size.height.max(1),
            present_mode,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
            // 1 にすると、投入したフレームが次の走査で必ず出る。
            // 2 では 1 周期ぶん古いフレームが表示されうる。
            desired_maximum_frame_latency: 1,
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &surface_config);

        let font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        let rects = RectRenderer::new(&device, format);

        // 名前で引けないフォントを指定されたら総称の等幅へ落とす。
        let family = if has_family(&font_system, font_name) {
            Some(font_name.to_string())
        } else {
            log::warn!("フォント {font_name} が見つからない。等幅の既定を使う");
            None
        };

        let mut r = Renderer {
            device,
            queue,
            target: Some(SurfaceTarget {
                instance,
                surface,
                config: surface_config,
                window,
            }),
            format,
            width: physical_size.width.max(1),
            height: physical_size.height.max(1),
            font_system,
            swash_cache,
            viewport,
            atlas,
            text_renderer,
            rects,
            glyphs: HashMap::new(),
            family,
            metrics: Metrics::new(1.0, 1.0),
            cell: CellMetrics {
                width: 1.0,
                height: 1.0,
            },
            scale,
            font_size,
            cell_draws: Vec::new(),
            rect_draws: Vec::new(),
            timing: std::env::var("TEX_FRAME_LOG").is_ok().then(FrameTiming::default),
            bench_target: None,
        };
        r.recompute_metrics();
        r
    }

    /// ウィンドウを持たない描画器を作る。描画結果の確認に使う。
    pub async fn offscreen(width: u32, height: u32, font_name: &str, font_size: f32) -> Renderer {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .expect("GPU アダプタが見つからない");
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .expect("GPU デバイスを作れない");
        let format = wgpu::TextureFormat::Bgra8UnormSrgb;

        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        let rects = RectRenderer::new(&device, format);
        let family = if has_family(&font_system, font_name) {
            Some(font_name.to_string())
        } else {
            None
        };
        let _ = &mut font_system;

        let mut r = Renderer {
            device,
            queue,
            target: None,
            format,
            width,
            height,
            font_system,
            swash_cache,
            viewport,
            atlas,
            text_renderer,
            rects,
            glyphs: HashMap::new(),
            family,
            metrics: Metrics::new(1.0, 1.0),
            cell: CellMetrics {
                width: 1.0,
                height: 1.0,
            },
            scale: 1.0,
            font_size,
            cell_draws: Vec::new(),
            rect_draws: Vec::new(),
            timing: std::env::var("TEX_FRAME_LOG").is_ok().then(FrameTiming::default),
            bench_target: None,
        };
        r.recompute_metrics();
        r
    }

    /// フォント寸法からセルの幅と高さを決め直す。
    fn recompute_metrics(&mut self) {
        let px = (self.font_size * self.scale).max(4.0);
        let line_height = (px * 1.30).round().max(px + 1.0);
        self.metrics = Metrics::new(px, line_height);

        let attrs = match &self.family {
            Some(name) => Attrs::new().family(Family::Name(name)),
            None => Attrs::new().family(Family::Monospace),
        };
        let mut probe = Buffer::new(&mut self.font_system, self.metrics);
        probe.set_size(None, None);
        probe.set_text("MMMMMMMMMM", &attrs, Shaping::Basic, None);
        probe.shape_until_scroll(&mut self.font_system, false);
        let width = probe
            .layout_runs()
            .next()
            .map(|run| run.line_w / 10.0)
            .filter(|w| *w > 0.5)
            .unwrap_or(px * 0.6);

        self.cell = CellMetrics {
            width: (width * 100.0).round() / 100.0,
            height: line_height,
        };
        self.glyphs.clear();
    }

    pub fn cell(&self) -> CellMetrics {
        self.cell
    }
    pub fn font_size(&self) -> f32 {
        self.font_size
    }

    /// ウィンドウ全体が何桁何行になるか。
    pub fn grid_size(&self) -> (usize, usize) {
        let cols = (self.width as f32 / self.cell.width).floor() as usize;
        let rows = (self.height as f32 / self.cell.height).floor() as usize;
        (cols.max(1), rows.max(1))
    }

    pub fn set_font_size(&mut self, size: f32) {
        self.font_size = size.clamp(6.0, 72.0);
        self.recompute_metrics();
    }

    pub fn set_scale(&mut self, scale: f32) {
        if (scale - self.scale).abs() > f32::EPSILON {
            self.scale = scale;
            self.recompute_metrics();
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width.max(1);
        self.height = height.max(1);
        if let Some(t) = &mut self.target {
            t.config.width = self.width;
            t.config.height = self.height;
            t.surface.configure(&self.device, &t.config);
        }
    }

    fn request_redraw(&self) {
        if let Some(t) = &self.target {
            t.window.request_redraw();
        }
    }

    // ------------------------------------------------------------ 描画の指示

    pub fn begin(&mut self) {
        self.cell_draws.clear();
        self.rect_draws.clear();
    }

    /// セル座標に矩形を置く。
    pub fn fill_cells(&mut self, col: usize, row: usize, cols: usize, rows: usize, color: Rgb) {
        if cols == 0 || rows == 0 {
            return;
        }
        self.rect_draws.push(Rect {
            pos: [col as f32 * self.cell.width, row as f32 * self.cell.height],
            size: [
                cols as f32 * self.cell.width,
                rows as f32 * self.cell.height,
            ],
            color: rgba(color, 1.0),
        });
    }

    pub fn fill_cells_alpha(
        &mut self,
        col: usize,
        row: usize,
        cols: usize,
        rows: usize,
        color: Rgb,
        alpha: f32,
    ) {
        self.rect_draws.push(Rect {
            pos: [col as f32 * self.cell.width, row as f32 * self.cell.height],
            size: [
                cols as f32 * self.cell.width,
                rows as f32 * self.cell.height,
            ],
            color: rgba(color, alpha),
        });
    }

    /// セルの下端に下線を引く。
    pub fn underline_cells(&mut self, col: usize, row: usize, cols: usize, color: Rgb) {
        let thickness = (self.cell.height / 14.0).max(1.0).round();
        self.rect_draws.push(Rect {
            pos: [
                col as f32 * self.cell.width,
                (row + 1) as f32 * self.cell.height - thickness,
            ],
            size: [cols as f32 * self.cell.width, thickness],
            color: rgba(color, 1.0),
        });
    }

    /// 縦棒のカーソル。
    pub fn cursor_beam(&mut self, col: usize, row: usize, color: Rgb) {
        let thickness = (self.cell.width / 6.0).max(1.0).round();
        self.rect_draws.push(Rect {
            pos: [col as f32 * self.cell.width, row as f32 * self.cell.height],
            size: [thickness, self.cell.height],
            color: rgba(color, 1.0),
        });
    }

    /// 1 セルに 1 文字を置く。
    pub fn put_char(&mut self, col: usize, row: usize, c: char, fg: Rgb, bold: bool, italic: bool) {
        if c == ' ' || c == '\0' {
            return;
        }
        self.cell_draws.push(CellDraw {
            x: col as f32 * self.cell.width,
            y: row as f32 * self.cell.height,
            key: GlyphKey { c, bold, italic },
            color: GColor::rgba(fg.r, fg.g, fg.b, 0xff),
        });
    }

    /// 文字列をセル座標から順に置く。全角文字は 2 桁を使う。
    pub fn put_str(&mut self, col: usize, row: usize, s: &str, fg: Rgb) -> usize {
        let mut c = col;
        for ch in s.chars() {
            self.put_char(c, row, ch, fg, false, false);
            c += char_cols(ch);
        }
        c - col
    }

    /// 幅を超える部分を省略記号に置き換えて置く。
    pub fn put_str_clipped(
        &mut self,
        col: usize,
        row: usize,
        s: &str,
        max_cols: usize,
        fg: Rgb,
    ) -> usize {
        if max_cols == 0 {
            return 0;
        }
        let total: usize = s.chars().map(char_cols).sum();
        if total <= max_cols {
            return self.put_str(col, row, s, fg);
        }
        let mut used = 0;
        let mut c = col;
        for ch in s.chars() {
            let w = char_cols(ch);
            if used + w > max_cols.saturating_sub(1) {
                break;
            }
            self.put_char(c, row, ch, fg, false, false);
            c += w;
            used += w;
        }
        self.put_char(c, row, '…', fg, false, false);
        used + 1
    }

    // ------------------------------------------------------------ 実際の描画

    /// 描画の指示から字形とバッファを用意する。
    fn prepare_frame(&mut self) {
        let Renderer {
            font_system,
            glyphs,
            metrics,
            family,
            cell_draws,
            ..
        } = self;
        for draw in cell_draws.iter() {
            if glyphs.contains_key(&draw.key) {
                continue;
            }
            let mut attrs = match family {
                Some(name) => Attrs::new().family(Family::Name(name)),
                None => Attrs::new().family(Family::Monospace),
            };
            if draw.key.bold {
                attrs = attrs.weight(Weight::BOLD);
            }
            if draw.key.italic {
                attrs = attrs.style(glyphon::Style::Italic);
            }
            let mut buffer = Buffer::new(font_system, *metrics);
            buffer.set_size(None, None);
            let mut s = [0u8; 4];
            // Basic では字形の代替が働かず、等幅フォントにない文字が
            // 豆腐になる。1 文字ずつ整形しているので合字は生じない。
            buffer.set_text(draw.key.c.encode_utf8(&mut s), &attrs, Shaping::Advanced, None);
            buffer.shape_until_scroll(font_system, false);
            glyphs.insert(draw.key, buffer);
        }

        let (w, h) = (self.width, self.height);
        self.viewport.update(
            &self.queue,
            Resolution {
                width: w,
                height: h,
            },
        );
        self.rects
            .prepare(&self.device, &self.queue, (w, h), &self.rect_draws);

        let areas: Vec<TextArea<'_>> = self
            .cell_draws
            .iter()
            .filter_map(|d| {
                self.glyphs.get(&d.key).map(|buffer| TextArea {
                    buffer,
                    left: d.x,
                    top: d.y,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: 0,
                        top: 0,
                        right: w as i32,
                        bottom: h as i32,
                    },
                    default_color: d.color,
                    custom_glyphs: &[],
                })
            })
            .collect();

        if let Err(e) = self.text_renderer.prepare(
            &self.device,
            &self.queue,
            &mut self.font_system,
            &mut self.atlas,
            &self.viewport,
            areas,
            &mut self.swash_cache,
        ) {
            log::warn!("文字の準備に失敗: {e}");
        }
    }

    fn encode_pass(&self, encoder: &mut wgpu::CommandEncoder, view: &wgpu::TextureView, bg: Rgb) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("tex-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(clear_color(bg)),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        self.rects.render(&mut pass);
        if let Err(e) = self
            .text_renderer
            .render(&self.atlas, &self.viewport, &mut pass)
        {
            log::warn!("文字の描画に失敗: {e}");
        }
    }

    pub fn render(&mut self, background: Rgb) {
        let t0 = std::time::Instant::now();
        let (n_cells, n_rects) = (self.cell_draws.len(), self.rect_draws.len());
        self.prepare_frame();
        let t_prepare = t0.elapsed();
        let t1 = std::time::Instant::now();
        let Some(target) = &self.target else {
            return;
        };

        let frame = match target.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => f,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                // ここで再描画を要求すると、隠れているあいだ要求と失敗を
                // 際限なく繰り返して CPU を焼く。次の出来事まで待つ。
                return;
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Suboptimal(_) => {
                target.surface.configure(&self.device, &target.config);
                self.request_redraw();
                return;
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                let surface = target
                    .instance
                    .create_surface(target.window.clone())
                    .expect("サーフェスを作り直せない");
                surface.configure(&self.device, &target.config);
                if let Some(t) = &mut self.target {
                    t.surface = surface;
                }
                self.request_redraw();
                return;
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                log::error!("サーフェスの取得で検証エラー");
                return;
            }
        };

        let t_acquire = t1.elapsed();
        let t2 = std::time::Instant::now();
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        self.encode_pass(&mut encoder, &view, background);
        self.queue.submit(Some(encoder.finish()));
        let t_encode = t2.elapsed();
        let t3 = std::time::Instant::now();
        self.queue.present(frame);
        let t_present = t3.elapsed();
        self.atlas.trim();

        let glyphs = self.glyphs.len() as u64;
        if let Some(t) = &mut self.timing {
            t.frames += 1;
            t.prepare_us += t_prepare.as_micros() as u64;
            t.acquire_us += t_acquire.as_micros() as u64;
            t.encode_us += t_encode.as_micros() as u64;
            t.present_us += t_present.as_micros() as u64;
            t.cells += n_cells as u64;
            t.rects += n_rects as u64;
            t.glyph_cache = glyphs;
            t.report(false);
        }
    }

    /// 描画の指示だけを組み立てて、CPU 側の費用を測る（ベンチ用）。
    pub fn bench_prepare(&mut self) -> std::time::Duration {
        let t = std::time::Instant::now();
        self.prepare_frame();
        t.elapsed()
    }

    /// 読み戻しをせずに 1 フレームを GPU へ投げる（ベンチ用）。
    ///
    /// 出力先のテクスチャは使い回す。毎フレーム作ると、
    /// 画面いっぱいの大きさでは確保だけで数百 MB になる。
    pub fn bench_submit(&mut self, background: Rgb) -> std::time::Duration {
        if self.bench_target.is_none() {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("tex-bench"),
                size: wgpu::Extent3d {
                    width: self.width,
                    height: self.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: self.format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.bench_target = Some((texture, view));
        }
        let view = &self.bench_target.as_ref().unwrap().1;
        let t = std::time::Instant::now();
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        self.encode_pass(&mut encoder, view, background);
        self.queue.submit(Some(encoder.finish()));
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        self.atlas.trim();
        t.elapsed()
    }

    /// 1 フレームをテクスチャへ描き、RGBA のバイト列として取り出す。
    pub fn render_to_pixels(&mut self, background: Rgb) -> Vec<u8> {
        self.prepare_frame();
        let (w, h) = (self.width, self.height);
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("tex-offscreen"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        // コピーの 1 行は 256 バイト境界に揃える必要がある。
        let unpadded = w as usize * 4;
        let padded = unpadded.div_ceil(256) * 256;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tex-readback"),
            size: (padded * h as usize) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        self.encode_pass(&mut encoder, &view, background);
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded as u32),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));

        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        rx.recv().expect("読み戻しを待てない").expect("読み戻しに失敗");

        let data = slice.get_mapped_range().expect("読み戻しの領域を取れない");
        // BGRA から RGBA へ並べ替えつつ、行の詰め物を落とす。
        let mut out = Vec::with_capacity(unpadded * h as usize);
        for row in 0..h as usize {
            let start = row * padded;
            for px in data[start..start + unpadded].chunks_exact(4) {
                out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
            }
        }
        drop(data);
        buffer.unmap();
        self.atlas.trim();
        out
    }
}

fn rgba(c: Rgb, a: f32) -> [f32; 4] {
    [
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        a,
    ]
}

/// クリア色は線形空間で渡す。
fn clear_color(c: Rgb) -> wgpu::Color {
    fn lin(v: u8) -> f64 {
        let c = v as f64 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    wgpu::Color {
        r: lin(c.r),
        g: lin(c.g),
        b: lin(c.b),
        a: 1.0,
    }
}

/// 文字が占める桁数。
pub fn char_cols(c: char) -> usize {
    use unicode_width::UnicodeWidthChar;
    match c.width() {
        Some(0) | None => 1,
        Some(w) => w,
    }
}

fn has_family(fs: &FontSystem, name: &str) -> bool {
    fs.db()
        .faces()
        .any(|f| f.families.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)))
}

#[cfg(test)]
mod tests {
    use super::char_cols;

    #[test]
    fn 全角文字は二桁を占める() {
        assert_eq!(char_cols('a'), 1);
        assert_eq!(char_cols('あ'), 2);
        assert_eq!(char_cols('漢'), 2);
        assert_eq!(char_cols('…'), 1);
    }

    #[test]
    fn 幅のない文字も一桁として数える() {
        // 結合文字を 0 桁にするとセル位置がずれるため 1 桁として扱う。
        assert_eq!(char_cols('\u{0301}'), 1);
    }
}
