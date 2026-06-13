// 半透明方块：水 / 玻璃使用同一程序化图集，并与世界雾色轻柔混合。

struct Globals {
    view_proj: mat4x4<f32>,
    chunk_origin: vec4<f32>,
    camera_pos: vec4<f32>,
    fog_color: vec4<f32>,
    fog_params: vec4<f32>,
    sun_dir: vec4<f32>,
};

@group(0) @binding(0) var<uniform> g: Globals;
@group(1) @binding(0) var atlas_tex: texture_2d<f32>;
@group(1) @binding(1) var atlas_sampler: sampler;

struct VsIn {
    @location(0) packed: u32,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) raw_uv: vec2<f32>,
    @location(1) world_pos: vec3<f32>,
    @location(2) face_f: f32,
    @location(3) tex_f: f32,
};

@vertex
fn vs_main(in: VsIn) -> VsOut {
    let p = in.packed;
    let lx = f32(p & 0x1Fu);
    let ly = f32((p >> 5u) & 0x1FFu);
    let lz = f32((p >> 14u) & 0x1Fu);
    let face = (p >> 19u) & 0x7u;
    let tex = (p >> 22u) & 0xFFu;
    let world_pos = vec3<f32>(lx, ly, lz) + g.chunk_origin.xyz;

    var out: VsOut;
    out.clip = g.view_proj * vec4<f32>(world_pos, 1.0);
    out.raw_uv = face_uv(face, world_pos);
    out.world_pos = world_pos;
    out.face_f = f32(face);
    out.tex_f = f32(tex);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let face = u32(in.face_f + 0.5);
    let tex = u32(in.tex_f + 0.5);
    var uv = in.raw_uv;
    if tex == 4u {
        let t = g.fog_params.w;
        uv = uv + vec2<f32>(0.05 * sin(t * 0.7 + in.raw_uv.y), 0.04 * cos(t * 0.6 + in.raw_uv.x));
    }

    let sample = textureSample(atlas_tex, atlas_sampler, atlas_uv(tex, uv)).rgb;
    let normal = face_normal(face);
    let sun = max(dot(normal, normalize(g.sun_dir.xyz)), 0.0);
    var color = sample * face_brightness(face) * (0.86 + 0.14 * sun);
    var alpha = block_alpha(tex);

    if tex == 4u {
        let shimmer = 0.04 * sin((in.raw_uv.x + in.raw_uv.y) * 5.2 + g.fog_params.w * 1.4);
        color = color + vec3<f32>(0.02, 0.045, 0.055) + vec3<f32>(shimmer);
        alpha = alpha + shimmer * 0.25;
    } else if tex == 5u {
        color = mix(color, vec3<f32>(0.92, 0.98, 1.0), 0.18);
    }

    let dist = distance(g.camera_pos.xyz, in.world_pos);
    let fog = fog_factor(dist) * 0.78;
    color = mix(tone_map(color), g.fog_color.rgb, fog);
    alpha = clamp(alpha, 0.18, 0.62);
    return vec4<f32>(color, alpha);
}

fn face_uv(face: u32, world_pos: vec3<f32>) -> vec2<f32> {
    switch face {
        case 0u, 1u: { return vec2<f32>(world_pos.z, world_pos.y); }
        case 2u, 3u: { return vec2<f32>(world_pos.x, world_pos.z); }
        default: { return vec2<f32>(world_pos.x, world_pos.y); }
    }
}

fn atlas_uv(tex: u32, raw_uv: vec2<f32>) -> vec2<f32> {
    let columns = 4u;
    let tile_px = 32.0;
    let atlas_px = vec2<f32>(128.0, 128.0);
    let col = f32(tex % columns);
    let row = f32(tex / columns);
    let local = fract(raw_uv) * 30.0 + vec2<f32>(1.0, 1.0);
    return (vec2<f32>(col, row) * tile_px + local) / atlas_px;
}

fn block_alpha(tex: u32) -> f32 {
    switch tex {
        case 4u: { return 0.46; }
        case 5u: { return 0.34; }
        default: { return 0.42; }
    }
}

fn face_normal(face: u32) -> vec3<f32> {
    switch face {
        case 0u: { return vec3<f32>(1.0, 0.0, 0.0); }
        case 1u: { return vec3<f32>(-1.0, 0.0, 0.0); }
        case 2u: { return vec3<f32>(0.0, 1.0, 0.0); }
        case 3u: { return vec3<f32>(0.0, -1.0, 0.0); }
        case 4u: { return vec3<f32>(0.0, 0.0, 1.0); }
        default: { return vec3<f32>(0.0, 0.0, -1.0); }
    }
}

fn face_brightness(face: u32) -> f32 {
    switch face {
        case 2u: { return 1.0; }
        case 3u: { return 0.66; }
        case 0u, 1u: { return 0.88; }
        default: { return 0.78; }
    }
}

fn fog_factor(dist: f32) -> f32 {
    let start = g.fog_params.x;
    let end = max(g.fog_params.y, start + 1.0);
    return smoothstep(start, end, dist) * g.fog_params.z;
}

fn tone_map(color: vec3<f32>) -> vec3<f32> {
    let mapped = color / (color + vec3<f32>(0.24, 0.24, 0.24));
    return mix(color, mapped, 0.22);
}
