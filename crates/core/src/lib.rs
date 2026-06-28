//! VoxWeb 核心数据结构与网络协议定义。
//! 本 crate 零浏览器依赖，所有 crate 都依赖它。

pub mod block;
pub mod chunk;
pub mod field;
pub mod geometry;
pub mod protocol;

pub use block::{
    BlockID, BlockProperties, BreakKernel, CellFlags, MATERIAL_REGISTRY, MaterialCell, MaterialID,
    MaterialProperties, MaterialRegistry, MechanicsClass, MixSlot, PlacementKernel,
    StabilityPolicy, VisualClass, properties,
};
pub use chunk::{CHUNK_SIZE, CHUNK_X, CHUNK_Y, CHUNK_Z, Chunk, ChunkPos, Position};
pub use field::{Column, FieldChunk, ObjectID, Span, column_index};
pub use geometry::{Aabb, PLAYER_EYE_OFFSET, PLAYER_HEIGHT, PLAYER_WIDTH, player_aabb};
pub use protocol::{
    AckReason, CHUNK_SNAPSHOT_PAYLOAD_MAX, ClientMessage, EntityId, OutboundMessage,
    PROTOCOL_VERSION, PlayerEntry, PlayerSnapshot, Recipient, RoomEvent, ServerMessage, encode,
};
