//! 程序化材质纹理与 mipmap 纹理数组。
//!
//! 当前没有外部美术资源，启动时由 Rust 生成 16 张 32×32 RGBA8 tile。
//! 每种材质占一个 array layer，避免图集采样串色；CPU 生成完整 mip 链。

pub const TILE_SIZE: u32 = 32;
pub const ATLAS_COLUMNS: u32 = 4;
pub const ATLAS_ROWS: u32 = 4;
pub const ATLAS_WIDTH: u32 = TILE_SIZE * ATLAS_COLUMNS;
pub const ATLAS_HEIGHT: u32 = TILE_SIZE * ATLAS_ROWS;
pub const TILE_COUNT: u32 = ATLAS_COLUMNS * ATLAS_ROWS;
pub const MIP_LEVEL_COUNT: u32 = TILE_SIZE.ilog2() + 1;

/// 纹理图集 GPU 资源。
pub struct TextureAtlas {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub sampler: wgpu::Sampler,
    pub bind_group_layout: wgpu::BindGroupLayout,
    pub bind_group: wgpu::BindGroup,
}

impl TextureAtlas {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("texture_atlas.texture"),
            size: wgpu::Extent3d {
                width: TILE_SIZE,
                height: TILE_SIZE,
                depth_or_array_layers: TILE_COUNT,
            },
            mip_level_count: MIP_LEVEL_COUNT,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let atlas = generate_atlas_rgba();
        // 每种材质是独立 array layer；mipmap 只在同一 tile 内生成，不会把相邻
        // 材质平均进去，也不需要 shader 为图集边缘预留像素。
        for layer in 0..TILE_COUNT {
            let mut tile = extract_tile(&atlas, layer);
            for mip in 0..MIP_LEVEL_COUNT {
                let size = (TILE_SIZE >> mip).max(1);
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &texture,
                        mip_level: mip,
                        origin: wgpu::Origin3d {
                            x: 0,
                            y: 0,
                            z: layer,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    &tile,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(size * 4),
                        rows_per_image: Some(size),
                    },
                    wgpu::Extent3d {
                        width: size,
                        height: size,
                        depth_or_array_layers: 1,
                    },
                );
                if mip + 1 < MIP_LEVEL_COUNT {
                    tile = downsample_rgba(&tile, size, size);
                }
            }
        }
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("texture_atlas.array_view"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            mip_level_count: Some(MIP_LEVEL_COUNT),
            array_layer_count: Some(TILE_COUNT),
            ..Default::default()
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("texture_atlas.sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            // 近处保留像素材质轮廓，远处用 mipmap 抑制高频噪点闪烁。
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("texture_atlas.layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("texture_atlas.bind_group"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        Self {
            texture,
            view,
            sampler,
            bind_group_layout,
            bind_group,
        }
    }
}

fn extract_tile(atlas: &[u8], slot: u32) -> Vec<u8> {
    let col = slot % ATLAS_COLUMNS;
    let row = slot / ATLAS_COLUMNS;
    let mut tile = Vec::with_capacity((TILE_SIZE * TILE_SIZE * 4) as usize);
    for y in 0..TILE_SIZE {
        let start = (((row * TILE_SIZE + y) * ATLAS_WIDTH + col * TILE_SIZE) * 4) as usize;
        let end = start + (TILE_SIZE * 4) as usize;
        tile.extend_from_slice(&atlas[start..end]);
    }
    tile
}

/// CPU box filter：每个低级 mip texel 是上一级 2×2 区域的平均值。
fn downsample_rgba(previous: &[u8], width: u32, height: u32) -> Vec<u8> {
    let next_width = (width / 2).max(1);
    let next_height = (height / 2).max(1);
    let mut next = vec![0; (next_width * next_height * 4) as usize];
    for y in 0..next_height {
        for x in 0..next_width {
            let mut sum = [0u32; 4];
            for dy in 0..2 {
                for dx in 0..2 {
                    let sx = (x * 2 + dx).min(width - 1);
                    let sy = (y * 2 + dy).min(height - 1);
                    let src = ((sy * width + sx) * 4) as usize;
                    for channel in 0..4 {
                        sum[channel] += u32::from(previous[src + channel]);
                    }
                }
            }
            let dst = ((y * next_width + x) * 4) as usize;
            for channel in 0..4 {
                next[dst + channel] = ((sum[channel] + 2) / 4) as u8;
            }
        }
    }
    next
}

