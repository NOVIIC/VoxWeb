//! VoxWeb 客户端入口（cdylib）。
//! 负责 wasm-bindgen 启动、主循环、AppState 状态机、输入、相机、UI。

pub mod app;
pub mod camera;
pub mod input;
pub mod interp;
pub mod physics;
pub mod prediction;
pub mod raycast;
pub mod storage;
pub mod ui;

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

use voxweb_render::device::{configure_surface, init_device};
use voxweb_render::graph::RenderGraph;
use voxweb_render::passes::opaque::OpaquePass;
use voxweb_render::Renderer;
// Phase 1: use wgpu dependency for rendering

/// 客户端全局状态（AppState 状态机 + 渲染器 + 输入等）。
/// 使用 Rc<RefCell<>> 实现内部可变性，单线程 WASM 环境无需 Mutex。
pub struct Client {
    pub app_state: app::AppState,
    pub renderer: Option<Renderer>,
    pub camera: camera::Camera,
    pub input: input::InputState,
}

impl Client {
    pub fn new() -> Self {
        Self {
            app_state: app::AppState::Loading,
            renderer: None,
            camera: camera::Camera::default(),
            input: input::InputState::default(),
        }
    }
}

/// wasm-bindgen 入口：在 WASM 加载后由 JS 调用。
#[wasm_bindgen(start)]
pub async fn start() -> Result<(), JsValue> {
    // 设置 panic hook → console
    console_error_panic_hook::set_once();

    // 初始化 tracing → console
    tracing_wasm::set_as_global_default();

    log::info!("VoxWeb 客户端启动...");

    // 获取 canvas 元素
    let window = web_sys::window().expect("无 window 对象");
    let document = window.document().expect("无 document 对象");
    let canvas = document
        .get_element_by_id("game")
        .expect("未找到 #game canvas")
        .dyn_into::<HtmlCanvasElement>()
        .expect("#game 不是 canvas");

    // 创建 wgpu 设备 + Surface
    let ctx = init_device(&canvas).await.map_err(|e| JsValue::from_str(&e))?;

    // 配置 Surface 初始尺寸
    let width = canvas.client_width() as u32;
    let height = canvas.client_height() as u32;
    configure_surface(&ctx.surface, &ctx.device, ctx.surface_format, width, height);

    // 构建 RenderGraph（Phase 0：仅 Opaque Pass 占位）
    let mut graph = RenderGraph::new();
    graph.add_pass(Box::new(OpaquePass::new()));

    // 构建渲染器
    let renderer = Renderer {
        device: ctx.device,
        queue: ctx.queue,
        surface: ctx.surface,
        config: voxweb_render::RendererConfig::default(),
        surface_format: ctx.surface_format,
    };

    let client = Rc::new(RefCell::new(Client {
        app_state: app::AppState::Lobby,
        renderer: Some(renderer),
        camera: camera::Camera::default(),
        input: input::InputState::default(),
    }));

    // 启动主循环（通过 requestAnimationFrame）
    run_loop(client);

    Ok(())
}

/// 主循环：由 requestAnimationFrame 驱动。
fn run_loop(_client: Rc<RefCell<Client>>) {
    // Phase 0: 静态清屏占位 → Phase 1 改完完整帧循环
    // 使用 wasm_bindgen_futures 或 Closure 注册 RAF 回调
}
