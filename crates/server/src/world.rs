//! 世界状态管理：Chunk 表、地形生成器、方块读写。
//!
//! Phase 2：World 持有 TerrainGenerator；区块按需生成 + 卸载；
//! 提供世界坐标查询接口供网格化跨区块剔除使用。
//! Phase 5：set_block 实装 dirty 标记；持久化层（Phase 8）会从 drain_dirty 取出待写。

use std::collections::{HashMap, HashSet, VecDeque};

use voxweb_core::block::{BlockID, MaterialCell};
use voxweb_core::chunk::{CHUNK_X, CHUNK_Y, CHUNK_Z, Chunk, ChunkPos, Position};
use voxweb_core::field::FieldChunk;
use voxweb_core::object::{FreeObject, FreeObjectState, ObjectID};

use crate::persistence::PersistenceManager;
use crate::terrain::TerrainGenerator;

/// Phase 8 默认内存 chunk cache 容量。4096 chunk 约 512MB 原始方块数据。
pub const DEFAULT_CHUNK_CACHE_CAPACITY: usize = 4096;

/// 世界状态。Phase 2 仅含 chunk 表 + 地形生成器；Phase 5 起加 dirty_chunks。
pub struct World {
    pub seed: u64,
    pub chunks: HashMap<ChunkPos, Chunk>,
    /// MaterialField 镜像。当前渲染/碰撞仍使用 `chunks` 适配视图，但权威写入会同步维护这里。
    pub field_chunks: HashMap<ChunkPos, FieldChunk>,
    /// 从静态场提取出的材质团。Dynamic 对象按 tick 模拟，静止后投影回 FieldChunk。
    pub free_objects: HashMap<ObjectID, FreeObject>,
    /// 待做稳定性判定的软材质 cell 队列（沙/土/草颗粒下落级联）。
    /// 编辑与颗粒 spawn/settle 会把邻域入队，`physics::step_soft_grains` 每 tick 限额 drain。
    pub unstable_soft: VecDeque<Position>,
    /// `unstable_soft` 的去重集合，避免同一 cell 反复入队膨胀。
    unstable_soft_set: HashSet<Position>,
    pub terrain: TerrainGenerator,
    /// 自创建以来的总 tick 数（Phase 2 仅累加，Phase 5 起驱动 Server::broadcast_tick）
    pub tick_count: u64,
    /// Phase 5：被 set_block 修改过的 ChunkPos 集合。
    /// 持久化层（Phase 8）通过 drain_dirty 取出待写；Phase 5 暂不消费。
    pub dirty_chunks: HashSet<ChunkPos>,
    /// Phase 8：dirty / in-flight / retry 状态机。`dirty_chunks` 保留为轻量只读视图，便于测试和调试。
    pub persistence: PersistenceManager,
    /// 简易 LRU 顺序表。front 旧、back 新；容量不引入外部 crate。
    lru_order: VecDeque<ChunkPos>,
    pinned_chunks: HashSet<ChunkPos>,
    chunk_cache_capacity: usize,
    next_object_id: ObjectID,
}

