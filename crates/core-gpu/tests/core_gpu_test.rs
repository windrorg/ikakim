use core_gpu::{
    ColorSpace, CompositorConfig, GpuCompositor, GpuContext, OverlayGovernor,
};
use core_types::clock::PtsUs;
use core_types::media::LayerTransform;

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