/// 生成完整 atlas 像素，供 GPU 上传和单元测试复用。
pub fn generate_atlas_rgba() -> Vec<u8> {
    let mut data = vec![0; (ATLAS_WIDTH * ATLAS_HEIGHT * 4) as usize];

    for slot in 0..(ATLAS_COLUMNS * ATLAS_ROWS) {
        paint_tile(&mut data, slot as u8, |x, y| {
            let checker = if ((x / 4) + (y / 4)) % 2 == 0 {
                205
            } else {
                140
            };
            [checker, 52, 196, 255]
        });
    }

    paint_tile(&mut data, 1, |x, y| {
        let n = signed_noise(x, y, 11, 18);
        let vein = if (x + y * 2 + noise(x, y, 5) % 7).is_multiple_of(19) {
            18
        } else {
            0
        };
        rgba([118 + n + vein, 124 + n + vein, 124 + n + vein], 255)
    });
    paint_tile(&mut data, 2, |x, y| {
        let n = signed_noise(x, y, 23, 16);
        let fleck = if noise(x, y, 91).is_multiple_of(9) {
            18
        } else {
            0
        };
        rgba([78 + n, 135 + n + fleck, 72 + n], 255)
    });
    paint_tile(&mut data, 3, |x, y| {
        let n = signed_noise(x, y, 37, 18);
        let pebble = if noise(x, y, 17).is_multiple_of(13) {
            16
        } else {
            0
        };
        rgba([126 + n + pebble, 89 + n, 60 + n], 255)
    });
    paint_tile(&mut data, 4, |x, y| {
        let wave = (((x * 3 + y * 5) % 17) as i32 - 8).abs();
        let streak = if (x + y * 2) % 13 == 0 { 22 } else { 0 };
        rgba([58 + streak, 125 + wave + streak, 172 + wave + streak], 255)
    });
    paint_tile(&mut data, 5, |x, y| {
        let edge = x == 0 || y == 0 || x == TILE_SIZE - 1 || y == TILE_SIZE - 1;
        let slash = x == y || x + y == TILE_SIZE - 1;
        let glow = if edge {
            38
        } else if slash {
            30
        } else {
            0
        };
        rgba([164 + glow, 208 + glow, 224 + glow], 255)
    });
    paint_tile(&mut data, 6, |x, y| {
        let n = signed_noise(x, y, 61, 12);
        let speck = if noise(x, y, 71).is_multiple_of(11) {
            -18
        } else {
            0
        };
        rgba([205 + n + speck, 184 + n + speck, 125 + n], 255)
    });
    paint_tile(&mut data, 7, |x, y| {
        let stripe = if (x + noise(0, y, 19) % 5) % 7 <= 1 {
            24
        } else {
            0
        };
        let n = signed_noise(x, y, 83, 10);
        rgba([126 + n + stripe, 84 + n, 45 + n], 255)
    });
    paint_tile(&mut data, 8, |x, y| {
        let n = signed_noise(x, y, 101, 20);
        let leaf = if noise(x / 2, y / 2, 29).is_multiple_of(5) {
            28
        } else {
            0
        };
        rgba([58 + n, 124 + n + leaf, 62 + n], 255)
    });
    paint_tile(&mut data, 9, |x, y| {
        let mortar = x.is_multiple_of(16) || y.is_multiple_of(8) || (y / 8) % 2 == 1 && x == 8;
        if mortar {
            rgba([70, 74, 72], 255)
        } else {
            let n = signed_noise(x, y, 131, 14);
            let chip = if noise(x, y, 137).is_multiple_of(17) {
                -22
            } else {
                0
            };
            rgba([116 + n + chip, 120 + n + chip, 116 + n + chip], 255)
        }
    });
    paint_tile(&mut data, 10, |x, y| {
        let n = signed_noise(x, y, 151, 18);
        let crack = if (x * 3 + y * 5 + noise(x, y, 157) % 11).is_multiple_of(23) {
            -42
        } else {
            0
        };
        rgba([48 + n + crack, 52 + n + crack, 56 + n + crack], 255)
    });

    data
}

