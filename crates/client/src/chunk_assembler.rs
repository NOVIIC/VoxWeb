//! FieldSnapshot 分片组装器。
//!
//! Host 用 bootstrap 快照或 `FieldRequest` 响应把每个 FieldChunk 的 bincode 编码结果切片发送；
//! Remote 端接收时通过本模块按 ChunkPos 汇集，齐了返回 concatenated bytes。
//!
//! 本层不关心具体编码格式；解码在上层 `apply_server_message` 做。

use std::collections::HashMap;

use voxweb_core::chunk::ChunkPos;

/// 一个正在组装中的 chunk 的临时状态。
#[derive(Debug)]
struct PartialAssemble {
    /// 主机预告的片总数。
    frag_total: u16,
    /// received[i] = Some(bytes) 表示第 i 片已到；None 表示还未收到。
    fragments: Vec<Option<Vec<u8>>>,
}

impl PartialAssemble {
    fn new(frag_total: u16) -> Self {
        Self {
            frag_total,
            fragments: vec![None; frag_total as usize],
        }
    }

    /// 检查是否所有片都已到齐。
    fn is_complete(&self) -> bool {
        self.fragments.iter().all(Option::is_some)
    }

    /// 取出所有 payload 拼接成 Vec<u8>。调用方保证 is_complete。
    fn concat(&self) -> Vec<u8> {
        self.fragments
            .iter()
            .filter_map(|f| f.as_deref())
            .flatten()
            .copied()
            .collect()
    }
}

/// 分片组装器。Host 一次性发入局快照，接收端靠这个汇集每个 chunk 的分片。
#[derive(Default)]
pub struct ChunkAssembler {
    partials: HashMap<ChunkPos, PartialAssemble>,
}

impl ChunkAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// 接收一片 payload。
    ///
    /// * `pos` — chunk 坐标
    /// * `frag_index` — 本片的序号（0-based）
    /// * `frag_total` — 该 chunk 的片总数
    /// * `payload` — 本片的字节（由调用方从 `FieldSnapshot` 中移出）
    ///
    /// 当所有片到齐时返回完整的 concatenated bytes，并把该 chunk 的临时状态清掉；
    /// 尚未到齐返回 `None`。
    pub fn ingest(
        &mut self,
        pos: ChunkPos,
        frag_index: u16,
        frag_total: u16,
        payload: Vec<u8>,
    ) -> Option<Vec<u8>> {
        let entry = self.partials.entry(pos).or_insert_with(|| {
            // 防御：若 frag_total == 0 则至少分配 1 槽（逻辑上不应发生）
            PartialAssemble::new(frag_total.max(1))
        });

        // 若 frag_total 在一次组装中变了（如 Host 重启中继发了不同编码版本的快照），
        // 则清空旧进度重新开始  — 这是极罕见的边缘情况但处理成本极低。
        if entry.frag_total != frag_total {
            *entry = PartialAssemble::new(frag_total.max(1));
        }

        // 写入该片；重复摄入同一 index 的片（重传）直接覆盖
        if frag_index as usize >= entry.fragments.len() {
            log::warn!(
                "[assembler] chunk {pos:?} frag_index {frag_index} >= frag_total {frag_total}; dropped"
            );
            return None;
        }
        entry.fragments[frag_index as usize] = Some(payload);

        if entry.is_complete() {
            let partial = self
                .partials
                .remove(&pos)
                .expect("just checked is_complete");
            Some(partial.concat())
        } else {
            None
        }
    }

    /// 放弃对某个 chunk 的组装（例如连接的 Remote 断线，清理残留）。
    pub fn cancel(&mut self, pos: ChunkPos) {
        self.partials.remove(&pos);
    }

    pub fn len(&self) -> usize {
        self.partials.len()
    }

    pub fn is_empty(&self) -> bool {
        self.partials.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_returns_none_until_complete() {
        let mut a = ChunkAssembler::new();
        let pos = ChunkPos::new(0, 0);

        assert!(a.ingest(pos, 0, 3, b"AAAA".to_vec()).is_none());
        assert!(a.ingest(pos, 1, 3, b"BBBB".to_vec()).is_none());
        // 第三片到齐
        let full = a
            .ingest(pos, 2, 3, b"CCCC".to_vec())
            .expect("should complete");
        assert_eq!(full, b"AAAABBBBCCCC");
        // entry 已清空
        assert!(a.partials.is_empty());
    }

    #[test]
    fn out_of_order_fragments_reassemble() {
        let mut a = ChunkAssembler::new();
        let pos = ChunkPos::new(0, 1);
        // 乱序到达
        assert!(a.ingest(pos, 2, 3, b"C".to_vec()).is_none());
        assert!(a.ingest(pos, 0, 3, b"A".to_vec()).is_none());
        let full = a
            .ingest(pos, 1, 3, b"B".to_vec())
            .expect("final fragment should complete assembly");
        assert_eq!(&full[..], b"ABC");
    }

    #[test]
    fn duplicate_fragment_is_idempotent() {
        let mut a = ChunkAssembler::new();
        let pos = ChunkPos::new(1, 0);
        a.ingest(pos, 0, 2, b"X".to_vec());
        // 重复摄入第一片不应打破进度
        a.ingest(pos, 0, 2, b"X".to_vec());
        assert!(a.ingest(pos, 1, 2, b"Y".to_vec()).is_some());
    }

    #[test]
    fn frag_total_mismatch_resets_entry() {
        let mut a = ChunkAssembler::new();
        let pos = ChunkPos::new(0, 0);
        a.ingest(pos, 0, 3, b"A".to_vec());
        // Host 重发了不同 total → 旧进度被清空
        let r = a.ingest(pos, 0, 2, b"B".to_vec());
        assert!(r.is_none());
        let full = a.ingest(pos, 1, 2, b"C".to_vec());
        assert_eq!(full, Some(b"BC".to_vec()));
    }

    #[test]
    fn out_of_range_fragment_is_dropped() {
        let mut a = ChunkAssembler::new();
        let pos = ChunkPos::new(0, 0);
        a.ingest(pos, 0, 2, b"X".to_vec());
        // frag_index 999 >= frag_total 2 → 丢弃
        assert!(a.ingest(pos, 999, 2, b"Y".to_vec()).is_none());
    }

    #[test]
    fn zero_frag_total_does_not_create_empty_slot_vec() {
        let mut a = ChunkAssembler::new();
        let pos = ChunkPos::new(0, 0);
        let full = a.ingest(pos, 0, 0, b"X".to_vec());
        assert_eq!(full, Some(b"X".to_vec()));
        assert!(a.is_empty());
    }
}
