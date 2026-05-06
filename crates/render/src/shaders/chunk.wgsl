// VoxWeb 不透明方块着色器 (Phase 1)
//
// 顶点输入：单个 u32（PackedVertex），按位段解包：
//   bits  0..5  : lx       (5 bit)   方块角点 X
//   bits  5..14 : ly       (9 bit)   方块角点 Y
//   bits 14..19 : lz       (5 bit)   方块角点 Z
//   bits 19..22 : face_dir (3 bit)   面朝向
//   bits 22..30 : tex      (8 bit)   纹理图集索引（Phase 1 仅用作颜色查表）
//   bits 30..32 : ao       (2 bit)   AO 等级（Phase 7 启用）
//
// Phase 1 暂未引入纹理图集，颜色直接从 tex 索引派生（每个 BlockID 一种颜色）。
// 顶级面比侧面亮、底面最暗，模拟最朴素的方向光。

struct Globals {
    view_proj: mat4x4<f32>,
    chunk_origin: vec4<f32>,  // xyz + padding
};

@group(0) @binding(0) var<uniform> g: Globals;

struct VsIn {
    @location(0) packed: u32,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec3<f32>,
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
    let ao = f32(ao_raw) / 3.0;

    let local_pos = vec3<f32>(lx, ly, lz);
    let world_pos = local_pos + g.chunk_origin.xyz;

    var out: VsOut;
    out.clip = g.view_proj * vec4<f32>(world_pos, 1.0);

    let base = block_color(tex);
    let face_shade = face_brightness(face);
    // 0.55 ~ 1.0 之间的 AO 衰减（Phase 1 全 1.0）
    out.color = base * face_shade * (0.55 + 0.45 * ao);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(in.color, 1.0);
}

// 按 tex_index 查颜色（与 BlockProperties.texture_index 对齐）。
fn block_color(tex: u32) -> vec3<f32> {
    switch tex {
        case 1u: { return vec3<f32>(0.55, 0.55, 0.58); }  // STONE
        case 2u: { return vec3<f32>(0.36, 0.66, 0.30); }  // GRASS
        case 3u: { return vec3<f32>(0.55, 0.40, 0.27); }  // DIRT
        case 4u: { return vec3<f32>(0.20, 0.45, 0.85); }  // WATER
        case 5u: { return vec3<f32>(0.85, 0.92, 0.96); }  // GLASS
        case 6u: { return vec3<f32>(0.92, 0.85, 0.62); }  // SAND
        case 7u: { return vec3<f32>(0.50, 0.35, 0.20); }  // WOOD
        case 8u: { return vec3<f32>(0.30, 0.55, 0.25); }  // LEAVES
        default: { return vec3<f32>(0.85, 0.30, 0.85); }  // 未注册：洋红
    }
}

// 面朝向 → 亮度系数：模拟方向光自顶向下
fn face_brightness(face: u32) -> f32 {
    switch face {
        case 2u: { return 1.00; }  // PosY 顶面
        case 3u: { return 0.55; }  // NegY 底面
        case 0u: { return 0.85; }  // PosX
        case 1u: { return 0.85; }  // NegX
        case 4u: { return 0.72; }  // PosZ
        case 5u: { return 0.72; }  // NegZ
        default: { return 0.85; }
    }
}
