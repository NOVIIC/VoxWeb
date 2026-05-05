//! VoxWeb 核心数据结构与网络协议定义。
//! 本 crate 零浏览器依赖，所有 crate 都依赖它。

pub mod block;
pub mod chunk;
pub mod protocol;

pub use block::{BlockID, BlockProperties, properties};
pub use chunk::{CHUNK_SIZE, CHUNK_X, CHUNK_Y, CHUNK_Z, Chunk, ChunkPos, Position};
pub use protocol::{AckReason, ClientMessage, PlayerSnapshot, RoomEvent, ServerMessage, encode};
