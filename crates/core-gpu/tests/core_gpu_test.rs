use core_gpu::{
    ColorSpace, CompositorConfig, ExternalRenderFormat, ExternalRenderTarget, GpuCompositor,
    GpuContext, OverlayGovernor,
};
use core_types::clock::PtsUs;
use core_types::media::LayerTransform;
use std::time::Instant;

#[test]
fn test_nv12_and_yuyv_bt709_bt601_shader_conversion() {
    let ctx = GpuContext::new_headless().expect("wgpu headless adapter required");
    println!(
        "core-gpu adapter: {} ({})",
        ctx.adapter_name, ctx.backend_name
    );

    let mut comp = GpuCompositor::new(
        ctx,
        CompositorConfig {
            main_width: 16,
            main_height: 16,
            preview_width: 8,
            preview_height: 8,
            target_fps: 60,
        },
    );

    // 1. NV12 純赤フレーム (BT.709 Limited: Y=63, U=102, V=240)
    let w = 8u32;
    let h = 8u32;
    let y_plane = vec![63u8; (w * h) as usize];
    let mut uv_plane = Vec::with_capacity(((w / 2) * (h / 2) * 2) as usize);
    for _ in 0..((w / 2) * (h / 2)) {
        uv_plane.push(102);
        uv_plane.push(240);
    }

    let nv12_layer = comp
        .create_nv12_layer(
            0,
            true,
            false,
            ColorSpace::Bt709Limited,
            w,
            h,
            &y_plane,
            &uv_plane,
            LayerTransform::default(),
        )
        .unwrap();

    comp.render_dual(&[&nv12_layer], PtsUs(0)).unwrap();
    let main_px = comp
        .readback_texture_rgba(comp.main_texture(), 16, 16)
        .unwrap();
    let center_idx = (8 * 16 + 8) * 4;
    assert!(main_px[center_idx] >= 245, "R={}", main_px[center_idx]);
    assert!(main_px[center_idx + 1] <= 10, "G={}", main_px[center_idx + 1]);
    assert!(main_px[center_idx + 2] <= 10, "B={}", main_px[center_idx + 2]);

    // プレビューレンダーターゲット (8x8) にも同一色がダウンサンプル出力されていること
    let prev_px = comp
        .readback_texture_rgba(comp.preview_texture(), 8, 8)
        .unwrap();
    let p_idx = (4 * 8 + 4) * 4;
    assert!(prev_px[p_idx] >= 245, "preview R={}", prev_px[p_idx]);

    // 2. YUYV デュアルビュー (Rg8Unorm + Rgba8Unorm) 純緑フレーム (BT.601 Limited: Y=150, U=44, V=21)
    let mut yuyv = Vec::with_capacity((w * h * 2) as usize);
    for _ in 0..((w / 2) * h) {
        yuyv.extend_from_slice(&[150, 44, 150, 21]);
    }
    let yuyv_layer = comp
        .create_yuyv_layer(
            1,
            true,
            ColorSpace::Bt601Limited,
            w,
            h,
            &yuyv,
            LayerTransform::default(),
        )
        .unwrap();

    comp.render_dual(&[&yuyv_layer], PtsUs(16_667)).unwrap();
    let green_px = comp
        .readback_texture_rgba(comp.main_texture(), 16, 16)
        .unwrap();
    assert!(green_px[center_idx] <= 10, "R={}", green_px[center_idx]);
    assert!(green_px[center_idx + 1] >= 245, "G={}", green_px[center_idx + 1]);
    assert!(green_px[center_idx + 2] <= 10, "B={}", green_px[center_idx + 2]);
}

