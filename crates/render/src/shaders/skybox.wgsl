struct Globals {
    inv_view_proj: mat4x4<f32>,
    sun_dir_time: vec4<f32>,
};

@group(0) @binding(0) var<uniform> g: Globals;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) ndc: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    var pos = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -3.0),
        vec2<f32>( 3.0,  1.0),
        vec2<f32>(-1.0,  1.0),
    );
    var out: VsOut;
    out.ndc = pos[vi];
    out.clip = vec4<f32>(pos[vi], 0.0, 1.0);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let near = g.inv_view_proj * vec4<f32>(in.ndc, 0.0, 1.0);
    let far = g.inv_view_proj * vec4<f32>(in.ndc, 1.0, 1.0);
    let ray = normalize((far.xyz / far.w) - (near.xyz / near.w));
    let t = clamp(ray.y * 0.5 + 0.5, 0.0, 1.0);
    let horizon = vec3<f32>(0.70, 0.84, 0.94);
    let zenith = vec3<f32>(0.20, 0.42, 0.78);
    var color = mix(horizon, zenith, smoothstep(0.05, 0.85, t));
    let sun_dir = normalize(g.sun_dir_time.xyz);
    let sun_dot = max(dot(ray, sun_dir), 0.0);
    let sun = pow(sun_dot, 256.0) * vec3<f32>(1.0, 0.86, 0.50);
    let glow = pow(sun_dot, 16.0) * vec3<f32>(0.35, 0.22, 0.08);
    color = color + sun + glow;
    return vec4<f32>(color, 1.0);
}