impl World {
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            chunks: HashMap::new(),
            field_chunks: HashMap::new(),
            free_objects: HashMap::new(),
            unstable_soft: VecDeque::new(),
            unstable_soft_set: HashSet::new(),
            terrain: TerrainGenerator::new(seed),
            tick_count: 0,
            dirty_chunks: HashSet::new(),
            persistence: PersistenceManager::new(),
            lru_order: VecDeque::new(),
            pinned_chunks: HashSet::new(),
            chunk_cache_capacity: DEFAULT_CHUNK_CACHE_CAPACITY,
            next_object_id: 1,
        }
    }

    /// 若 chunk 未生成则调地形生成器生成并插入。已存在则跳过。
    /// Phase 2 的 chunk 入口点（由 client::chunk_loader 调用）。
    pub fn ensure_chunk_generated(&mut self, pos: ChunkPos) {
        if self.chunks.contains_key(&pos) {
            if !self.field_chunks.contains_key(&pos)
                && let Some(chunk) = self.chunks.get(&pos)
            {
                self.field_chunks.insert(pos, FieldChunk::from_chunk(chunk));
            }
            self.touch_chunk(pos);
            return;
        }
        let chunk = self.terrain.generate_chunk(pos);
        let field = FieldChunk::from_chunk(&chunk);
        self.chunks.insert(pos, chunk);
        self.field_chunks.insert(pos, field);
        self.touch_chunk(pos);
        self.evict_if_needed();
    }

    /// 卸载（移除）一个 chunk。Phase 5 引入持久化后会先把 dirty 数据 flush 再移除。
    pub fn unload_chunk(&mut self, pos: ChunkPos) {
        if !self.persistence.is_dirty_or_in_flight(pos) {
            self.chunks.remove(&pos);
            self.field_chunks.remove(&pos);
            self.lru_order.retain(|p| *p != pos);
        }
    }

    /// 世界坐标 MaterialCell 查询；chunk 未加载或 y 越界一律返回 EMPTY。
    pub fn get_cell_world(&self, wx: i32, wy: i32, wz: i32) -> MaterialCell {
        if wy < 0 || wy >= CHUNK_Y as i32 {
            return MaterialCell::EMPTY;
        }
        let cp = Position::new(wx, wy, wz).to_chunk_pos();
        // local 坐标计算（rem_euclid 保证负坐标正确折算）
        let lx = wx.rem_euclid(CHUNK_X as i32) as usize;
        let lz = wz.rem_euclid(CHUNK_Z as i32) as usize;
        let ly = wy as usize;
        if let Some(field) = self.field_chunks.get(&cp) {
            return field.get(lx, ly, lz);
        }
        self.chunks
            .get(&cp)
            .map(|chunk| MaterialCell::from_block_id(chunk.get(lx, ly, lz)))
            .unwrap_or(MaterialCell::EMPTY)
    }

    /// 世界坐标方块查询；chunk 未加载或 y 越界一律返回 AIR。
    /// 供 chunk_mesh::generate_with_neighbors 的回调使用。
    pub fn get_block_world(&self, wx: i32, wy: i32, wz: i32) -> BlockID {
        self.get_cell_world(wx, wy, wz).to_block_id()
    }

    /// 在世界坐标处写入一个 MaterialCell（若 chunk 未加载则忽略）。
    /// 成功写入会把该 chunk 标记为 dirty。
    pub fn set_cell(&mut self, pos: Position, cell: MaterialCell) {
        self.set_cell_inner(pos, cell, true);
    }

    /// 写入本地世界视图但不标记 dirty。Remote 客户端接收 Host 快照 / 乐观预测时使用。
    pub fn set_cell_untracked(&mut self, pos: Position, cell: MaterialCell) {
        self.set_cell_inner(pos, cell, false);
    }

    /// 在世界坐标处放置一个方块（若 chunk 未加载则忽略）。
    /// 保留给旧 `BlockID` 调用方；内部转为 MaterialCell 写入。
    pub fn set_block(&mut self, pos: Position, block: BlockID) {
        self.set_cell(pos, MaterialCell::from_block_id(block));
    }

    /// 写入本地世界视图但不标记 dirty。Remote 客户端接收 Host 快照 / 乐观预测时使用。
    pub fn set_block_untracked(&mut self, pos: Position, block: BlockID) {
        self.set_cell_untracked(pos, MaterialCell::from_block_id(block));
    }

    fn set_cell_inner(&mut self, pos: Position, cell: MaterialCell, track_dirty: bool) {
        let cp = pos.to_chunk_pos();
        if !self.chunks.contains_key(&cp) {
            return;
        }
        let Some(idx) = pos.local_index() else {
            return;
        };
        if !self.field_chunks.contains_key(&cp)
            && let Some(chunk) = self.chunks.get(&cp)
        {
            self.field_chunks.insert(cp, FieldChunk::from_chunk(chunk));
        }

        let lx = pos.x.rem_euclid(CHUNK_X as i32) as usize;
        let lz = pos.z.rem_euclid(CHUNK_Z as i32) as usize;
        let ly = pos.y as usize;
        if self
            .field_chunks
            .get(&cp)
            .is_some_and(|field| field.get(lx, ly, lz) == cell)
        {
            self.touch_chunk(cp);
            return;
        }
        if let Some(field) = self.field_chunks.get_mut(&cp) {
            field.set(lx, ly, lz, cell);
        }
        if let Some(chunk) = self.chunks.get_mut(&cp) {
            chunk.blocks[idx] = cell.to_block_id();
        }
        if track_dirty {
            self.dirty_chunks.insert(cp);
            self.persistence.mark_dirty(cp);
        }
        self.touch_chunk(cp);
    }

    /// 读取世界坐标处的方块。chunk 未加载返回 AIR（与 get_block_world 等价的 Position 接口）。
    pub fn get_block(&self, pos: Position) -> BlockID {
        self.get_block_world(pos.x, pos.y, pos.z)
    }

    /// 读取世界坐标处的 MaterialCell。chunk 未加载返回 EMPTY。
    pub fn get_cell(&self, pos: Position) -> MaterialCell {
        self.get_cell_world(pos.x, pos.y, pos.z)
    }

    /// Phase 5：取出当前 dirty chunk 列表并清空集合。
    /// Phase 5 暂无调用方；Phase 8 持久化层每秒 flush 一次。
    pub fn drain_dirty(&mut self) -> Vec<ChunkPos> {
        let mut seen = HashSet::new();
        let mut drained = Vec::new();
        for pos in self.dirty_chunks.drain() {
            if seen.insert(pos) {
                drained.push(pos);
            }
        }
        for pos in self.persistence.take_dirty() {
            if seen.insert(pos) {
                drained.push(pos);
            }
        }
        drained
    }

    /// 将 dirty 移入 in-flight，并同步 `dirty_chunks` 视图。
    pub fn snapshot_dirty(&mut self, limit: usize, current_tick: u64) -> Vec<ChunkPos> {
        let positions = self.persistence.snapshot_dirty(limit, current_tick);
        for pos in &positions {
            self.dirty_chunks.remove(pos);
        }
        positions
    }

    /// 持久化写入成功后清理 dirty 状态。
    pub fn commit_flushed(&mut self, positions: &[ChunkPos]) {
        for pos in positions {
            self.dirty_chunks.remove(pos);
        }
        self.persistence.commit_flushed(positions);
    }

    /// 持久化写入失败后恢复 dirty 状态。
    pub fn record_flush_failure(&mut self, positions: &[ChunkPos], current_tick: u64) {
        for pos in positions {
            self.dirty_chunks.insert(*pos);
        }
        self.persistence
            .record_flush_failure(positions, current_tick);
    }

    /// 推进 tick 计数（Phase 5 起会驱动玩家广播等）。
    pub fn tick(&mut self) {
        self.tick_count += 1;
    }

    /// 直接载入持久化层读出的 FieldChunk，不标 dirty。
    pub fn load_field_chunk_from_storage(&mut self, pos: ChunkPos, field: FieldChunk) {
        let mut field = field;
        // active FreeObject 的 refs 由运行时对象表重建，避免把旧存档中的过期 refs 带进世界。
        field.free_object_refs.clear();
        let chunk = field.to_chunk();
        self.chunks.insert(pos, chunk);
        self.field_chunks.insert(pos, field);
        self.touch_chunk(pos);
        self.rebuild_free_object_refs();
        self.evict_if_needed();
    }

    pub fn record_projected_free_object(
        &mut self,
        cells: &[(Position, MaterialCell)],
        final_offset_y: i32,
    ) -> Option<ObjectID> {
        let id = self.next_object_id;
        self.next_object_id = if self.next_object_id == ObjectID::MAX {
            1
        } else {
            self.next_object_id + 1
        };
        let object = FreeObject::projected_from_cells(id, cells, final_offset_y)?;
        self.free_objects.insert(id, object);
        Some(id)
    }

    pub fn spawn_dynamic_free_object(
        &mut self,
        cells: &[(Position, MaterialCell)],
    ) -> Option<ObjectID> {
        let id = self.next_object_id;
        self.next_object_id = if self.next_object_id == ObjectID::MAX {
            1
        } else {
            self.next_object_id + 1
        };
        let object = FreeObject::dynamic_from_cells(id, cells)?;
        self.free_objects.insert(id, object);
        self.rebuild_free_object_refs();
        Some(id)
    }

    /// 提取一个软材质颗粒（sand/dirt/grass）为 active grain 对象。
    /// 不重建 free_object_refs（调用方在批量 spawn 后统一 `rebuild_free_object_refs`）。
    pub fn spawn_grain(&mut self, pos: Position, cell: MaterialCell) -> Option<ObjectID> {
        let id = self.allocate_object_id();
        let object = FreeObject::dynamic_grain_from_cells(id, &[(pos, cell)])?;
        self.free_objects.insert(id, object);
        Some(id)
    }

    /// 当前活跃（Dynamic）软材质颗粒数量，用于限制同时下落的 grain 上限。
    pub fn active_grain_count(&self) -> usize {
        self.free_objects
            .values()
            .filter(|object| object.granular && object.state == FreeObjectState::Dynamic)
            .count()
    }

    /// 把一个 cell 加入软材质稳定性判定队列（去重）。真正是否下落由 physics 层判定。
    pub fn enqueue_unstable(&mut self, pos: Position) {
        if self.unstable_soft_set.insert(pos) {
            self.unstable_soft.push_back(pos);
        }
    }

    /// 从队列取出下一个待判定 cell。
    pub fn next_unstable(&mut self) -> Option<Position> {
        let pos = self.unstable_soft.pop_front()?;
        self.unstable_soft_set.remove(&pos);
        Some(pos)
    }

    /// 当前排队待判定的软材质 cell 数量。
    pub fn unstable_len(&self) -> usize {
        self.unstable_soft.len()
    }

    pub fn insert_dynamic_free_object(
        &mut self,
        id: ObjectID,
        cells: &[(Position, MaterialCell)],
    ) -> Option<()> {
        // Remote 收到 Spawn/SpawnBatch 时走这里。软材质颗粒必须保留 granular，
        // 否则客户端会把沙/土/草画成硬立方体。
        let granular = !cells.is_empty()
            && cells.iter().all(|(_, cell)| {
                !cell.is_empty()
                    && voxweb_core::properties(cell.primary).stability
                        == voxweb_core::StabilityPolicy::ImmediateRelaxation
            });
        let object = if granular {
            FreeObject::dynamic_grain_from_cells(id, cells)?
        } else {
            FreeObject::dynamic_from_cells(id, cells)?
        };
        self.free_objects.insert(id, object);
        if self.next_object_id <= id {
            self.next_object_id = id.saturating_add(1).max(1);
        }
        self.rebuild_free_object_refs();
        Some(())
    }

    /// 从存档恢复 active FreeObject。若存档中的 id 与当前运行时对象冲突，会分配新 id。
    pub fn load_active_free_objects(&mut self, objects: Vec<FreeObject>) {
        for mut object in objects {
            if object.state != FreeObjectState::Dynamic || object.samples.is_empty() {
                continue;
            }
            if self.free_objects.contains_key(&object.id) {
                object.id = self.allocate_object_id();
            } else if self.next_object_id <= object.id {
                self.next_object_id = object.id.saturating_add(1).max(1);
            }
            self.free_objects.insert(object.id, object);
        }
        self.rebuild_free_object_refs();
    }

    /// 返回需要持久化到 world.json 的 active objects。排序保证 JSON 输出稳定。
    pub fn active_free_objects(&self) -> Vec<FreeObject> {
        let mut objects = self
            .free_objects
            .values()
            .filter(|object| object.state == FreeObjectState::Dynamic)
            .cloned()
            .collect::<Vec<_>>();
        objects.sort_by_key(|object| object.id);
        objects
    }

    pub fn dynamic_object_aabbs(&self) -> Vec<voxweb_core::Aabb> {
        self.free_objects
            .values()
            .filter(|object| object.state == FreeObjectState::Dynamic)
            .map(FreeObject::aabb)
            .collect()
    }

    pub fn rebuild_free_object_refs(&mut self) {
        for field in self.field_chunks.values_mut() {
            field.free_object_refs.clear();
        }

        let refs = self
            .free_objects
            .values()
            .filter(|object| object.state == FreeObjectState::Dynamic)
            .flat_map(|object| {
                chunk_refs_for_object(object)
                    .into_iter()
                    .map(move |pos| (pos, object.id))
            })
            .collect::<Vec<_>>();

        for (pos, object_id) in refs {
            let Some(field) = self.field_chunks.get_mut(&pos) else {
                continue;
            };
            if !field.free_object_refs.contains(&object_id) {
                field.free_object_refs.push(object_id);
            }
        }
    }

    pub fn set_chunk_cache_capacity(&mut self, capacity: usize) {
        self.chunk_cache_capacity = capacity.max(1);
        self.evict_if_needed();
    }

    pub fn chunk_cache_capacity(&self) -> usize {
        self.chunk_cache_capacity
    }

    pub fn pin_chunks<I>(&mut self, chunks: I)
    where
        I: IntoIterator<Item = ChunkPos>,
    {
        self.pinned_chunks.clear();
        self.pinned_chunks.extend(chunks);
    }

    pub fn pinned_len(&self) -> usize {
        self.pinned_chunks.len()
    }

    fn touch_chunk(&mut self, pos: ChunkPos) {
        self.lru_order.retain(|p| *p != pos);
        self.lru_order.push_back(pos);
    }

    fn evict_if_needed(&mut self) {
        while self.chunks.len() > self.chunk_cache_capacity {
            let Some(candidate) = self.lru_order.pop_front() else {
                break;
            };
            if self.pinned_chunks.contains(&candidate)
                || self.persistence.is_dirty_or_in_flight(candidate)
            {
                self.lru_order.push_back(candidate);
                if self.lru_order.iter().all(|p| {
                    self.pinned_chunks.contains(p) || self.persistence.is_dirty_or_in_flight(*p)
                }) {
                    break;
                }
                continue;
            }
            self.chunks.remove(&candidate);
            self.field_chunks.remove(&candidate);
        }
    }

    fn allocate_object_id(&mut self) -> ObjectID {
        loop {
            let id = self.next_object_id;
            self.next_object_id = if self.next_object_id == ObjectID::MAX {
                1
            } else {
                self.next_object_id + 1
            };
            if !self.free_objects.contains_key(&id) {
                return id;
            }
        }
    }
}

