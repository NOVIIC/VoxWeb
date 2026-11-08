//! WGSL 静态校验：不需要浏览器/GPU，也能提前发现纹理维度和采样参数错误。

fn validate_wgsl(source: &str) {
    let module = naga::front::wgsl::parse_str(source).expect("WGSL 应能解析");
    let mut validator = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    );
    validator.validate(&module).expect("WGSL 应通过校验");
}

#[test]
fn chunk_shader_is_valid() {
    validate_wgsl(include_str!("shaders/chunk.wgsl"));
}

#[test]
fn transparent_shader_is_valid() {
    validate_wgsl(include_str!("shaders/transparent.wgsl"));
}

#[test]
fn skybox_shader_is_valid() {
    validate_wgsl(include_str!("shaders/skybox.wgsl"));
}

#[test]
fn selection_shader_is_valid() {
    validate_wgsl(include_str!("shaders/selection.wgsl"));
}
