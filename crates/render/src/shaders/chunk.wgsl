// VoxWeb 顶点着色器（占位）
// Phase 1 填充：顶点压缩解码 + MVP 变换

struct VertexInput {
    @location(0) packed: u32,
}

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) tex_coord: vec2<f32>,
    @location(1) ao: f32,
}

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.position = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    out.tex_coord = vec2<f32>(0.0, 0.0);
    out.ao = 0.0;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return vec4<f32>(1.0, 1.0, 1.0, 1.0);
}