fn chunk_refs_for_object(object: &FreeObject) -> Vec<ChunkPos> {
    let aabb = object.aabb();
    let min_x = aabb.min.x.floor() as i32;
    let max_x = (aabb.max.x - f32::EPSILON).floor().max(aabb.min.x.floor()) as i32;
    let min_z = aabb.min.z.floor() as i32;
    let max_z = (aabb.max.z - f32::EPSILON).floor().max(aabb.min.z.floor()) as i32;
    let min_cx = min_x.div_euclid(CHUNK_X as i32);
    let max_cx = max_x.div_euclid(CHUNK_X as i32);
    let min_cz = min_z.div_euclid(CHUNK_Z as i32);
    let max_cz = max_z.div_euclid(CHUNK_Z as i32);

    let mut refs = Vec::new();
    for cz in min_cz..=max_cz {
        for cx in min_cx..=max_cx {
            refs.push(ChunkPos::new(cx, cz));
        }
    }
    refs
}

#[cfg(test)]
mod tests {
    use super::*;
    use voxweb_core::block::BlockID;
    use voxweb_core::chunk::Position;

    #[test]
    fn ensure_chunk_generated_is_idempotent_and_uses_terrain() {
        let mut world = World::new(12345);
        let pos = ChunkPos::new(0, 0);

        // 首次生成
        world.ensure_chunk_generated(pos);
        assert!(world.chunks.contains_key(&pos));
        assert!(world.field_chunks.contains_key(&pos));
        let snapshot: Vec<BlockID> = world.chunks[&pos].blocks.clone();
        // Perlin 地形：至少应有非 AIR 方块（基岩 + 一层地形）
        assert!(
            snapshot.iter().any(|b| *b != BlockID::AIR),
            "生成的 chunk 应至少有一个非空方块"
        );

        // 第二次调用：不应覆盖（同 blocks）
        world.ensure_chunk_generated(pos);
        assert_eq!(world.chunks[&pos].blocks, snapshot);
        assert_eq!(world.field_chunks[&pos].to_chunk().blocks, snapshot);
    }