#[test]
fn test_top_bottom_alpha_compositing_and_dynamic_transform_z_order() {
    let ctx = GpuContext::new_headless().expect("wgpu headless adapter required");
    let mut comp = GpuCompositor::new(
        ctx,
        CompositorConfig {
            main_width: 16,
            main_height: 16,
            preview_width: 8,
            preview_height: 8,
            target_fps: 60,
        },
    );

    // Layer 0 (z=0): 全面 青 (0, 0, 255, 255)
    let mut blue = Vec::with_capacity(4 * 4 * 4);
    for _ in 0..16 {
        blue.extend_from_slice(&[0, 0, 255, 255]);
    }
    let l0 = comp
        .create_rgba_layer(0, true, false, 4, 4, &blue, LayerTransform::default())
        .unwrap();

    // Layer 1 (z=1): Top-Bottom Alpha (8x8: 上半分 4行=純赤, 下半分 4行=約50%アルファ (128))
    let mut tb = Vec::with_capacity(8 * 8 * 4);
    for row in 0..8 {
        for _col in 0..8 {
            if row < 4 {
                tb.extend_from_slice(&[255, 0, 0, 255]);
            } else {
                tb.extend_from_slice(&[128, 128, 128, 255]);
            }
        }
    }
    let l1 = comp
        .create_rgba_layer(
            1,
            false,
            true,
            8,
            8,
            &tb,
            LayerTransform {
                z_order: 1,
                ..Default::default()
            },
        )
        .unwrap();

    // Layer 2 (z=2): 緑 (0, 255, 0, 255) を右上象限 (x=0.5, y=0.5, scale=0.5) に配置
    let mut green = Vec::with_capacity(4 * 4 * 4);
    for _ in 0..16 {
        green.extend_from_slice(&[0, 255, 0, 255]);
    }
    let mut l2 = comp
        .create_rgba_layer(
            2,
            false,
            false,
            4,
            4,
            &green,
            LayerTransform {
                x: 0.5,
                y: 0.5,
                scale_x: 0.5,
                scale_y: 0.5,
                z_order: 2,
                ..Default::default()
            },
        )
        .unwrap();

    // Z 順あえて逆順配列で渡し、コンポジタ側が z_order 昇順で描画することを確認
    comp.render_dual(&[&l2, &l1, &l0], PtsUs(0)).unwrap();
    let px = comp
        .readback_texture_rgba(comp.main_texture(), 16, 16)
        .unwrap();

    // 左下象限 (x_px=3, y_px=12): 赤50% + 青背景 => (~128, 0, ~127)
    let bl = (12 * 16 + 3) * 4;
    assert!(
        (115..=142).contains(&px[bl]) && px[bl + 1] <= 5 && (115..=142).contains(&px[bl + 2]),
        "bottom-left unexpected RGBA={:?}",
        &px[bl..bl + 4]
    );

    // 右上象限 (x_px=12, y_px=3): Layer 2 の緑が最前面
    let tr = (3 * 16 + 12) * 4;
    assert!(
        px[tr] <= 10 && px[tr + 1] >= 245 && px[tr + 2] <= 10,
        "top-right expected green, got RGBA={:?}",
        &px[tr..tr + 4]
    );

    // 動的トランスフォーム: Layer 2 を非表示にすると右上象限も Red+Blue ブレンドに戻る
    l2.set_transform(
        &comp.context().queue,
        LayerTransform {
            visible: false,
            ..l2.transform
        },
    );
    comp.render_dual(&[&l2, &l1, &l0], PtsUs(16_667)).unwrap();
    let px2 = comp
        .readback_texture_rgba(comp.main_texture(), 16, 16)
        .unwrap();
    assert!(
        (115..=142).contains(&px2[tr]) && px2[tr + 1] <= 5 && (115..=142).contains(&px2[tr + 2]),
        "top-right after hide unexpected RGBA={:?}",
        &px2[tr..tr + 4]
    );
}

#[test]
fn test_overlay_governor_limits_and_throttling() {
    let mut gov = OverlayGovernor::from_limits(16, 16_666);
    assert_eq!(gov.hardware_cap(), 8);
    assert_eq!(gov.current_cap(), 8);

    // 予算の 90% 超が 3 回連続で続くと上限が 8 -> 7 へ自動低下
    gov.record_frame_time(20_000);
    gov.record_frame_time(20_000);
    gov.record_frame_time(20_000);
    assert_eq!(gov.current_cap(), 7);

    // 実レンダリング上でも Limits 由来の動画オーバーレイ制限が機能することを確認
    let ctx = GpuContext::new_headless().expect("wgpu headless adapter required");
    let mut comp = GpuCompositor::new(
        ctx,
        CompositorConfig {
            main_width: 8,
            main_height: 8,
            preview_width: 4,
            preview_height: 4,
            target_fps: 60,
        },
    );
    // ガバナ上限を強制的に 1 に落とし、Top-Bottom Alpha 動画レイヤーを 3 つ投入
    while comp.governor_mut().current_cap() > 1 {
        comp.governor_mut().record_frame_time(50_000);
    }
    let tb = vec![128u8; 4 * 4 * 4];
    let ov1 = comp
        .create_rgba_layer(1, false, true, 4, 4, &tb, LayerTransform::default())
        .unwrap();
    let ov2 = comp
        .create_rgba_layer(2, false, true, 4, 4, &tb, LayerTransform::default())
        .unwrap();
    let ov3 = comp
        .create_rgba_layer(3, false, true, 4, 4, &tb, LayerTransform::default())
        .unwrap();

    let stats = comp.render_dual(&[&ov1, &ov2, &ov3], PtsUs(0)).unwrap();
    assert_eq!(stats.rendered_layers, 1);
    assert_eq!(stats.throttled_layers, 2);
}

