//! GPU glyph atlas: rasterize once, pack, cache, and reuse.
//!
//! A [`GlyphAtlas`] holds two texture atlases, selected per glyph by its
//! rasterized content: an `R8Unorm` mask atlas for outline glyphs and an
//! `Rgba8Unorm` color atlas for emoji. Glyphs are rasterized through
//! cosmic-text's [`SwashCache`], packed into the texture with an etagere
//! allocator, and cached by [`CacheKey`] so repeated lookups are free.
//!
//! When an atlas fills, least-recently-used glyphs not needed this frame are
//! evicted. If eviction cannot free enough room the texture grows: it doubles
//! and copies the old texture into the new one, which preserves every glyph's
//! coordinates.

use cosmic_text::{CacheKey, FontSystem, SwashCache, SwashImage};
use etagere::{size2, AllocId, Allocation, BucketedAtlasAllocator};
use lru::LruCache;
use rustc_hash::FxBuildHasher;
use std::collections::HashSet;
use swash::scale::image::Content;
use wgpu::{
    CommandEncoderDescriptor, Device, Extent3d, Origin3d, Queue, TexelCopyBufferLayout,
    TexelCopyTextureInfo, Texture, TextureAspect, TextureDescriptor, TextureDimension,
    TextureFormat, TextureUsages, TextureView, TextureViewDescriptor,
};

/// Edge length, in texels, of a freshly created atlas texture. Atlases double
/// from here as they fill, up to the device's maximum texture dimension.
const INITIAL_SIZE: u32 = 256;

/// Which of the two atlases a glyph lives in, so the text pass can bind the
/// matching texture and choose mask-vs-color blending.
#[derive(Clone, Copy, Debug)]
pub enum AtlasKind {
    Mask,
    Color,
}

/// Where a rasterized glyph sits in its atlas, for the text pass to draw it.
#[derive(Clone, Copy, Debug)]
pub struct GlyphInfo {
    pub kind: AtlasKind,
    /// Atlas texture coordinates as `[u_min, v_min, u_max, v_max]`.
    pub uv: [f32; 4],
    /// Glyph bitmap size in texels, `[width, height]`.
    pub size: [u32; 2],
    /// Bitmap offset from the glyph origin, `[left, top]` (top is upward).
    pub placement: [i32; 2],
}

/// The identity a glyph is cached under.
///
/// A font glyph is keyed by its [`CacheKey`] and a procedural glyph by its
/// codepoint and cell size. Both share one atlas, packer, and eviction order.
/// The distinct variants keep a procedural glyph from colliding with the font
/// glyph for the same codepoint.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum CacheId {
    Font(CacheKey),
    Procedural { cp: u32, width: u32, height: u32 },
}

/// A pair of glyph atlases, one for mask glyphs and one for color glyphs.
pub struct GlyphAtlas {
    mask: Atlas,
    color: Atlas,
}

impl GlyphAtlas {
    pub fn new(device: &Device) -> GlyphAtlas {
        let max_dim = device.limits().max_texture_dimension_2d;

        GlyphAtlas {
            mask: Atlas::new(device, AtlasKind::Mask, max_dim),
            color: Atlas::new(device, AtlasKind::Color, max_dim),
        }
    }

    /// Mark the start of a frame, releasing the previous frame's glyphs for
    /// eviction. Call before the frame's [`Self::get_or_insert`] calls so a
    /// glyph drawn this frame is never evicted to make room for another.
    pub fn begin_frame(&mut self) {
        self.mask.in_use.clear();
        self.color.in_use.clear();
    }

    /// Look up `key`, rasterizing and caching it on first use.
    ///
    /// Returns the glyph's atlas placement, or `None` for a whitespace glyph
    /// with no pixels or when the atlas is full and nothing can be evicted.
    pub fn get_or_insert(
        &mut self,
        device: &Device,
        queue: &Queue,
        font_system: &mut FontSystem,
        swash_cache: &mut SwashCache,
        key: CacheKey,
    ) -> Option<GlyphInfo> {
        let id = CacheId::Font(key);
        if let Some(hit) = self.mask.lookup(id) {
            return hit;
        }
        if let Some(hit) = self.color.lookup(id) {
            return hit;
        }

        let image = swash_cache.get_image_uncached(font_system, key)?;
        let atlas = match atlas_kind(image.content) {
            AtlasKind::Mask => &mut self.mask,
            AtlasKind::Color => &mut self.color,
        };

        atlas.insert_image(device, queue, id, &image)
    }

