//! MaterialField 原型存储结构。
//!
//! 当前运行时仍以 [`crate::chunk::Chunk`] 作为网络/渲染兼容格式；本模块提供
//! `FieldChunk` / `Column` / `Span`，作为后续 FieldSnapshot、OPFS schema 和
//! 颗粒/FreeObject 质量守恒迁移的核心数据结构。

use serde::{Deserialize, Serialize};

use crate::block::{BlockID, MaterialCell};
use crate::chunk::{CHUNK_X, CHUNK_Y, CHUNK_Z, Chunk};

pub type ObjectID = u64;

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct FieldChunk {
    pub columns: Box<[Column]>,
    pub free_object_refs: Vec<ObjectID>,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum Column {
    Spans(Vec<Span>),
    Dense(Box<[MaterialCell]>),
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Span {
    pub y_start: u16,
    pub length: u16,
    pub cell: MaterialCell,
}

impl FieldChunk {
    pub fn empty() -> Self {
        Self {
            columns: vec![Column::Spans(Vec::new()); CHUNK_X * CHUNK_Z].into_boxed_slice(),
            free_object_refs: Vec::new(),
        }
    }

    pub fn from_chunk(chunk: &Chunk) -> Self {
        let mut field = Self::empty();
        for lz in 0..CHUNK_Z {
            for lx in 0..CHUNK_X {
                let mut cells = [MaterialCell::EMPTY; CHUNK_Y];
                for (ly, cell) in cells.iter_mut().enumerate() {
                    *cell = block_to_cell(chunk.get(lx, ly, lz));
                }
                field.columns[column_index(lx, lz)] = Column::from_cells(&cells);
            }
        }
        field
    }

    pub fn to_chunk(&self) -> Chunk {
        let mut chunk = Chunk::empty();
        for lz in 0..CHUNK_Z {
            for lx in 0..CHUNK_X {
                let column = &self.columns[column_index(lx, lz)];
                for ly in 0..CHUNK_Y {
                    chunk.set(lx, ly, lz, cell_to_block(column.get(ly)));
                }
            }
        }
        chunk
    }

    pub fn get(&self, lx: usize, ly: usize, lz: usize) -> MaterialCell {
        self.columns[column_index(lx, lz)].get(ly)
    }

    pub fn set(&mut self, lx: usize, ly: usize, lz: usize, cell: MaterialCell) {
        self.columns[column_index(lx, lz)].set(ly, cell);
    }
}

impl Column {
    pub fn empty() -> Self {
        Self::Spans(Vec::new())
    }

    pub fn from_cells(cells: &[MaterialCell; CHUNK_Y]) -> Self {
        let mut spans = Vec::new();
        let mut y = 0usize;
        while y < CHUNK_Y {
            let cell = cells[y];
            let mut len = 1usize;
            while y + len < CHUNK_Y && cells[y + len] == cell {
                len += 1;
            }
            if !cell.is_empty() {
                spans.push(Span {
                    y_start: y as u16,
                    length: len as u16,
                    cell,
                });
            }
            y += len;
        }
        Self::Spans(spans)
    }

    pub fn get(&self, ly: usize) -> MaterialCell {
        match self {
            Column::Spans(spans) => spans
                .iter()
                .find(|span| {
                    let start = span.y_start as usize;
                    let end = start + span.length as usize;
                    ly >= start && ly < end
                })
                .map(|span| span.cell)
                .unwrap_or(MaterialCell::EMPTY),
            Column::Dense(cells) => cells[ly],
        }
    }

    pub fn set(&mut self, ly: usize, cell: MaterialCell) {
        if let Column::Spans(spans) = self {
            let mut dense = vec![MaterialCell::EMPTY; CHUNK_Y].into_boxed_slice();
            for span in spans.iter() {
                let start = span.y_start as usize;
                let end = (start + span.length as usize).min(CHUNK_Y);
                for y in start..end {
                    dense[y] = span.cell;
                }
            }
            *self = Column::Dense(dense);
        }

        let Column::Dense(cells) = self else {
            unreachable!("column was converted to dense");
        };
        cells[ly] = cell;
    }

    pub fn is_dense(&self) -> bool {
        matches!(self, Column::Dense(_))
    }

    pub fn span_count(&self) -> usize {
        match self {
            Column::Spans(spans) => spans.len(),
            Column::Dense(_) => 0,
        }
    }
}

#[inline]
pub fn column_index(lx: usize, lz: usize) -> usize {
    lz * CHUNK_X + lx
}

fn block_to_cell(block: BlockID) -> MaterialCell {
    MaterialCell::from_block_id(block)
}

fn cell_to_block(cell: MaterialCell) -> BlockID {
    cell.to_block_id()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_field_chunk_roundtrips_to_empty_chunk() {
        let field = FieldChunk::empty();
        let chunk = field.to_chunk();
        assert!(chunk.blocks.iter().all(|b| *b == BlockID::AIR));
    }

    #[test]
    fn chunk_to_field_to_chunk_roundtrip_preserves_primary_materials() {
        let mut chunk = Chunk::empty();
        chunk.set(1, 2, 3, BlockID::STONE);
        chunk.set(1, 3, 3, BlockID::STONE);
        chunk.set(1, 4, 3, BlockID::DIRT);
        chunk.set(8, 80, 8, BlockID::SAND);

        let field = FieldChunk::from_chunk(&chunk);
        let restored = field.to_chunk();

        assert_eq!(restored, chunk);
        assert_eq!(field.get(1, 2, 3), MaterialCell::full(BlockID::STONE));
        assert_eq!(field.get(1, 4, 3), MaterialCell::full(BlockID::DIRT));
    }

    #[test]
    fn natural_columns_are_span_encoded() {
        let mut chunk = Chunk::empty();
        for y in 0..64 {
            chunk.set(0, y, 0, BlockID::STONE);
        }
        for y in 64..68 {
            chunk.set(0, y, 0, BlockID::DIRT);
        }

        let field = FieldChunk::from_chunk(&chunk);
        let column = &field.columns[column_index(0, 0)];

        assert!(!column.is_dense());
        assert_eq!(column.span_count(), 2);
    }

    #[test]
    fn editing_span_column_degrades_to_dense() {
        let mut field = FieldChunk::empty();
        field.set(2, 10, 2, MaterialCell::full(BlockID::GLASS));

        let column = &field.columns[column_index(2, 2)];
        assert!(column.is_dense());
        assert_eq!(field.get(2, 10, 2), MaterialCell::full(BlockID::GLASS));
    }
}
