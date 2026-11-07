//! GPU tile cache: rendered page pieces keyed by (page, level, row, col).

use std::collections::{BTreeMap, HashSet};

use crate::gfx::{Gpu, TexId};

/// Level used for whole-page low-resolution thumbnails.
pub const THUMB: i8 = i8::MIN;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct TileKey {
    pub page: u32,
    pub level: i8,
    pub row: u16,
    pub col: u16,
}

impl TileKey {
    pub fn thumb(page: u32) -> TileKey {
        TileKey { page, level: THUMB, row: 0, col: 0 }
    }

    pub fn is_thumb(&self) -> bool {
        self.level == THUMB
    }
}

#[derive(Debug)]
pub struct Tile {
    pub tex: TexId,
    pub w: u32,
    pub h: u32,
    pub tex_w: u32,
    pub tex_h: u32,
    pub used: u64,
}

impl Tile {
    pub fn uv(&self) -> [f32; 4] {
        [0.0, 0.0, self.w as f32 / self.tex_w as f32, self.h as f32 / self.tex_h as f32]
    }

    /// Texel-centre bounds of the valid area, for clamping.
    pub fn clamp(&self) -> [f32; 4] {
        let (tw, th) = (self.tex_w as f32, self.tex_h as f32);
        [0.5 / tw, 0.5 / th, (self.w as f32 - 0.5) / tw, (self.h as f32 - 0.5) / th]
    }

    fn bytes(&self) -> usize {
        self.tex_w as usize * self.tex_h as usize * 4
    }
}

const POOL_MAX: usize = 6;

pub struct TileCache {
    tiles: BTreeMap<TileKey, Tile>,
    /// Spare textures by allocated size, reused to avoid reallocation.
    pool: Vec<(u32, u32, TexId)>,
    bytes: usize,
    pub budget: usize,
}

impl TileCache {
    pub fn new(budget: usize) -> TileCache {
        TileCache { tiles: BTreeMap::new(), pool: Vec::new(), bytes: 0, budget }
    }

    pub fn len(&self) -> usize {
        self.tiles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn contains(&self, key: &TileKey) -> bool {
        self.tiles.contains_key(key)
    }

    pub fn get(&self, key: &TileKey) -> Option<&Tile> {
        self.tiles.get(key)
    }

    pub fn touch(&mut self, key: &TileKey, frame: u64) {
        if let Some(t) = self.tiles.get_mut(key) {
            t.used = frame;
        }
    }

    /// All cached tiles of `page`, thumbnails first, then by level.
    pub fn page(&self, page: u32) -> impl Iterator<Item = (&TileKey, &Tile)> {
        let lo = TileKey { page, level: i8::MIN, row: 0, col: 0 };
        let hi = TileKey { page, level: i8::MAX, row: u16::MAX, col: u16::MAX };
        self.tiles.range(lo..=hi)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert(&mut self, gpu: &mut dyn Gpu, key: TileKey, w: u32, h: u32, tex_w: u32, tex_h: u32, pixels: &[u8], frame: u64) {
        if let Some(old) = self.tiles.remove(&key) {
            self.release(gpu, old);
        }
        let tex = match self.pool.iter().position(|&(pw, ph, _)| pw == tex_w && ph == tex_h) {
            Some(i) => self.pool.swap_remove(i).2,
            None => gpu.create_texture(tex_w, tex_h, false),
        };
        gpu.upload(tex, 0, 0, w, h, pixels, false);
        let tile = Tile { tex, w, h, tex_w, tex_h, used: frame };
        self.bytes += tile.bytes();
        self.tiles.insert(key, tile);
    }

    fn release(&mut self, gpu: &mut dyn Gpu, tile: Tile) {
        self.bytes -= tile.bytes();
        if self.pool.len() < POOL_MAX {
            self.pool.push((tile.tex_w, tile.tex_h, tile.tex));
        } else {
            gpu.delete_texture(tile.tex);
        }
    }

    /// Evicts least recently used tiles until under budget. Tiles in `wanted`
    /// or drawn in `frame` are never evicted (the request planner keeps the
    /// wanted set within budget, so this cannot thrash).
    pub fn evict(&mut self, gpu: &mut dyn Gpu, frame: u64, wanted: &HashSet<TileKey>) {
        if self.bytes <= self.budget {
            return;
        }
        let mut candidates: Vec<(u64, TileKey)> =
            self.tiles.iter().filter(|(k, t)| t.used != frame && !wanted.contains(k)).map(|(k, t)| (t.used, *k)).collect();
        candidates.sort_unstable();
        for (_, key) in candidates {
            if self.bytes <= self.budget {
                break;
            }
            if let Some(t) = self.tiles.remove(&key) {
                self.release(gpu, t);
            }
        }
    }

    /// Drops every tile for which `keep` returns false.
    pub fn retain(&mut self, gpu: &mut dyn Gpu, keep: impl Fn(&TileKey) -> bool) {
        let drop: Vec<TileKey> = self.tiles.keys().filter(|k| !keep(k)).copied().collect();
        for k in drop {
            if let Some(t) = self.tiles.remove(&k) {
                self.release(gpu, t);
            }
        }
    }

    pub fn clear(&mut self, gpu: &mut dyn Gpu) {
        for (_, t) in std::mem::take(&mut self.tiles) {
            gpu.delete_texture(t.tex);
        }
        for (_, _, tex) in self.pool.drain(..) {
            gpu.delete_texture(tex);
        }
        self.bytes = 0;
    }

    /// Forgets all textures without deleting them (the GL context was lost).
    pub fn forget(&mut self) {
        self.tiles.clear();
        self.pool.clear();
        self.bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx::NullGpu;

    #[test]
    fn evicts_unwanted_then_lru_and_reuses_textures() {
        let mut gpu = NullGpu::default();
        let mut cache = TileCache::new(3 * 64 * 64 * 4);
        let px = vec![0u8; 64 * 64 * 4];
        let key = |p| TileKey { page: p, level: 0, row: 0, col: 0 };
        for p in 0..5 {
            cache.insert(&mut gpu, key(p), 64, 64, 64, 64, &px, p as u64);
        }
        let wanted: HashSet<_> = [key(0), key(4)].into_iter().collect();
        cache.evict(&mut gpu, 4, &wanted);
        assert_eq!(cache.len(), 3);
        assert!(cache.contains(&key(0)) && cache.contains(&key(4)) && cache.contains(&key(3)));
        let live_before = gpu.live.len();
        cache.insert(&mut gpu, key(9), 64, 64, 64, 64, &px, 5);
        assert_eq!(gpu.live.len(), live_before, "texture reused from pool");
        assert_eq!(cache.page(9).count(), 1);
        cache.clear(&mut gpu);
        assert!(gpu.live.is_empty());
    }
}