    #[test]
    fn get_block_world_returns_air_for_unloaded_or_out_of_bounds() {
        let world = World::new(42);
        // 未加载 chunk
        assert_eq!(world.get_block_world(0, 64, 0), BlockID::AIR);
        // y 越界
        assert_eq!(world.get_block_world(0, -1, 0), BlockID::AIR);
        assert_eq!(world.get_block_world(0, 256, 0), BlockID::AIR);
    }

    #[test]
    fn get_block_world_reads_loaded_chunk() {
        let mut world = World::new(7);
        world.ensure_chunk_generated(ChunkPos::new(0, 0));
        // 与 chunk.get 等价
        let direct = world.chunks[&ChunkPos::new(0, 0)].get(3, 0, 5);
        let via_world = world.get_block_world(3, 0, 5);
        assert_eq!(direct, via_world);
    }

    #[test]
    fn unload_chunk_removes_chunk_entry() {
        let mut world = World::new(1);
        let pos = ChunkPos::new(2, -3);
        world.ensure_chunk_generated(pos);
        assert!(world.chunks.contains_key(&pos));
        world.unload_chunk(pos);
        assert!(!world.chunks.contains_key(&pos));
        assert!(!world.field_chunks.contains_key(&pos));
    }

    #[test]
    fn set_block_uses_position_local_index() {
        let mut world = World::new(0);
        world.ensure_chunk_generated(ChunkPos::new(0, 0));
        world.set_block(Position::new(5, 100, 7), BlockID::STONE);
        assert_eq!(world.get_block(Position::new(5, 100, 7)), BlockID::STONE);
        assert_eq!(
            world.field_chunks[&ChunkPos::new(0, 0)]
                .get(5, 100, 7)
                .to_block_id(),
            BlockID::STONE
        );
    }

