//! `wgpu` ベースのゼロコピー指向 GPU コンポジタ＆デュアルレンダーターゲット

use crate::async_rt::block_on;
use crate::governor::{GovernorStats, OverlayGovernor};
use core_types::clock::PtsUs;
use core_types::media::LayerTransform;
use core_types::traits::RenderSink;
use core_types::{CoreError, CoreResult};
use std::time::Instant;
use wgpu::{
    AddressMode, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingResource, BindingType, BlendState,
    Buffer, BufferDescriptor, BufferUsages, Color, ColorTargetState, ColorWrites, CommandEncoder,
    CommandEncoderDescriptor, Device, DeviceDescriptor, Extent3d, Features, FilterMode,
    FragmentState, ImageCopyBuffer, ImageCopyTexture, ImageDataLayout, Instance,
    InstanceDescriptor, Limits, LoadOp, Maintain, MapMode, MultisampleState, Operations, Origin3d,
    PipelineLayoutDescriptor, PowerPreference, PrimitiveState, PrimitiveTopology, Queue,
    RenderPassColorAttachment, RenderPassDescriptor, RenderPipeline, RenderPipelineDescriptor,
    RequestAdapterOptions, Sampler, SamplerBindingType, SamplerDescriptor, ShaderModuleDescriptor,
    ShaderSource, ShaderStages, StoreOp, Texture, TextureAspect, TextureDescriptor,
    TextureDimension, TextureFormat, TextureSampleType, TextureUsages, TextureView,
    TextureViewDescriptor, TextureViewDimension, VertexState,
};

const COMPOSITOR_WGSL: &str = include_str!("shader.wgsl");

/// シェーダーが扱う入力ソース種別
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum LayerInputKind {
    Rgba = 0,
    Nv12 = 1,
    YuyvDual = 2,
    TopBottomRgba = 3,
    TopBottomNv12 = 4,
}

impl LayerInputKind {
    pub fn is_video_overlay(self) -> bool {
        matches!(
            self,
            Self::Nv12 | Self::YuyvDual | Self::TopBottomRgba | Self::TopBottomNv12
        )
    }
}

/// YUV -> RGB 変換に使う色空間マトリクス
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ColorSpace {
    Bt709Limited = 0,
    Bt601Limited = 1,
    Bt709Full = 2,
    Bt601Full = 3,
}

/// Uniform バッファへ書き込む 64-byte レイアウト（`std140` 互換）
#[derive(Debug, Clone, Copy)]
struct LayerUniformData {
    words: [f32; 16],
}

impl LayerUniformData {
    fn from_params(
        tr: &LayerTransform,
        kind: LayerInputKind,
        cs: ColorSpace,
        src_width: u32,
        src_height: u32,
    ) -> Self {
        let (sin_r, cos_r) = tr.rotation_rad.sin_cos();
        let m00 = cos_r * tr.scale_x;
        let m10 = sin_r * tr.scale_x;
        let m01 = -sin_r * tr.scale_y;
        let m11 = cos_r * tr.scale_y;

        Self {
            words: [
                m00,
                m10,
                0.0,
                0.0,
                m01,
                m11,
                0.0,
                0.0,
                tr.x,
                tr.y,
                tr.opacity.clamp(0.0, 1.0),
                0.0,
                (kind as u32) as f32,
                (cs as u32) as f32,
                src_width as f32,
                src_height as f32,
            ],
        }
    }

    fn to_bytes(self) -> [u8; 64] {
        let mut out = [0u8; 64];
        for (i, w) in self.words.iter().enumerate() {
            out[i * 4..(i + 1) * 4].copy_from_slice(&w.to_le_bytes());
        }
        out
    }
}

/// 初期化済み `wgpu` デバイスとキューの共有コンテキスト
pub struct GpuContext {
    pub instance: Instance,
    pub adapter_name: String,
    pub backend_name: String,
    pub device: Device,
    pub queue: Queue,
    pub limits: Limits,
}

