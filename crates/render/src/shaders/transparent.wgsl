struct Globals {
    view_proj: mat4x4<f32>,
    chunk_origin: vec4<f32>,
};

@group(0) @binding(0) var<uniform> g: Globals;

struct VsIn {
    @location(0) packed: u32,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec3<f32>,
    @location(1) alpha: f32,
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
    out.color = block_color(tex) * face_brightness(face);
    out.alpha = block_alpha(tex);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(in.color, in.alpha);
}

fn block_color(tex: u32) -> vec3<f32> {
    switch tex {
        case 4u: { return vec3<f32>(0.18, 0.45, 0.85); }
        case 5u: { return vec3<f32>(0.82, 0.94, 1.00); }
        default: { return vec3<f32>(0.85, 0.30, 0.85); }
    }
}

fn block_alpha(tex: u32) -> f32 {
    switch tex {
        case 4u: { return 0.52; }
        case 5u: { return 0.38; }
        default: { return 0.55; }
    }
}

fn face_brightness(face: u32) -> f32 {
    switch face {
        case 2u: { return 1.00; }
        case 3u: { return 0.62; }
        case 0u: { return 0.88; }
        case 1u: { return 0.88; }
        case 4u: { return 0.78; }
        case 5u: { return 0.78; }
        default: { return 0.85; }
    }
}