    #[test]
    fn set_cell_preserves_material_cell_and_updates_chunk_mirror() {
        let mut world = World::new(0);
        let cp = ChunkPos::new(0, 0);
        let pos = Position::new(5, 100, 7);
        let cell = MaterialCell {
            occupancy: 128,
            primary: BlockID::SAND,
            secondary: None,
            flags: voxweb_core::CellFlags::empty(),
        };
        world.ensure_chunk_generated(cp);

        world.set_cell(pos, cell);

        assert_eq!(world.get_cell(pos), cell);
        assert_eq!(world.get_block(pos), BlockID::SAND);
        assert_eq!(world.chunks[&cp].get(5, 100, 7), BlockID::SAND);
        assert!(world.dirty_chunks.contains(&cp));
    }

    #[test]
    fn dynamic_free_object_preserves_material_cell() {
        let mut world = World::new(0);
        world.ensure_chunk_generated(ChunkPos::new(0, 0));
        let pos = Position::new(5, 100, 7);
        let cell = MaterialCell {
            occupancy: 180,
            primary: BlockID::SAND,
            secondary: Some(voxweb_core::block::MixSlot {
                material: BlockID::DIRT,
                occupancy: 40,
            }),
            flags: voxweb_core::block::CellFlags(voxweb_core::block::CellFlags::DIRTY),
        };

        let object_id = world
            .spawn_dynamic_free_object(&[(pos, cell)])
            .expect("spawn dynamic object");
        let object = world.free_objects.get(&object_id).unwrap();

        assert_eq!(object.cells_at_current_position(), vec![(pos, cell)]);
        assert_eq!(object.samples[0].cell(), cell);
        assert_eq!(object.material_summary.total_mass, 220);
    }

