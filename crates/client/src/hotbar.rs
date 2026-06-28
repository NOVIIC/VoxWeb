//! 玩家手持方块栏（hotbar）：9 格快捷栏 + 当前选中格。
//!
//! Phase 3 简化版：1-9 数字键切换选中格；HUD 底部一行显示。
//! Phase 6 会扩展为图标 + 鼠标滚轮切换。

use voxweb_core::block::BlockID;

/// 9 格 hotbar。`selected` 为 0..=8。
#[derive(Clone, Debug)]
pub struct Hotbar {
    pub items: [BlockID; 9],
    pub selected: usize,
}

impl Default for Hotbar {
    fn default() -> Self {
        // 9 个常用材质默认排布；基岩不进入创造模式热栏。
        Self {
            items: [
                BlockID::STONE,
                BlockID::DIRT,
                BlockID::GRASS,
                BlockID::SAND,
                BlockID::WOOD,
                BlockID::LEAVES,
                BlockID::GLASS,
                BlockID::WATER,
                BlockID::STONE_BRICKS,
            ],
            selected: 0,
        }
    }
}

impl Hotbar {
    /// 当前选中的方块。
    pub fn current(&self) -> BlockID {
        self.items[self.selected]
    }

    /// 按 1-9 键设置选中格（idx 来自 InputState::hotbar_request，0..=8）。
    /// 越界时静默忽略。
    pub fn select(&mut self, idx: u8) {
        if (idx as usize) < self.items.len() {
            self.selected = idx as usize;
        }
    }

    /// 选中格简称（HUD 显示用）。
    pub fn current_label(&self) -> &'static str {
        block_label(self.current())
    }
}

/// BlockID → 显示用简称（与 hotbar 默认排布对齐）。
pub fn block_label(id: BlockID) -> &'static str {
    match id {
        BlockID::AIR => "AIR",
        BlockID::STONE => "STONE",
        BlockID::DIRT => "DIRT",
        BlockID::GRASS => "GRASS",
        BlockID::SAND => "SAND",
        BlockID::WOOD => "WOOD",
        BlockID::LEAVES => "LEAVES",
        BlockID::GLASS => "GLASS",
        BlockID::WATER => "WATER",
        BlockID::STONE_BRICKS => "BRICK",
        BlockID::BEDROCK => "BEDROCK",
        _ => "???",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_selects_first() {
        let h = Hotbar::default();
        assert_eq!(h.current(), BlockID::STONE);
        assert_eq!(h.current_label(), "STONE");
    }

    #[test]
    fn select_changes_current() {
        let mut h = Hotbar::default();
        h.select(2);
        assert_eq!(h.current(), BlockID::GRASS);
        h.select(6);
        assert_eq!(h.current(), BlockID::GLASS);
        h.select(8);
        assert_eq!(h.current(), BlockID::STONE_BRICKS);
    }

    #[test]
    fn select_out_of_range_ignored() {
        let mut h = Hotbar {
            selected: 3,
            ..Hotbar::default()
        };
        h.select(99);
        assert_eq!(h.selected, 3, "越界请求应不改变选中");
    }
}