fn paint_tile(data: &mut [u8], slot: u8, color: impl Fn(u32, u32) -> [u8; 4]) {
    let col = u32::from(slot) % ATLAS_COLUMNS;
    let row = u32::from(slot) / ATLAS_COLUMNS;
    for y in 0..TILE_SIZE {
        for x in 0..TILE_SIZE {
            let px = col * TILE_SIZE + x;
            let py = row * TILE_SIZE + y;
            let i = ((py * ATLAS_WIDTH + px) * 4) as usize;
            data[i..i + 4].copy_from_slice(&color(x, y));
        }
    }
}

fn rgba(rgb: [i32; 3], a: u8) -> [u8; 4] {
    [
        rgb[0].clamp(0, 255) as u8,
        rgb[1].clamp(0, 255) as u8,
        rgb[2].clamp(0, 255) as u8,
        a,
    ]
}

fn signed_noise(x: u32, y: u32, seed: u32, span: i32) -> i32 {
    (noise(x, y, seed) % (span as u32 * 2 + 1)) as i32 - span
}

fn noise(x: u32, y: u32, seed: u32) -> u32 {
    let mut v = x.wrapping_mul(374_761_393)
        ^ y.wrapping_mul(668_265_263)
        ^ seed.wrapping_mul(2_246_822_519);
    v = (v ^ (v >> 13)).wrapping_mul(1_274_126_177);
    v ^ (v >> 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(data: &[u8], slot: u8, x: u32, y: u32) -> [u8; 4] {
        let col = u32::from(slot) % ATLAS_COLUMNS;
        let row = u32::from(slot) / ATLAS_COLUMNS;
        let px = col * TILE_SIZE + x;
        let py = row * TILE_SIZE + y;
        let i = ((py * ATLAS_WIDTH + px) * 4) as usize;
        [data[i], data[i + 1], data[i + 2], data[i + 3]]
    }

    #[test]
    fn atlas_has_expected_byte_len() {
        let data = generate_atlas_rgba();
        assert_eq!(data.len(), (ATLAS_WIDTH * ATLAS_HEIGHT * 4) as usize);
    }

    #[test]
    fn known_tiles_are_not_fallback_magenta() {
        let data = generate_atlas_rgba();
        assert_ne!(pixel(&data, 1, 8, 8), pixel(&data, 0, 8, 8));
        assert_ne!(pixel(&data, 4, 8, 8), pixel(&data, 0, 8, 8));
        assert_ne!(pixel(&data, 8, 8, 8), pixel(&data, 0, 8, 8));
        assert_ne!(pixel(&data, 9, 8, 8), pixel(&data, 0, 8, 8));
        assert_ne!(pixel(&data, 10, 8, 8), pixel(&data, 0, 8, 8));
    }

    #[test]
    fn glass_tile_has_brighter_edges() {
        let data = generate_atlas_rgba();
        let edge = pixel(&data, 5, 0, 8);
        let center = pixel(&data, 5, 12, 8);
        assert!(edge[0] > center[0]);
        assert_eq!(edge[3], 255);
    }

    #[test]
    fn tile_extraction_does_not_include_atlas_neighbors() {
        let atlas = generate_atlas_rgba();
        let tile = extract_tile(&atlas, 5);
        assert_eq!(tile.len(), (TILE_SIZE * TILE_SIZE * 4) as usize);
        assert_eq!(&tile[0..4], &pixel(&atlas, 5, 0, 0));
        let last = tile.len() - 4;
        assert_eq!(&tile[last..], &pixel(&atlas, 5, 31, 31));
    }

    #[test]
    fn mip_chain_has_power_of_two_levels() {
        assert_eq!(MIP_LEVEL_COUNT, 6);
        let source = [
            0, 10, 20, 255, // (0,0)
            10, 20, 30, 255, // (1,0)
            20, 30, 40, 255, // (0,1)
            30, 40, 50, 255, // (1,1)
        ];
        assert_eq!(downsample_rgba(&source, 2, 2), vec![15, 25, 35, 255]);
    }
}
