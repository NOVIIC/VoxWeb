// 不透明方块：压缩顶点解包 + 程序化纹理图集 + AO + 柔和距离雾。

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

struct SmoothVsIn {
    @location(0) local_pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) raw_uv: vec2<f32>,
    @location(3) tex_f: f32,
    @location(4) ao_f: f32,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) raw_uv: vec2<f32>,
    @location(1) world_pos: vec3<f32>,
    @location(2) ao: f32,
    @location(3) face_f: f32,
    @location(4) tex_f: f32,
    @location(5) normal: vec3<f32>,
};

@vertex
fn vs_main(in: VsIn) -> VsOut {
    let p = in.packed;
    let lx = f32(p & 0x1Fu);
    let ly = f32((p >> 5u) & 0x1FFu);
    let lz = f32((p >> 14u) & 0x1Fu);
    let face = (p >> 19u) & 0x7u;
    let tex = (p >> 22u) & 0xFFu;
    let ao_raw = (p >> 30u) & 0x3u;

    let world_pos = vec3<f32>(lx, ly, lz) + g.chunk_origin.xyz;

    var out: VsOut;
    out.clip = g.view_proj * vec4<f32>(world_pos, 1.0);
    out.raw_uv = face_uv(face, world_pos);
    out.world_pos = world_pos;
    out.ao = f32(ao_raw) / 3.0;
    out.face_f = f32(face);
    out.tex_f = f32(tex);
    out.normal = face_normal(face);
    return out;
}

@vertex
fn vs_smooth(in: SmoothVsIn) -> VsOut {
    let world_pos = in.local_pos + g.chunk_origin.xyz;

    var out: VsOut;
    out.clip = g.view_proj * vec4<f32>(world_pos, 1.0);
    out.raw_uv = in.raw_uv;
    out.world_pos = world_pos;
    out.ao = clamp(in.ao_f / 3.0, 0.0, 1.0);
    out.face_f = 2.0;
    out.tex_f = in.tex_f;
    out.normal = normalize(in.normal);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let face = u32(in.face_f + 0.5);
    let tex = u32(in.tex_f + 0.5);
    let sample = textureSample(atlas_tex, atlas_sampler, atlas_uv(tex, in.raw_uv)).rgb;
    let normal = normalize(in.normal);
    let sun = normalize(g.sun_dir.xyz);
    let direct = max(dot(normal, sun), 0.0);
    let slope_light = clamp(0.68 + 0.32 * normal.y, 0.58, 1.02);
    let face_light = face_brightness(face) * slope_light * (0.78 + 0.22 * direct);
    let ao_light = 0.58 + 0.42 * in.ao;
    var color = sample * face_light * ao_light;

    // 很轻的暖光，让自然柔和的色调不至于发灰。
    color = color * vec3<f32>(1.02, 1.00, 0.96) + vec3<f32>(0.015, 0.012, 0.008);
    color = tone_map(color);

    let dist = distance(g.camera_pos.xyz, in.world_pos);
    let fog = fog_factor(dist);
    color = mix(color, g.fog_color.rgb, fog);
    return vec4<f32>(color, 1.0);
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
        case 2u: { return 1.00; }
        case 3u: { return 0.58; }
        case 0u: { return 0.86; }
        case 1u: { return 0.82; }
        case 4u: { return 0.76; }
        case 5u: { return 0.72; }
        default: { return 0.84; }
    }
}

fn fog_factor(dist: f32) -> f32 {
    let start = g.fog_params.x;
    let end = max(g.fog_params.y, start + 1.0);
    return smoothstep(start, end, dist) * g.fog_params.z;
}

fn tone_map(color: vec3<f32>) -> vec3<f32> {
    let mapped = color / (color + vec3<f32>(0.24, 0.24, 0.24));
    return mix(color, mapped, 0.28);
}
