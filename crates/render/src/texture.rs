//! 程序化纹理图集。
//!
//! 当前没有外部美术资源，图集在启动时由 Rust 生成 RGBA8 像素并上传到 GPU。
//! shader 通过方块属性里的 `texture_index` 选择对应 32x32 tile。

pub const TILE_SIZE: u32 = 32;
pub const ATLAS_COLUMNS: u32 = 4;
pub const ATLAS_ROWS: u32 = 4;
pub const ATLAS_WIDTH: u32 = TILE_SIZE * ATLAS_COLUMNS;
pub const ATLAS_HEIGHT: u32 = TILE_SIZE * ATLAS_ROWS;

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
                width: ATLAS_WIDTH,
                height: ATLAS_HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let pixels = generate_atlas_rgba();
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(ATLAS_WIDTH * 4),
                rows_per_image: Some(ATLAS_HEIGHT),
            },
            wgpu::Extent3d {
                width: ATLAS_WIDTH,
                height: ATLAS_HEIGHT,
                depth_or_array_layers: 1,
            },
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("texture_atlas.sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
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
                        view_dimension: wgpu::TextureViewDimension::D2,
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
}
