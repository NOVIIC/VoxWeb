struct Globals {
    inv_view_proj: mat4x4<f32>,
    sun_dir_time: vec4<f32>,
    fog_color: vec4<f32>,
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

    let height = clamp(ray.y * 0.5 + 0.5, 0.0, 1.0);
    let horizon_blend = smoothstep(0.0, 0.72, height);
    let zenith = vec3<f32>(0.34, 0.55, 0.78);
    let high = vec3<f32>(0.52, 0.69, 0.84);
    let horizon = mix(g.fog_color.rgb, vec3<f32>(0.86, 0.82, 0.72), 0.28);
    var color = mix(horizon, mix(high, zenith, height), horizon_blend);

    let sun_dir = normalize(g.sun_dir_time.xyz);
    let sun_dot = max(dot(ray, sun_dir), 0.0);
    let sun_core = pow(sun_dot, 520.0) * vec3<f32>(1.0, 0.88, 0.58);
    let sun_glow = pow(sun_dot, 18.0) * vec3<f32>(0.42, 0.28, 0.12);
    let broad_haze = pow(sun_dot, 3.0) * vec3<f32>(0.08, 0.065, 0.04);
    color = color + sun_core + sun_glow + broad_haze;

    let horizon_haze = 1.0 - smoothstep(0.08, 0.42, abs(ray.y));
    color = mix(color, g.fog_color.rgb, horizon_haze * 0.34);
    color = color / (color + vec3<f32>(0.18, 0.18, 0.18));
    return vec4<f32>(color, 1.0);
}
