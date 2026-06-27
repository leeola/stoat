//! GPU glyph atlas: rasterize once, pack, cache, and reuse.
//!
//! A [`GlyphAtlas`] holds two texture atlases, selected per glyph by its
//! rasterized content: an `R8Unorm` mask atlas for outline glyphs and an
//! `Rgba8Unorm` color atlas for emoji. Glyphs are rasterized through
//! cosmic-text's [`SwashCache`], packed into the texture with an etagere
//! allocator, and cached by [`CacheKey`] so repeated lookups are free.
//!
//! When an atlas fills, least-recently-used glyphs not needed this frame are
//! evicted; if eviction cannot free enough room the texture grows (doubling,
//! re-uploading every retained glyph at its preserved coordinates).

use cosmic_text::{CacheKey, FontSystem, SwashCache, SwashImage};
use etagere::{size2, AllocId, Allocation, BucketedAtlasAllocator};
use lru::LruCache;
use rustc_hash::FxBuildHasher;
use std::collections::HashSet;
use swash::scale::image::Content;
use wgpu::{
    Device, Extent3d, Origin3d, Queue, TexelCopyBufferLayout, TexelCopyTextureInfo, Texture,
    TextureAspect, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages, TextureView,
    TextureViewDescriptor,
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

/// What a cached glyph was rasterized from.
///
/// Font glyphs are re-rasterizable through swash from their [`CacheKey`], while
/// procedurally drawn glyphs are sized to a specific cell and kept as pixels.
/// Both share one atlas, packer, and eviction order; the distinct variants keep
/// a procedural glyph from colliding with the font glyph for the same codepoint.
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

        atlas.insert_image(device, queue, font_system, swash_cache, id, &image)
    }

    /// Look up a procedurally drawn glyph, rasterizing and caching it on first
    /// use.
    ///
    /// `render` produces the `width`x`height` R8 coverage only on a cache miss;
    /// the glyph is stored with those pixels so an atlas grow re-uploads it
    /// without re-running `render`. Procedural glyphs always live in the mask
    /// atlas. `None` when the atlas is full and nothing can be evicted, or when
    /// `render` yields no pixels.
    #[allow(clippy::too_many_arguments)]
    pub fn get_or_insert_procedural(
        &mut self,
        device: &Device,
        queue: &Queue,
        font_system: &mut FontSystem,
        swash_cache: &mut SwashCache,
        cp: u32,
        width: u32,
        height: u32,
        render: impl FnOnce() -> Vec<u8>,
    ) -> Option<GlyphInfo> {
        let id = CacheId::Procedural { cp, width, height };
        if let Some(hit) = self.mask.lookup(id) {
            return hit;
        }

        self.mask.insert_pixels(
            device,
            queue,
            font_system,
            swash_cache,
            id,
            width,
            height,
            render(),
        )
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

    /// Pack and cache a font glyph's swash bitmap, with no retained pixels: a
    /// later grow re-rasterizes it through swash from its [`CacheKey`].
    fn insert_image(
        &mut self,
        device: &Device,
        queue: &Queue,
        font_system: &mut FontSystem,
        swash_cache: &mut SwashCache,
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

        let allocation = self.allocate(device, queue, font_system, swash_cache, width, height)?;
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
            pixels: None,
        };
        let info = glyph_info(&cached, self.kind, self.size);
        self.cache.put(id, cached);
        self.in_use.insert(id);

        info
    }

    /// Pack and cache a procedurally drawn glyph, retaining its `pixels` so a
    /// grow re-uploads them rather than re-rasterizing through swash.
    #[allow(clippy::too_many_arguments)]
    fn insert_pixels(
        &mut self,
        device: &Device,
        queue: &Queue,
        font_system: &mut FontSystem,
        swash_cache: &mut SwashCache,
        id: CacheId,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    ) -> Option<GlyphInfo> {
        if width == 0 || height == 0 || pixels.is_empty() {
            return None;
        }

        let allocation = self.allocate(device, queue, font_system, swash_cache, width, height)?;
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
            pixels: Some(pixels),
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
        font_system: &mut FontSystem,
        swash_cache: &mut SwashCache,
        width: u32,
        height: u32,
    ) -> Option<Allocation> {
        loop {
            if let Some(allocation) = self.try_allocate(width, height) {
                return Some(allocation);
            }
            if !self.grow(device, queue, font_system, swash_cache) {
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
        }
    }

    /// Double the atlas (up to the device limit) and re-upload every retained
    /// glyph; etagere preserves existing coordinates across the grow, so only
    /// the texture and size change. `false` if already at the device limit.
    fn grow(
        &mut self,
        device: &Device,
        queue: &Queue,
        font_system: &mut FontSystem,
        swash_cache: &mut SwashCache,
    ) -> bool {
        if self.size >= self.max_dim {
            return false;
        }

        let new_size = (self.size * 2).min(self.max_dim);
        self.packer.grow(size2(new_size as i32, new_size as i32));
        self.texture = create_texture(device, self.kind, new_size);

        let channels = num_channels(self.kind);
        for (id, glyph) in &self.cache {
            if glyph.alloc.is_none() {
                continue;
            }

            let origin = [glyph.x, glyph.y];
            let size = [glyph.width, glyph.height];
            if let Some(pixels) = &glyph.pixels {
                write_glyph(queue, &self.texture, channels, origin, size, pixels);
            } else if let CacheId::Font(key) = id {
                let Some(image) = swash_cache.get_image_uncached(font_system, *key) else {
                    continue;
                };
                write_glyph(queue, &self.texture, channels, origin, size, &image.data);
            }
        }

        self.view = self.texture.create_view(&TextureViewDescriptor::default());
        self.size = new_size;
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
    /// `Some` for a procedurally drawn glyph, holding its coverage bytes so a
    /// grow re-uploads it without a font; `None` for a font glyph, which is
    /// re-rasterized through swash on grow.
    pixels: Option<Vec<u8>>,
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
            pixels: None,
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
        usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
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
    use super::uv_rect;

    #[test]
    fn uv_rect_normalizes_to_atlas_size() {
        assert_eq!(uv_rect(64, 32, 16, 8, 256), [0.25, 0.125, 0.3125, 0.15625]);
        assert_eq!(uv_rect(0, 0, 256, 256, 256), [0.0, 0.0, 1.0, 1.0]);
    }
}