    /// Look up a procedurally drawn glyph, rasterizing and caching it on first
    /// use.
    ///
    /// `render` produces the `width`x`height` R8 coverage only on a cache miss
    /// and is uploaded once. A later atlas grow copies it forward from the old
    /// texture, so `render` never re-runs. Procedural glyphs always live in the
    /// mask atlas. `None` when the atlas is full and nothing can be evicted, or
    /// when `render` yields no pixels.
    pub fn get_or_insert_procedural(
        &mut self,
        device: &Device,
        queue: &Queue,
        cp: u32,
        width: u32,
        height: u32,
        render: impl FnOnce() -> Vec<u8>,
    ) -> Option<GlyphInfo> {
        let id = CacheId::Procedural { cp, width, height };
        if let Some(hit) = self.mask.lookup(id) {
            return hit;
        }

        self.mask
            .insert_pixels(device, queue, id, width, height, render())
    }

    pub fn mask_view(&self) -> &TextureView {
        &self.mask.view
    }

    pub fn color_view(&self) -> &TextureView {
        &self.color.view
    }

    /// The mask and color atlas texture dimensions, in texels.
    ///
    /// Each grows (doubles) only when a glyph no longer fits, which moves every
    /// packed glyph's UV. A caller that caches glyph instances compares this
    /// across a frame to tell whether reused UVs are still valid.
    pub fn texture_dims(&self) -> (u32, u32) {
        (self.mask.size, self.color.size)
    }

    /// A generation counter that changes whenever any packed glyph's UV moves,
    /// across both the mask and color atlases.
    ///
    /// A grow or an eviction in either atlas bumps it. A caller that cached
    /// glyph instances against an earlier frame's atlas may reuse them only
    /// while this is unchanged. A difference means some UV moved, so the cached
    /// instances now point at the wrong pixels and must be rebuilt.
    pub fn content_epoch(&self) -> u64 {
        self.mask.epoch.wrapping_add(self.color.epoch)
    }
}

struct Atlas {
    kind: AtlasKind,
    texture: Texture,
    view: TextureView,
    packer: BucketedAtlasAllocator,
    size: u32,
    max_dim: u32,
    cache: LruCache<CacheId, CachedGlyph, FxBuildHasher>,
    in_use: HashSet<CacheId, FxBuildHasher>,
    /// Bumped whenever a packed glyph's UV changes: a grow rescales every
    /// normalized coordinate, and an eviction frees a slot another glyph then
    /// reuses. A caller reusing glyph instances across frames compares it to
    /// tell whether the UVs it cached still point at the right pixels.
    epoch: u64,
}

impl Atlas {
    fn new(device: &Device, kind: AtlasKind, max_dim: u32) -> Atlas {
        let size = INITIAL_SIZE.min(max_dim);
        let texture = create_texture(device, kind, size);
        let view = texture.create_view(&TextureViewDescriptor::default());

        Atlas {
            kind,
            texture,
            view,
            packer: BucketedAtlasAllocator::new(size2(size as i32, size as i32)),
            size,
            max_dim,
            cache: LruCache::unbounded_with_hasher(FxBuildHasher),
            in_use: HashSet::default(),
            epoch: 0,
        }
    }

    /// `Some` if `id` is cached (inner value `None` for an empty glyph),
    /// `None` if it must be rasterized.
    fn lookup(&mut self, id: CacheId) -> Option<Option<GlyphInfo>> {
        let cached = self.cache.get(&id)?;
        let info = glyph_info(cached, self.kind, self.size);
        self.in_use.insert(id);
        Some(info)
    }

    /// Pack and cache a font glyph's swash bitmap. A later grow copies it
    /// forward from the old texture, so no pixels are retained.
    fn insert_image(
        &mut self,
        device: &Device,
        queue: &Queue,
        id: CacheId,
        image: &SwashImage,
    ) -> Option<GlyphInfo> {
        let width = image.placement.width;
        let height = image.placement.height;

        if width == 0 || height == 0 {
            self.cache.put(
                id,
                CachedGlyph::empty(image.placement.left, image.placement.top),
            );
            self.in_use.insert(id);
            return None;
        }

        let allocation = self.allocate(device, queue, width, height)?;
        let x = allocation.rectangle.min.x as u32;
        let y = allocation.rectangle.min.y as u32;
        write_glyph(
            queue,
            &self.texture,
            num_channels(self.kind),
            [x, y],
            [width, height],
            &image.data,
        );

        let cached = CachedGlyph {
            alloc: Some(allocation.id),
            x,
            y,
            width,
            height,
            left: image.placement.left,
            top: image.placement.top,
        };
        let info = glyph_info(&cached, self.kind, self.size);
        self.cache.put(id, cached);
        self.in_use.insert(id);

        info
    }

