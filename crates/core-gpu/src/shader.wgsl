// Unified Video/Overlay Compositor WGSL Shader (Issue #5)
// Supports:
//   - RGBA / BGRA
//   - NV12 (Plane 0: R8Unorm Y, Plane 1: Rg8Unorm UV)
//   - YUYV Dual-View (View 0: Rg8Unorm luma, View 1: Rgba8Unorm packed chroma)
//   - Top-Bottom Alpha Video (Top 50% RGB, Bottom 50% Alpha) for RGBA & NV12
//   - BT.709 & BT.601 Limited/Full range matrices
//   - Dynamic 2D Affine Transform (position, scale, rotation, opacity)

struct LayerUniform {
    // 2D affine matrix columns + translation stored in vec4s:
    // col0 = (m00, m10, 0, 0), col1 = (m01, m11, 0, 0), trans = (tx, ty, opacity, 0)
    col0: vec4<f32>,
    col1: vec4<f32>,
    trans_opacity: vec4<f32>,
    // params.x = kind (0=Rgba, 1=Nv12, 2=YuyvDual, 3=TopBottomRgba, 4=TopBottomNv12)
    // params.y = color_space (0=Bt709Limited, 1=Bt601Limited, 2=Bt709Full, 3=Bt601Full)
    // params.z = src_width, params.w = src_height
    params: vec4<f32>,
};

@group(0) @binding(0)
var<uniform> u_layer: LayerUniform;

@group(0) @binding(1)
var tex_plane0: texture_2d<f32>;

@group(0) @binding(2)
var tex_plane1: texture_2d<f32>;

@group(0) @binding(3)
var tex_sampler: sampler;

struct VertexOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VertexOut {
    // Fullscreen quad triangle strip (0..4): (-1,-1), (1,-1), (-1,1), (1,1)
    var quad_pos = array<vec2<f32>, 4>(
        vec2<f32>(-1.0,  1.0),
        vec2<f32>( 1.0,  1.0),
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 1.0, -1.0),
    );
    var quad_uv = array<vec2<f32>, 4>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 1.0),
    );

    let p = quad_pos[vid];
    let transformed = u_layer.col0.xy * p.x + u_layer.col1.xy * p.y + u_layer.trans_opacity.xy;

    var out: VertexOut;
    out.pos = vec4<f32>(transformed, 0.0, 1.0);
    out.uv = quad_uv[vid];
    return out;
}

fn yuv_to_rgb(y_raw: f32, u_raw: f32, v_raw: f32, cs: u32) -> vec3<f32> {
    let is_limited = (cs == 0u || cs == 1u);
    let y = select(y_raw, (y_raw - (16.0 / 255.0)) * (255.0 / 219.0), is_limited);
    let cb = select(u_raw - (128.0 / 255.0), (u_raw - (128.0 / 255.0)) * (255.0 / 224.0), is_limited);
    let cr = select(v_raw - (128.0 / 255.0), (v_raw - (128.0 / 255.0)) * (255.0 / 224.0), is_limited);

    let is_bt709 = (cs == 0u || cs == 2u);
    let rgb709 = vec3<f32>(
        y + 1.5748 * cr,
        y - 0.187324 * cb - 0.468124 * cr,
        y + 1.8556 * cb
    );
    let rgb601 = vec3<f32>(
        y + 1.402 * cr,
        y - 0.344136 * cb - 0.714136 * cr,
        y + 1.772 * cb
    );
    return clamp(select(rgb601, rgb709, is_bt709), vec3<f32>(0.0), vec3<f32>(1.0));
}

fn sample_nv12(uv: vec2<f32>, cs: u32) -> vec3<f32> {
    let y = textureSampleLevel(tex_plane0, tex_sampler, uv, 0.0).r;
    let uvv = textureSampleLevel(tex_plane1, tex_sampler, uv, 0.0).rg;
    return yuv_to_rgb(y, uvv.x, uvv.y, cs);
}

fn sample_yuyv_dual(uv: vec2<f32>, cs: u32) -> vec3<f32> {
    // plane0 = Rg8Unorm (w x h): .r is exact pixel Y
    // plane1 = Rgba8Unorm (w/2 x h): .g is U, .a is V for the macropixel pair
    let y = textureSampleLevel(tex_plane0, tex_sampler, uv, 0.0).r;
    let chroma = textureSampleLevel(tex_plane1, tex_sampler, uv, 0.0);
    return yuv_to_rgb(y, chroma.g, chroma.a, cs);
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let kind = u32(u_layer.params.x + 0.5);
    let cs = u32(u_layer.params.y + 0.5);
    let opacity = clamp(u_layer.trans_opacity.z, 0.0, 1.0);
    let h = max(u_layer.params.w, 2.0);
    let half_texel_y = 0.5 / h;

    let is_tb = (kind >= 3u);
    let uv_tb_rgb = vec2<f32>(in.uv.x, clamp(in.uv.y * 0.5, 0.0, 0.5 - half_texel_y));
    let uv_rgb = select(in.uv, uv_tb_rgb, is_tb);
    let uv_alpha = vec2<f32>(in.uv.x, clamp(0.5 + in.uv.y * 0.5, 0.5 + half_texel_y, 1.0));

    let p0_rgb = textureSampleLevel(tex_plane0, tex_sampler, uv_rgb, 0.0);
    let p1_rgb = textureSampleLevel(tex_plane1, tex_sampler, uv_rgb, 0.0);
    let p0_alpha = textureSampleLevel(tex_plane0, tex_sampler, uv_alpha, 0.0);

    let is_yuyv = (kind == 2u);
    let yuv_u = select(p1_rgb.r, p1_rgb.g, is_yuyv);
    let yuv_v = select(p1_rgb.g, p1_rgb.a, is_yuyv);
    let yuv_rgb = yuv_to_rgb(p0_rgb.r, yuv_u, yuv_v, cs);

    let is_rgba_kind = (kind == 0u || kind == 3u);
    let base_rgb = select(yuv_rgb, p0_rgb.rgb, is_rgba_kind);

    let mask_limited = clamp((p0_alpha.r - (16.0 / 255.0)) * (255.0 / 219.0), 0.0, 1.0);
    let mask_full = clamp(p0_alpha.r, 0.0, 1.0);
    let nv12_mask_a = select(mask_full, mask_limited, cs == 0u || cs == 1u);
    let tb_alpha = select(nv12_mask_a, mask_full, kind == 3u);

    let raw_alpha = select(select(1.0, p0_rgb.a, kind == 0u), tb_alpha, is_tb);
    let a = raw_alpha * opacity;
    return vec4<f32>(base_rgb * a, a);
}