impl GpuContext {
    /// ヘッドレスコンポジタ向けの `GpuContext` を生成する（Metal / Vulkan / GL へ自動フォールバック）
    pub fn new_headless() -> CoreResult<Self> {
        let instance = Instance::new(InstanceDescriptor::default());
        let adapter_opt = block_on(instance.request_adapter(&RequestAdapterOptions {
            power_preference: PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .or_else(|| {
            block_on(instance.request_adapter(&RequestAdapterOptions {
                power_preference: PowerPreference::LowPower,
                compatible_surface: None,
                force_fallback_adapter: true,
            }))
        });

        let adapter = adapter_opt.ok_or_else(|| {
            CoreError::PlatformIo("no compatible wgpu adapter found (Metal/Vulkan/GLES)".into())
        })?;
        let info = adapter.get_info();
        let supported_limits = adapter.limits();

        let (device, queue) = block_on(adapter.request_device(
            &DeviceDescriptor {
                label: Some("ikakim-gpu-device"),
                required_features: Features::empty(),
                required_limits: supported_limits.clone(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))
        .map_err(|e| CoreError::PlatformIo(format!("wgpu request_device failed: {}", e)))?;

        Ok(Self {
            instance,
            adapter_name: info.name,
            backend_name: format!("{:?}", info.backend),
            device,
            queue,
            limits: supported_limits,
        })
    }
}

/// GPU テクスチャへ保持された 1 レイヤー分の描画リソース
pub struct GpuLayer {
    pub layer_id: usize,
    pub is_base_layer: bool,
    pub kind: LayerInputKind,
    pub color_space: ColorSpace,
    pub src_width: u32,
    pub src_height: u32,
    pub transform: LayerTransform,
    _plane0: Texture,
    _plane1: Texture,
    view0: TextureView,
    view1: TextureView,
    uniform_buf: Buffer,
    bind_group: BindGroup,
}

impl GpuLayer {
    pub fn set_transform(&mut self, queue: &Queue, transform: LayerTransform) {
        self.transform = transform;
        let uni = LayerUniformData::from_params(
            &self.transform,
            self.kind,
            self.color_space,
            self.src_width,
            self.src_height,
        );
        queue.write_buffer(&self.uniform_buf, 0, &uni.to_bytes());
    }

    pub fn view0(&self) -> &TextureView {
        &self.view0
    }

    pub fn view1(&self) -> &TextureView {
        &self.view1
    }
}

/// オフスクリーンレンダーターゲット（`RenderSink` 実装）
pub struct OffscreenRenderSink {
    width: u32,
    height: u32,
    texture: Texture,
    view: TextureView,
}

impl OffscreenRenderSink {
    pub fn new(device: &Device, label: &str, width: u32, height: u32) -> Self {
        let texture = device.create_texture(&TextureDescriptor {
            label: Some(label),
            size: Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::Rgba8Unorm,
            usage: TextureUsages::RENDER_ATTACHMENT
                | TextureUsages::TEXTURE_BINDING
                | TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&TextureViewDescriptor::default());
        Self {
            width: width.max(1),
            height: height.max(1),
            texture,
            view,
        }
    }

    pub fn texture(&self) -> &Texture {
        &self.texture
    }

    pub fn view(&self) -> &TextureView {
        &self.view
    }
}

impl RenderSink for OffscreenRenderSink {
    type RenderTarget = TextureView;

    fn target_size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn acquire_target(&mut self) -> CoreResult<Self::RenderTarget> {
        Ok(self.texture.create_view(&TextureViewDescriptor::default()))
    }

    fn present_target(&mut self, _target: Self::RenderTarget, _pts: PtsUs) -> CoreResult<()> {
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct CompositorConfig {
    pub main_width: u32,
    pub main_height: u32,
    pub preview_width: u32,
    pub preview_height: u32,
    pub target_fps: u32,
}

impl Default for CompositorConfig {
    fn default() -> Self {
        Self {
            main_width: 1920,
            main_height: 1080,
            preview_width: 1280,
            preview_height: 720,
            target_fps: 60,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RenderStats {
    pub rendered_layers: usize,
    pub throttled_layers: usize,
    pub governor: GovernorStats,
}

/// マルチフォーマット対応 GPU コンポジタ
pub struct GpuCompositor {
    ctx: GpuContext,
    bind_group_layout: BindGroupLayout,
    pipeline: RenderPipeline,
    sampler: Sampler,
    dummy_tex: Texture,
    dummy_view: TextureView,
    main_sink: OffscreenRenderSink,
    preview_sink: OffscreenRenderSink,
    preview_blit_uniform: Buffer,
    preview_blit_bg: BindGroup,
    governor: OverlayGovernor,
}

impl GpuCompositor {
    pub fn new(ctx: GpuContext, config: CompositorConfig) -> Self {
        let shader = ctx.device.create_shader_module(ShaderModuleDescriptor {
            label: Some("ikakim-compositor-wgsl"),
            source: ShaderSource::Wgsl(COMPOSITOR_WGSL.into()),
        });

        let bind_group_layout = ctx
            .device
            .create_bind_group_layout(&BindGroupLayoutDescriptor {
                label: Some("ikakim-layer-bgl"),
                entries: &[
                    BindGroupLayoutEntry {
                        binding: 0,
                        visibility: ShaderStages::VERTEX_FRAGMENT,
                        ty: BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    BindGroupLayoutEntry {
                        binding: 1,
                        visibility: ShaderStages::FRAGMENT,
                        ty: BindingType::Texture {
                            sample_type: TextureSampleType::Float { filterable: true },
                            view_dimension: TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    BindGroupLayoutEntry {
                        binding: 2,
                        visibility: ShaderStages::FRAGMENT,
                        ty: BindingType::Texture {
                            sample_type: TextureSampleType::Float { filterable: true },
                            view_dimension: TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    BindGroupLayoutEntry {
                        binding: 3,
                        visibility: ShaderStages::FRAGMENT,
                        ty: BindingType::Sampler(SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });

        let pipeline_layout = ctx
            .device
            .create_pipeline_layout(&PipelineLayoutDescriptor {
                label: Some("ikakim-compositor-pl"),
                bind_group_layouts: &[&bind_group_layout],
                push_constant_ranges: &[],
            });

        let pipeline = ctx
            .device
            .create_render_pipeline(&RenderPipelineDescriptor {
                label: Some("ikakim-compositor-pipeline"),
                layout: Some(&pipeline_layout),
                vertex: VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(ColorTargetState {
                        format: TextureFormat::Rgba8Unorm,
                        blend: Some(BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                        write_mask: ColorWrites::ALL,
                    })],
                }),
                primitive: PrimitiveState {
                    topology: PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: MultisampleState::default(),
                multiview: None,
                cache: None,
            });

        let sampler = ctx.device.create_sampler(&SamplerDescriptor {
            label: Some("ikakim-linear-sampler"),
            address_mode_u: AddressMode::ClampToEdge,
            address_mode_v: AddressMode::ClampToEdge,
            address_mode_w: AddressMode::ClampToEdge,
            mag_filter: FilterMode::Linear,
            min_filter: FilterMode::Linear,
            ..Default::default()
        });

        let dummy_tex = ctx.device.create_texture(&TextureDescriptor {
            label: Some("ikakim-dummy-1x1"),
            size: Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::Rgba8Unorm,
            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let dummy_view = dummy_tex.create_view(&TextureViewDescriptor::default());

        let main_sink = OffscreenRenderSink::new(
            &ctx.device,
            "ikakim-main-rt",
            config.main_width,
            config.main_height,
        );
        let preview_sink = OffscreenRenderSink::new(
            &ctx.device,
            "ikakim-preview-rt",
            config.preview_width,
            config.preview_height,
        );

        let blit_uni = LayerUniformData::from_params(
            &LayerTransform::default(),
            LayerInputKind::Rgba,
            ColorSpace::Bt709Limited,
            config.main_width,
            config.main_height,
        );
        let preview_blit_uniform = Self::create_uniform_buffer(&ctx.device, &ctx.queue, blit_uni);
        let preview_blit_bg = Self::make_bind_group(
            &ctx.device,
            &bind_group_layout,
            &preview_blit_uniform,
            main_sink.view(),
            &dummy_view,
            &sampler,
        );

        let target_us = 1_000_000u64 / (config.target_fps.max(1) as u64);
        let governor = OverlayGovernor::from_limits(
            ctx.limits.max_sampled_textures_per_shader_stage,
            target_us,
        );

        Self {
            ctx,
            bind_group_layout,
            pipeline,
            sampler,
            dummy_tex,
            dummy_view,
            main_sink,
            preview_sink,
            preview_blit_uniform,
            preview_blit_bg,
            governor,
        }
    }

    pub fn context(&self) -> &GpuContext {
        &self.ctx
    }

    pub fn governor_mut(&mut self) -> &mut OverlayGovernor {
        &mut self.governor
    }

    pub fn main_texture(&self) -> &Texture {
        self.main_sink.texture()
    }

    pub fn preview_texture(&self) -> &Texture {
        self.preview_sink.texture()
    }

    pub fn main_size(&self) -> (u32, u32) {
        self.main_sink.target_size()
    }

    pub fn preview_size(&self) -> (u32, u32) {
        self.preview_sink.target_size()
    }

    /// ゼロコピー外部テクスチャビューから直接 `GpuLayer` を構築する（Issue #6 の Apple/Linux HAL 連携用）
    #[allow(clippy::too_many_arguments)]
    pub fn create_layer_from_textures(
        &self,
        layer_id: usize,
        is_base_layer: bool,
        kind: LayerInputKind,
        color_space: ColorSpace,
        src_width: u32,
        src_height: u32,
        transform: LayerTransform,
        plane0: Texture,
        plane1: Option<Texture>,
    ) -> GpuLayer {
        let view0 = plane0.create_view(&TextureViewDescriptor::default());
        let plane1_tex = plane1.unwrap_or_else(|| {
            self.ctx.device.create_texture(&TextureDescriptor {
                label: Some("ikakim-unused-plane1"),
                size: Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: TextureDimension::D2,
                format: TextureFormat::Rgba8Unorm,
                usage: TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
        });
        let view1 = plane1_tex.create_view(&TextureViewDescriptor::default());
        let uni = LayerUniformData::from_params(&transform, kind, color_space, src_width, src_height);
        let uniform_buf = Self::create_uniform_buffer(&self.ctx.device, &self.ctx.queue, uni);
        let bind_group = Self::make_bind_group(
            &self.ctx.device,
            &self.bind_group_layout,
            &uniform_buf,
            &view0,
            &view1,
            &self.sampler,
        );

        GpuLayer {
            layer_id,
            is_base_layer,
            kind,
            color_space,
            src_width,
            src_height,
            transform,
            _plane0: plane0,
            _plane1: plane1_tex,
            view0,
            view1,
            uniform_buf,
            bind_group,
        }
    }

    /// CPU RGBA バッファからレイヤー生成（静止画・Top-Bottom Alpha RGBA 用）
    #[allow(clippy::too_many_arguments)]
    pub fn create_rgba_layer(
        &self,
        layer_id: usize,
        is_base_layer: bool,
        top_bottom_alpha: bool,
        width: u32,
        height: u32,
        rgba_data: &[u8],
        transform: LayerTransform,
    ) -> CoreResult<GpuLayer> {
        let expected = (width as usize) * (height as usize) * 4;
        if rgba_data.len() < expected {
            return Err(CoreError::UnsupportedFormat("rgba buffer too small".into()));
        }
        let tex = self.upload_2d_texture(
            "ikakim-rgba-layer",
            width,
            height,
            TextureFormat::Rgba8Unorm,
            rgba_data,
            width * 4,
        );
        let kind = if top_bottom_alpha {
            LayerInputKind::TopBottomRgba
        } else {
            LayerInputKind::Rgba
        };
        Ok(self.create_layer_from_textures(
            layer_id,
            is_base_layer,
            kind,
            ColorSpace::Bt709Limited,
            width,
            height,
            transform,
            tex,
            None,
        ))
    }

    /// CPU NV12（Y: w*h, UV: w*h/2）から 2 プレーンテクスチャレイヤーを生成
    #[allow(clippy::too_many_arguments)]
    pub fn create_nv12_layer(
        &self,
        layer_id: usize,
        is_base_layer: bool,
        top_bottom_alpha: bool,
        color_space: ColorSpace,
        width: u32,
        height: u32,
        y_plane: &[u8],
        uv_plane: &[u8],
        transform: LayerTransform,
    ) -> CoreResult<GpuLayer> {
        if width < 2 || height < 2 || width % 2 != 0 || height % 2 != 0 {
            return Err(CoreError::UnsupportedFormat(
                "NV12 dimensions must be even and >= 2".into(),
            ));
        }
        let y_tex = self.upload_2d_texture(
            "ikakim-nv12-y",
            width,
            height,
            TextureFormat::R8Unorm,
            y_plane,
            width,
        );
        let uv_tex = self.upload_2d_texture(
            "ikakim-nv12-uv",
            width / 2,
            height / 2,
            TextureFormat::Rg8Unorm,
            uv_plane,
            width,
        );
        let kind = if top_bottom_alpha {
            LayerInputKind::TopBottomNv12
        } else {
            LayerInputKind::Nv12
        };
        Ok(self.create_layer_from_textures(
            layer_id,
            is_base_layer,
            kind,
            color_space,
            width,
            height,
            transform,
            y_tex,
            Some(uv_tex),
        ))
    }

    /// CPU YUYV（4:2:2 packed: 2*w*h bytes）から Rg8Unorm + Rgba8Unorm デュアルビューレイヤーを生成
    #[allow(clippy::too_many_arguments)]
    pub fn create_yuyv_layer(
        &self,
        layer_id: usize,
        is_base_layer: bool,
        color_space: ColorSpace,
        width: u32,
        height: u32,
        yuyv_data: &[u8],
        transform: LayerTransform,
    ) -> CoreResult<GpuLayer> {
        if width < 2 || width % 2 != 0 {
            return Err(CoreError::UnsupportedFormat(
                "YUYV width must be even and >= 2".into(),
            ));
        }
        let expected = (width as usize) * (height as usize) * 2;
        if yuyv_data.len() < expected {
            return Err(CoreError::UnsupportedFormat("YUYV buffer too small".into()));
        }
        // View 0: Rg8Unorm (w x h, R=Y, G=U or V)
        let luma_tex = self.upload_2d_texture(
            "ikakim-yuyv-rg8",
            width,
            height,
            TextureFormat::Rg8Unorm,
            yuyv_data,
            width * 2,
        );
        // View 1: Rgba8Unorm (w/2 x h, G=U, A=V)
        let chroma_tex = self.upload_2d_texture(
            "ikakim-yuyv-rgba8",
            width / 2,
            height,
            TextureFormat::Rgba8Unorm,
            yuyv_data,
            width * 2,
        );
        Ok(self.create_layer_from_textures(
            layer_id,
            is_base_layer,
            LayerInputKind::YuyvDual,
            color_space,
            width,
            height,
            transform,
            luma_tex,
            Some(chroma_tex),
        ))
    }

    /// Z 順ソート・負荷ガバナ制限・デュアルレンダーターゲット出力を実行する
    pub fn render_dual(&mut self, layers: &[&GpuLayer], pts: PtsUs) -> CoreResult<RenderStats> {
        let start = Instant::now();
        let main_view = self.main_sink.acquire_target()?;
        let preview_view = self.preview_sink.acquire_target()?;

        let mut visible: Vec<&GpuLayer> = layers
            .iter()
            .copied()
            .filter(|l| l.transform.visible && l.transform.opacity > 0.0)
            .collect();
        visible.sort_by_key(|l| (l.transform.z_order, l.layer_id));

        let cap = self.governor.current_cap();
        let mut active_video_overlays = 0usize;
        let mut selected: Vec<&GpuLayer> = Vec::with_capacity(visible.len());
        let mut throttled = 0usize;

        for layer in visible {
            if !layer.is_base_layer && layer.kind.is_video_overlay() {
                if active_video_overlays >= cap {
                    throttled += 1;
                    continue;
                }
                active_video_overlays += 1;
            }
            selected.push(layer);
        }
        if throttled > 0 {
            self.governor.record_throttled(throttled);
        }

        let mut encoder = self
            .ctx
            .device
            .create_command_encoder(&CommandEncoderDescriptor {
                label: Some("ikakim-dual-pass-encoder"),
            });

        // Pass 1: 本配信用（高解像度）コンポジットパス
        {
            let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("ikakim-main-pass"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &main_view,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Clear(Color::BLACK),
                        store: StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            for layer in &selected {
                pass.set_bind_group(0, &layer.bind_group, &[]);
                pass.draw(0..4, 0..1);
            }
        }

        // Pass 2: プレビュー用（低解像度）スケール出力パス
        Self::encode_blit_pass(
            &mut encoder,
            &self.pipeline,
            &self.preview_blit_bg,
            &preview_view,
        );

        let cmd = encoder.finish();
        self.ctx.queue.submit(std::iter::once(cmd));
        self.ctx.device.poll(Maintain::Wait);

        let _ = self.main_sink.present_target(main_view, pts);
        let _ = self.preview_sink.present_target(preview_view, pts);
        let _ = (&self.dummy_tex, &self.dummy_view, &self.preview_blit_uniform);

        let elapsed_us = start.elapsed().as_micros() as u64;
        self.governor.record_frame_time(elapsed_us);

        Ok(RenderStats {
            rendered_layers: selected.len(),
            throttled_layers: throttled,
            governor: self.governor.stats(),
        })
    }

    /// ヘッドレス検証および CPU フォールバック向けに、レンダーターゲットから RGBA バイト列を読み出す
    pub fn readback_texture_rgba(&self, texture: &Texture, width: u32, height: u32) -> CoreResult<Vec<u8>> {
        let bytes_per_pixel = 4u32;
        let unpadded_row = width * bytes_per_pixel;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_row = unpadded_row.div_ceil(align) * align;
        let buffer_size = (padded_row as u64) * (height as u64);

        let staging = self.ctx.device.create_buffer(&BufferDescriptor {
            label: Some("ikakim-readback-staging"),
            size: buffer_size,
            usage: BufferUsages::MAP_READ | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let mut encoder = self
            .ctx
            .device
            .create_command_encoder(&CommandEncoderDescriptor {
                label: Some("ikakim-readback-encoder"),
            });
        encoder.copy_texture_to_buffer(
            ImageCopyTexture {
                texture,
                mip_level: 0,
                origin: Origin3d::ZERO,
                aspect: TextureAspect::All,
            },
            ImageCopyBuffer {
                buffer: &staging,
                layout: ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row),
                    rows_per_image: Some(height),
                },
            },
            Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.ctx.queue.submit(std::iter::once(encoder.finish()));

        let slice = staging.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(MapMode::Read, move |res| {
            let _ = tx.send(res);
        });
        self.ctx.device.poll(Maintain::Wait);
        rx.recv()
            .map_err(|_| CoreError::PlatformIo("readback channel closed".into()))?
            .map_err(|e| CoreError::PlatformIo(format!("map_async failed: {:?}", e)))?;

        let mapped = slice.get_mapped_range();
        let mut out = Vec::with_capacity((unpadded_row * height) as usize);
        for row in 0..height as usize {
            let start = row * padded_row as usize;
            out.extend_from_slice(&mapped[start..start + unpadded_row as usize]);
        }
        drop(mapped);
        staging.unmap();
        Ok(out)
    }

    fn encode_blit_pass(
        encoder: &mut CommandEncoder,
        pipeline: &RenderPipeline,
        blit_bg: &BindGroup,
        target_view: &TextureView,
    ) {
        let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
            label: Some("ikakim-preview-blit-pass"),
            color_attachments: &[Some(RenderPassColorAttachment {
                view: target_view,
                resolve_target: None,
                ops: Operations {
                    load: LoadOp::Clear(Color::BLACK),
                    store: StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, blit_bg, &[]);
        pass.draw(0..4, 0..1);
    }

    fn upload_2d_texture(
        &self,
        label: &str,
        width: u32,
        height: u32,
        format: TextureFormat,
        data: &[u8],
        bytes_per_row: u32,
    ) -> Texture {
        let tex = self.ctx.device.create_texture(&TextureDescriptor {
            label: Some(label),
            size: Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format,
            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.ctx.queue.write_texture(
            ImageCopyTexture {
                texture: &tex,
                mip_level: 0,
                origin: Origin3d::ZERO,
                aspect: TextureAspect::All,
            },
            data,
            ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(height),
            },
            Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        tex
    }

    fn create_uniform_buffer(device: &Device, queue: &Queue, uni: LayerUniformData) -> Buffer {
        let buf = device.create_buffer(&BufferDescriptor {
            label: Some("ikakim-layer-uniform"),
            size: 64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&buf, 0, &uni.to_bytes());
        buf
    }

    fn make_bind_group(
        device: &Device,
        layout: &BindGroupLayout,
        uniform_buf: &Buffer,
        view0: &TextureView,
        view1: &TextureView,
        sampler: &Sampler,
    ) -> BindGroup {
        device.create_bind_group(&BindGroupDescriptor {
            label: Some("ikakim-layer-bg"),
            layout,
            entries: &[
                BindGroupEntry {
                    binding: 0,
                    resource: uniform_buf.as_entire_binding(),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: BindingResource::TextureView(view0),
                },
                BindGroupEntry {
                    binding: 2,
                    resource: BindingResource::TextureView(view1),
                },
                BindGroupEntry {
                    binding: 3,
                    resource: BindingResource::Sampler(sampler),
                },
            ],
        })
    }
}