    #[test]
    fn set_block_marks_world_dirty() {
        // Phase 5：set_block 成功写入应把该 chunk 加进 dirty_chunks。
        let mut world = World::new(0);
        let cp = ChunkPos::new(0, 0);
        world.ensure_chunk_generated(cp);
        assert!(world.dirty_chunks.is_empty());

        world.set_block(Position::new(5, 100, 7), BlockID::STONE);
        assert!(world.dirty_chunks.contains(&cp));
        assert_eq!(world.persistence.dirty_len(), 1);

        // drain 后清空
        let drained = world.drain_dirty();
        assert_eq!(drained, vec![cp]);
        assert!(world.dirty_chunks.is_empty());
        assert_eq!(world.persistence.dirty_len(), 0);
    }

    #[test]
    fn commit_flushed_clears_dirty_view() {
        let mut world = World::new(0);
        let cp = ChunkPos::new(0, 0);
        world.ensure_chunk_generated(cp);
        world.set_block(Position::new(5, 100, 7), BlockID::STONE);

        let snap = world.snapshot_dirty(8, 60);
        assert_eq!(snap, vec![cp]);
        assert!(!world.dirty_chunks.contains(&cp));

        world.commit_flushed(&snap);
        assert!(!world.dirty_chunks.contains(&cp));
        assert!(world.drain_dirty().is_empty());
    }