#[test]
fn test_gpu_compositor_1080p60_external_bgra_zero_readback_targets() {
    let ctx = GpuContext::new_headless().expect("wgpu headless adapter required");
    let adapter_lc = ctx.adapter_name.to_lowercase();
    let is_sw_rasterizer = adapter_lc.contains("llvmpipe")
        || adapter_lc.contains("swiftshader")
        || adapter_lc.contains("basic render");

    let mw = 1920u32;
    let mh = 1080u32;
    let pw = 960u32;
    let ph = 540u32;
    let mut comp = GpuCompositor::new(
        ctx,
        CompositorConfig {
            main_width: mw,
            main_height: mh,
            preview_width: pw,
            preview_height: ph,
            target_fps: 60,
        },
    );

    let make_ext_bgra_tex = |w: u32, h: u32, label: &str| {
        comp.context().device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        })
    };

    let ext_main_tex = make_ext_bgra_tex(mw, mh, "ext-1080p-bgra");
    let ext_prev_tex = make_ext_bgra_tex(pw, ph, "ext-540p-bgra");
    let ext_main_view = ext_main_tex.create_view(&wgpu::TextureViewDescriptor::default());
    let ext_prev_view = ext_prev_tex.create_view(&wgpu::TextureViewDescriptor::default());

    // 1080p NV12 Base + Top-Bottom Alpha Overlay
    let y = vec![150u8; (mw * mh) as usize];
    let uv = vec![90u8; (mw * mh / 2) as usize];
    let base_layer = comp
        .create_nv12_layer(
            0,
            true,
            false,
            ColorSpace::Bt709Limited,
            mw,
            mh,
            &y,
            &uv,
            LayerTransform::default(),
        )
        .unwrap();

    // Small 64x64 top-bottom alpha RGBA overlay positioned in Top-Right quadrant
    let mut tb_overlay = vec![0u8; 64 * 128 * 4];
    for px in tb_overlay[..64 * 64 * 4].chunks_exact_mut(4) {
        px[0] = 240; // R
        px[1] = 20;
        px[2] = 20;
        px[3] = 255;
    }
    for px in tb_overlay[64 * 64 * 4..].chunks_exact_mut(4) {
        px[0] = 255;
        px[1] = 255;
        px[2] = 255;
        px[3] = 255;
    }
    let ov_layer = comp
        .create_rgba_layer(
            1,
            false,
            true,
            64,
            128,
            &tb_overlay,
            LayerTransform {
                x: 0.5,
                y: 0.5,
                scale_x: 0.25,
                scale_y: 0.25,
                z_order: 1,
                visible: true,
                ..Default::default()
            },
        )
        .unwrap();

    // Warmup + measure steady-state 1080p60 render_dual_into (skipped on CPU software rasterizers)
    let measure_frames = if is_sw_rasterizer { 4u64 } else { 30u64 };
    for w in 0..3u64 {
        comp.render_dual_into(
            &[&base_layer, &ov_layer],
            PtsUs(w * 16_667),
            Some(ExternalRenderTarget {
                view: &ext_main_view,
                format: ExternalRenderFormat::Bgra8Unorm,
                readable_for_preview_blit: true,
            }),
            Some(ExternalRenderTarget {
                view: &ext_prev_view,
                format: ExternalRenderFormat::Bgra8Unorm,
                readable_for_preview_blit: false,
            }),
        )
        .unwrap();
    }

    let t0 = Instant::now();
    for i in 0..measure_frames {
        let stats = comp
            .render_dual_into(
                &[&base_layer, &ov_layer],
                PtsUs((i + 3) * 16_667),
                Some(ExternalRenderTarget {
                    view: &ext_main_view,
                    format: ExternalRenderFormat::Bgra8Unorm,
                    readable_for_preview_blit: true,
                }),
                Some(ExternalRenderTarget {
                    view: &ext_prev_view,
                    format: ExternalRenderFormat::Bgra8Unorm,
                    readable_for_preview_blit: false,
                }),
            )
            .unwrap();
        assert_eq!(stats.rendered_layers, 2);
        assert_eq!(stats.throttled_layers, 0);
    }
    let avg_ms = t0.elapsed().as_secs_f64() * 1000.0 / measure_frames as f64;
    if !is_sw_rasterizer {
        assert!(
            avg_ms < 12.0,
            "1080p60 GPU external render avg {:.2}ms exceeds 12ms budget (60fps=16.67ms)",
            avg_ms
        );
    }

    // Verify rendered BGRA bytes directly on external targets (BGRA layout: [B, G, R, A])
    let main_bgra = comp.readback_texture_rgba(&ext_main_tex, mw, mh).unwrap();
    let prev_bgra = comp.readback_texture_rgba(&ext_prev_tex, pw, ph).unwrap();

    // Top-right quadrant overlay (x=1440, y=270 -> NDC 0.5, 0.5) should have Red dominant (BGRA R is byte +2)
    let tr_main = ((270 * mw + 1440) * 4) as usize;
    assert!(
        main_bgra[tr_main + 2] > 200 && main_bgra[tr_main] < 50,
        "1080p main BGRA top-right overlay expected R>200, B<50, got BGRA={:?}",
        &main_bgra[tr_main..tr_main + 4]
    );
    let tr_prev = ((135 * pw + 720) * 4) as usize;
    assert!(
        prev_bgra[tr_prev + 2] > 200 && prev_bgra[tr_prev] < 50,
        "540p preview BGRA top-right blit expected R>200, B<50, got BGRA={:?}",
        &prev_bgra[tr_prev..tr_prev + 4]
    );
}
