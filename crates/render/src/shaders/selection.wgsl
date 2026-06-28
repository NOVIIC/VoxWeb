// VoxWeb 选中方块线框着色器 (Phase 3)
//
// 顶点输入：单个 vec3<f32>（单位立方体角点，范围 [0, 1]）。
// Uniform：view_proj + box_min + box_size。
//
// 顶点位置 = box_min + corner * box_size，直接 transform 到 clip 空间。
// 片元一律输出半透明黑色。

struct Globals {
    view_proj: mat4x4<f32>,
    box_min: vec4<f32>,   // xyz + padding
    box_size: vec4<f32>,  // xyz + padding
};

@group(0) @binding(0) var<uniform> g: Globals;

struct VsIn {
    @location(0) corner: vec3<f32>,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
};

@vertex
fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    let world_pos = g.box_min.xyz + in.corner * g.box_size.xyz;
    out.clip = g.view_proj * vec4<f32>(world_pos, 1.0);
    return out;
}

@fragment
fn fs_main(_in: VsOut) -> @location(0) vec4<f32> {
    // 半透明黑边，alpha blend 在 pipeline 里配置
    return vec4<f32>(0.0, 0.0, 0.0, 0.85);
}