    /// Pack and cache a procedurally drawn glyph. A later grow copies it
    /// forward from the old texture, so `pixels` is uploaded once and not
    /// retained.
    fn insert_pixels(
        &mut self,
        device: &Device,
        queue: &Queue,
        id: CacheId,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    ) -> Option<GlyphInfo> {
        if width == 0 || height == 0 || pixels.is_empty() {
            return None;
        }

        let allocation = self.allocate(device, queue, width, height)?;
        let x = allocation.rectangle.min.x as u32;
        let y = allocation.rectangle.min.y as u32;
        write_glyph(
            queue,
            &self.texture,
            num_channels(self.kind),
            [x, y],
            [width, height],
            &pixels,
        );

        let cached = CachedGlyph {
            alloc: Some(allocation.id),
            x,
            y,
            width,
            height,
            left: 0,
            top: 0,
        };
        let info = glyph_info(&cached, self.kind, self.size);
        self.cache.put(id, cached);
        self.in_use.insert(id);

        info
    }

    /// Reserve a slot for a `width`x`height` glyph, growing the atlas when the
    /// packer is full. `None` only when the atlas is at the device limit and
    /// every sized glyph is needed this frame.
    fn allocate(
        &mut self,
        device: &Device,
        queue: &Queue,
        width: u32,
        height: u32,
    ) -> Option<Allocation> {
        loop {
            if let Some(allocation) = self.try_allocate(width, height) {
                return Some(allocation);
            }
            if !self.grow(device, queue) {
                return None;
            }
        }
    }

    /// Reserve a `width`x`height` region, evicting least-recently-used glyphs
    /// that are not needed this frame until it fits. `None` if every sized
    /// glyph is in use (the caller then grows the atlas).
    fn try_allocate(&mut self, width: u32, height: u32) -> Option<Allocation> {
        let size = size2(width as i32, height as i32);

        loop {
            if let Some(allocation) = self.packer.allocate(size) {
                return Some(allocation);
            }

            let (mut key, mut glyph) = self.cache.peek_lru()?;
            while glyph.alloc.is_none() {
                if self.in_use.contains(key) {
                    return None;
                }
                let _ = self.cache.pop_lru();
                (key, glyph) = self.cache.peek_lru()?;
            }

            if self.in_use.contains(key) {
                return None;
            }

            let (_, evicted) = self.cache.pop_lru().expect("peeked entry is present");
            self.packer
                .deallocate(evicted.alloc.expect("sized glyph has an allocation"));
            self.epoch = self.epoch.wrapping_add(1);
        }
    }

    /// Double the atlas (up to the device limit) and copy the old texture into
    /// the new one. etagere preserves existing coordinates across the grow, so
    /// a single GPU texture-to-texture copy relocates every packed glyph. The
    /// epoch still bumps because the larger texture rescales every normalized
    /// UV. `false` if already at the device limit.
    fn grow(&mut self, device: &Device, queue: &Queue) -> bool {
        if self.size >= self.max_dim {
            return false;
        }

        let new_size = (self.size * 2).min(self.max_dim);
        self.packer.grow(size2(new_size as i32, new_size as i32));

        // The old texture already holds every retained glyph at its preserved
        // coordinates, so copy it wholesale into the new one. A pending
        // write_texture staging copy flushes before this later-submitted
        // command buffer, so in-flight glyph uploads land first.
        let old = std::mem::replace(
            &mut self.texture,
            create_texture(device, self.kind, new_size),
        );
        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor::default());
        encoder.copy_texture_to_texture(
            old.as_image_copy(),
            self.texture.as_image_copy(),
            Extent3d {
                width: self.size,
                height: self.size,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);

        self.view = self.texture.create_view(&TextureViewDescriptor::default());
        self.size = new_size;
        self.epoch = self.epoch.wrapping_add(1);
        true
    }
}

struct CachedGlyph {
    /// `None` for an empty (whitespace) glyph that occupies no atlas space.
    alloc: Option<AllocId>,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    left: i32,
    top: i32,
}

impl CachedGlyph {
    fn empty(left: i32, top: i32) -> CachedGlyph {
        CachedGlyph {
            alloc: None,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            left,
            top,
        }
    }
}

/// The atlas placement of `glyph` for the text pass to draw, or `None` for an
/// empty glyph that occupies no atlas space.
fn glyph_info(glyph: &CachedGlyph, kind: AtlasKind, atlas_size: u32) -> Option<GlyphInfo> {
    if glyph.width == 0 || glyph.height == 0 {
        return None;
    }

    Some(GlyphInfo {
        kind,
        uv: uv_rect(glyph.x, glyph.y, glyph.width, glyph.height, atlas_size),
        size: [glyph.width, glyph.height],
        placement: [glyph.left, glyph.top],
    })
}

fn atlas_kind(content: Content) -> AtlasKind {
    match content {
        Content::Mask => AtlasKind::Mask,
        Content::SubpixelMask | Content::Color => AtlasKind::Color,
    }
}