    #[test]
    fn record_flush_failure_restores_dirty_view() {
        let mut world = World::new(0);
        let cp = ChunkPos::new(0, 0);
        world.ensure_chunk_generated(cp);
        world.set_block(Position::new(5, 100, 7), BlockID::STONE);

        let snap = world.snapshot_dirty(8, 60);
        world.record_flush_failure(&snap, 60);

        assert!(world.dirty_chunks.contains(&cp));
        assert!(world.persistence.dirty_len() > 0);
    }

    #[test]
    fn set_block_untracked_does_not_mark_dirty() {
        let mut world = World::new(0);
        let cp = ChunkPos::new(0, 0);
        world.ensure_chunk_generated(cp);

        world.set_block_untracked(Position::new(5, 100, 7), BlockID::STONE);
        assert_eq!(world.get_block(Position::new(5, 100, 7)), BlockID::STONE);
        assert_eq!(
            world.field_chunks[&cp].get(5, 100, 7).to_block_id(),
            BlockID::STONE
        );
        assert!(world.dirty_chunks.is_empty());
        assert_eq!(world.persistence.dirty_len(), 0);
    }

    #[test]
    fn set_block_on_unloaded_chunk_does_not_mark_dirty() {
        // chunk 未加载时 set_block 不应静默写入 / 标 dirty。
        let mut world = World::new(0);
        world.set_block(Position::new(0, 64, 0), BlockID::STONE);
        assert!(world.dirty_chunks.is_empty());
    }

    #[test]
    fn dynamic_free_object_refs_track_overlapped_chunks() {
        let mut world = World::new(0);
        let left = ChunkPos::new(0, 0);
        let right = ChunkPos::new(1, 0);
        world.ensure_chunk_generated(left);
        world.ensure_chunk_generated(right);

        let object_id = world
            .spawn_dynamic_free_object(&[
                (
                    Position::new(15, 5, 1),
                    MaterialCell::from_block_id(BlockID::STONE_BRICKS),
                ),
                (
                    Position::new(16, 5, 1),
                    MaterialCell::from_block_id(BlockID::STONE_BRICKS),
                ),
            ])
            .expect("spawn dynamic object");

        assert!(
            world.field_chunks[&left]
                .free_object_refs
                .contains(&object_id)
        );
        assert!(
            world.field_chunks[&right]
                .free_object_refs
                .contains(&object_id)
        );
        assert_eq!(world.active_free_objects().len(), 1);
    }

    #[test]
    fn load_field_chunk_clears_stale_refs_and_rebuilds_active_refs() {
        let mut source = World::new(0);
        source.ensure_chunk_generated(ChunkPos::new(0, 0));
        let object_id = source
            .spawn_dynamic_free_object(&[(
                Position::new(1, 5, 1),
                MaterialCell::from_block_id(BlockID::STONE_BRICKS),
            )])
            .expect("spawn dynamic object");
        let active = source.active_free_objects();
        let mut stored = source.field_chunks[&ChunkPos::new(0, 0)].clone();
        stored.free_object_refs.push(ObjectID::MAX);

        let mut world = World::new(0);
        world.load_active_free_objects(active);
        world.load_field_chunk_from_storage(ChunkPos::new(0, 0), stored);

        let refs = &world.field_chunks[&ChunkPos::new(0, 0)].free_object_refs;
        assert!(refs.contains(&object_id));
        assert!(!refs.contains(&ObjectID::MAX));
    }

    #[test]
    fn lru_evicts_unpinned_clean_chunks() {
        let mut world = World::new(0);
        world.set_chunk_cache_capacity(2);
        world.ensure_chunk_generated(ChunkPos::new(0, 0));
        world.ensure_chunk_generated(ChunkPos::new(1, 0));
        world.ensure_chunk_generated(ChunkPos::new(2, 0));
        assert_eq!(world.chunks.len(), 2);
        assert!(!world.chunks.contains_key(&ChunkPos::new(0, 0)));
    }

    #[test]
    fn lru_keeps_pinned_chunks() {
        let mut world = World::new(0);
        world.set_chunk_cache_capacity(1);
        world.ensure_chunk_generated(ChunkPos::new(0, 0));
        world.pin_chunks([ChunkPos::new(0, 0)]);
        world.ensure_chunk_generated(ChunkPos::new(1, 0));
        assert!(world.chunks.contains_key(&ChunkPos::new(0, 0)));
    }
}