fn uv_rect(x: u32, y: u32, width: u32, height: u32, atlas_size: u32) -> [f32; 4] {
    let size = atlas_size as f32;
    [
        x as f32 / size,
        y as f32 / size,
        (x + width) as f32 / size,
        (y + height) as f32 / size,
    ]
}

fn create_texture(device: &Device, kind: AtlasKind, size: u32) -> Texture {
    device.create_texture(&TextureDescriptor {
        label: Some("glyph atlas"),
        size: Extent3d {
            width: size,
            height: size,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format: texture_format(kind),
        usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST | TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

fn write_glyph(
    queue: &Queue,
    texture: &Texture,
    channels: u32,
    origin: [u32; 2],
    size: [u32; 2],
    data: &[u8],
) {
    queue.write_texture(
        TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: Origin3d {
                x: origin[0],
                y: origin[1],
                z: 0,
            },
            aspect: TextureAspect::All,
        },
        data,
        TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(size[0] * channels),
            rows_per_image: None,
        },
        Extent3d {
            width: size[0],
            height: size[1],
            depth_or_array_layers: 1,
        },
    );
}

fn texture_format(kind: AtlasKind) -> TextureFormat {
    match kind {
        AtlasKind::Mask => TextureFormat::R8Unorm,
        AtlasKind::Color => TextureFormat::Rgba8Unorm,
    }
}

fn num_channels(kind: AtlasKind) -> u32 {
    match kind {
        AtlasKind::Mask => 1,
        AtlasKind::Color => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::{uv_rect, GlyphAtlas};
    use crate::gpu::headless_device;
    use wgpu::{
        BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Device, Extent3d, MapMode,
        Origin3d, PollType, Queue, TexelCopyBufferInfo, TexelCopyBufferLayout,
        TexelCopyTextureInfo, Texture, TextureAspect,
    };

    #[test]
    fn uv_rect_normalizes_to_atlas_size() {
        assert_eq!(uv_rect(64, 32, 16, 8, 256), [0.25, 0.125, 0.3125, 0.15625]);
        assert_eq!(uv_rect(0, 0, 256, 256, 256), [0.0, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn grow_copies_existing_glyphs_into_the_larger_texture() {
        let Some((device, queue)) = headless_device() else {
            eprintln!("atlas grow test: no wgpu adapter, skipping");
            return;
        };

        let mut atlas = GlyphAtlas::new(&device);
        let (initial, _) = atlas.texture_dims();

        // Insert 20x20 solid glyphs in one frame (no begin_frame, so none are
        // evictable) until the mask atlas overflows and grows.
        let glyph = 20u32;
        let coverage = (glyph * glyph) as usize;
        let mut first = None;
        for cp in 0..300u32 {
            let info = atlas.get_or_insert_procedural(&device, &queue, cp, glyph, glyph, || {
                vec![255u8; coverage]
            });
            if cp == 0 {
                first = info;
            }
        }

        let (grown, _) = atlas.texture_dims();
        assert_eq!(grown, initial * 2, "mask atlas doubled to fit the glyphs");

        // The first glyph's coordinates are preserved across the grow, so its
        // solid coverage must survive the texture-to-texture copy. Its pixel
        // position is the pre-grow uv scaled by the pre-grow size.
        let first = first.expect("first glyph packed");
        let x = (first.uv[0] * initial as f32).round() as u32;
        let y = (first.uv[1] * initial as f32).round() as u32;

        let pixels = read_mask(&device, &queue, &atlas.mask.texture, grown);
        let at = |x: u32, y: u32| pixels[(y * grown + x) as usize];
        assert_eq!(at(x + 2, y + 2), 255, "copied glyph coverage intact");
        assert_eq!(
            at(x + glyph / 2, y + glyph / 2),
            255,
            "copied glyph centre intact"
        );
    }

    fn read_mask(device: &Device, queue: &Queue, texture: &Texture, size: u32) -> Vec<u8> {
        let buffer = device.create_buffer(&BufferDescriptor {
            label: Some("atlas grow readback"),
            size: u64::from(size) * u64::from(size),
            usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor::default());
        encoder.copy_texture_to_buffer(
            TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: Origin3d::ZERO,
                aspect: TextureAspect::All,
            },
            TexelCopyBufferInfo {
                buffer: &buffer,
                layout: TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(size),
                    rows_per_image: None,
                },
            },
            Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(Some(encoder.finish()));

        buffer.slice(..).map_async(MapMode::Read, |_| {});
        device
            .poll(PollType::wait_indefinitely())
            .expect("poll readback");
        buffer.slice(..).get_mapped_range().to_vec()
    }
}
