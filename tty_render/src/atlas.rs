//! GPU glyph atlas: rasterize once, pack, cache, and reuse.
//!
//! A [`GlyphAtlas`] holds two texture atlases, selected per glyph by its
//! rasterized content: an `R8Unorm` mask atlas for outline glyphs and an
//! `Rgba8Unorm` color atlas for emoji. Glyphs are rasterized through
//! cosmic-text's [`SwashCache`], packed into the texture with an etagere
//! allocator, and cached by [`CacheKey`] so repeated lookups are free.
//!
//! When an atlas fills it grows: the texture doubles and the old one is copied
//! into it, which preserves every glyph's coordinates. Past [`ATLAS_BUDGET`] the
//! growing stops and least-recently-used glyphs the current frame has not
//! touched are evicted instead, and only when nothing can be evicted does the
//! texture grow again, toward the device limit.

use cosmic_text::{CacheKey, FontSystem, SwashCache, SwashImage};
use etagere::{size2, AllocId, Allocation, BucketedAtlasAllocator};
use lru::LruCache;
use rustc_hash::FxBuildHasher;
use swash::scale::image::Content;
use wgpu::{
    CommandEncoderDescriptor, Device, Extent3d, Origin3d, Queue, TexelCopyBufferLayout,
    TexelCopyTextureInfo, Texture, TextureAspect, TextureDescriptor, TextureDimension,
    TextureFormat, TextureUsages, TextureView, TextureViewDescriptor,
};

/// Edge length, in texels, of a freshly created atlas texture. Atlases double
/// from here as they fill, up to the device's maximum texture dimension.
///
/// Sized past a screenful of distinct glyphs, since every rung of the ladder
/// below it copies the whole texture forward to climb.
const INITIAL_SIZE: u32 = 512;

/// Edge length past which an atlas stops growing and starts evicting.
///
/// The two answers are not priced the same. Growing copies the texture forward
/// and etagere keeps every glyph's coordinates, so nothing built against the
/// atlas goes stale. Evicting hands one glyph's texels to another, which moves
/// the content epoch, and that rebuilds the whole grid, the text runs, the
/// overlay bases, and every composite slot. Under glyph pressure that recurs
/// every few frames.
///
/// So growing is tried first, and this is where memory starts to matter more
/// than the rebuilds: a mask atlas at this size is 4MB and a color one 16MB.
/// Past it, evicting is the cheaper answer, and the texture grows again only
/// when there is nothing left to evict.
const ATLAS_BUDGET: u32 = 2048;

/// Which of the two atlases a glyph lives in, so the text pass can bind the
/// matching texture and choose mask-vs-color blending.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AtlasKind {
    Mask,
    Color,
}

/// Where a rasterized glyph sits in its atlas, for the text pass to draw it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlyphInfo {
    pub kind: AtlasKind,
    /// Atlas coordinates in texels, as `[x_min, y_min, x_max, y_max]`.
    ///
    /// Texels rather than the normalized coordinates a sampler wants, because
    /// normalizing here would tie every cached instance to the atlas size it
    /// was cut at, and a grow would invalidate the lot. The shader divides by
    /// the size the globals carry instead. Etagere keeps a glyph's texels
    /// across a grow, so these survive one.
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
        self.mask.frame = self.mask.frame.wrapping_add(1);
        self.color.frame = self.color.frame.wrapping_add(1);
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
    /// Each doubles when a glyph no longer fits, which replaces its texture and
    /// so its view. A caller compares this across a frame to tell whether a
    /// bind group holding those views is stale, and divides a glyph's texel
    /// coordinates by it to sample.
    pub fn texture_dims(&self) -> (u32, u32) {
        (self.mask.size, self.color.size)
    }

    /// A generation counter that changes whenever any packed glyph moves,
    /// across both the mask and color atlases.
    ///
    /// An eviction in either atlas bumps it, a freed slot being one another
    /// glyph packs into. A grow does not: the texture doubles and every glyph
    /// keeps its texels. A caller that cached glyph instances against an
    /// earlier frame's atlas may reuse them only while this is unchanged.
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
    /// Frame this atlas is packing, bumped once per frame. A glyph counts as
    /// this frame's when its stamp matches, which is what protects it from
    /// eviction, so nothing has to be reset between frames.
    frame: u64,
    /// Bumped whenever a packed glyph moves, which only an eviction does. It
    /// frees a slot another glyph then packs into, so a cached instance naming
    /// those texels would draw the wrong pixels.
    ///
    /// A grow leaves this alone. Every glyph keeps the texels it was cut at,
    /// which is what a cached instance names.
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
            frame: 0,
            epoch: 0,
        }
    }

    /// `Some` if `id` is cached (inner value `None` for an empty glyph),
    /// `None` if it must be rasterized.
    fn lookup(&mut self, id: CacheId) -> Option<Option<GlyphInfo>> {
        let (kind, frame) = (self.kind, self.frame);
        let cached = self.cache.get_mut(&id)?;
        cached.used_frame = frame;
        Some(glyph_info(cached, kind))
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
                CachedGlyph::empty(image.placement.left, image.placement.top, self.frame),
            );
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
            used_frame: self.frame,
        };
        let info = glyph_info(&cached, self.kind);
        self.cache.put(id, cached);

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
            used_frame: self.frame,
        };
        let info = glyph_info(&cached, self.kind);
        self.cache.put(id, cached);

        info
    }

    /// Reserve a slot for a `width`x`height` glyph.
    ///
    /// A full packer is answered in the order the answers cost: grow while under
    /// [`ATLAS_BUDGET`], since a grow leaves every glyph where it was; then
    /// evict a glyph this frame has not touched, which moves the ones cached
    /// against it; then grow again toward the device limit, which is what a
    /// frame using more than the budget holds is left with.
    ///
    /// `None` only when the atlas is at the device limit and every sized glyph
    /// is needed this frame.
    fn allocate(
        &mut self,
        device: &Device,
        queue: &Queue,
        width: u32,
        height: u32,
    ) -> Option<Allocation> {
        let size = size2(width as i32, height as i32);
        loop {
            if let Some(allocation) = self.packer.allocate(size) {
                return Some(allocation);
            }
            if self.size < ATLAS_BUDGET && self.grow(device, queue) {
                continue;
            }
            if self.evict() {
                continue;
            }
            if self.grow(device, queue) {
                continue;
            }
            return None;
        }
    }

    /// Drop the least-recently-used glyph this frame has not touched, freeing
    /// the atlas space it held.
    ///
    /// `false` when the least-recently-used glyph left is one this frame drew,
    /// which is what keeps a frame from evicting a glyph it is about to hand
    /// back. The caller grows instead.
    ///
    /// An empty glyph holds no space, so dropping one frees nothing and the
    /// walk carries on past it.
    fn evict(&mut self) -> bool {
        loop {
            let Some((_, glyph)) = self.cache.peek_lru() else {
                return false;
            };
            let (used_frame, alloc) = (glyph.used_frame, glyph.alloc);
            if used_frame == self.frame {
                return false;
            }

            self.cache.pop_lru();
            let Some(alloc) = alloc else {
                continue;
            };

            self.packer.deallocate(alloc);
            self.epoch = self.epoch.wrapping_add(1);
            return true;
        }
    }

    /// Double the atlas (up to the device limit) and copy the old texture into
    /// the new one. etagere preserves existing coordinates across the grow, so
    /// a single GPU texture-to-texture copy relocates every packed glyph.
    ///
    /// [`ATLAS_BUDGET`] is not enforced here. It is the point the ladder in
    /// [`Self::allocate`] stops preferring this to an eviction, not a size the
    /// atlas may never reach.
    ///
    /// The epoch holds. A glyph keeps the texels it was cut at, and a cached
    /// instance names those rather than a fraction of the atlas, so nothing
    /// that was built against this atlas has to be built again.
    ///
    /// `false` if already at the device limit.
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
    /// Frame this glyph was last looked up or inserted on. It equals the
    /// atlas's own counter exactly while the glyph is this frame's, which is
    /// what keeps it from being evicted to make room for another.
    used_frame: u64,
}

impl CachedGlyph {
    fn empty(left: i32, top: i32, used_frame: u64) -> CachedGlyph {
        CachedGlyph {
            alloc: None,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            left,
            top,
            used_frame,
        }
    }
}

/// The atlas placement of `glyph` for the text pass to draw, or `None` for an
/// empty glyph that occupies no atlas space.
fn glyph_info(glyph: &CachedGlyph, kind: AtlasKind) -> Option<GlyphInfo> {
    if glyph.width == 0 || glyph.height == 0 {
        return None;
    }

    Some(GlyphInfo {
        kind,
        uv: uv_rect(glyph.x, glyph.y, glyph.width, glyph.height),
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

/// A glyph's rectangle in texels, which the shader normalizes against the atlas
/// it is sampled from.
fn uv_rect(x: u32, y: u32, width: u32, height: u32) -> [f32; 4] {
    [x as f32, y as f32, (x + width) as f32, (y + height) as f32]
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
    use super::{uv_rect, GlyphAtlas, ATLAS_BUDGET};
    use crate::gpu::headless_device;
    use wgpu::{
        BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Device, Extent3d, MapMode,
        Origin3d, PollType, Queue, TexelCopyBufferInfo, TexelCopyBufferLayout,
        TexelCopyTextureInfo, Texture, TextureAspect,
    };

    /// The rectangle is in texels and says nothing about the atlas holding it,
    /// which is what lets a cached instance outlive a grow.
    #[test]
    fn uv_rect_names_texels_not_a_fraction_of_the_atlas() {
        assert_eq!(uv_rect(64, 32, 16, 8), [64.0, 32.0, 80.0, 40.0]);
        assert_eq!(uv_rect(0, 0, 256, 256), [0.0, 0.0, 256.0, 256.0]);
    }

    /// The two answers to a full atlas are not priced the same. Growing copies
    /// the texture forward and every glyph keeps the texels it was cut at, so
    /// nothing built against the atlas goes stale. Evicting hands one glyph's
    /// texels to another, which is what the content epoch reports and what
    /// rebuilds everything cached against it. So while there is room to grow
    /// into, growing is the answer, whatever is evictable.
    #[test]
    fn the_atlas_grows_before_it_evicts_while_it_has_room() {
        let Some((device, queue)) = headless_device() else {
            eprintln!("atlas eviction test: no wgpu adapter, skipping");
            return;
        };

        let mut atlas = GlyphAtlas::new(&device);
        let (initial, _) = atlas.texture_dims();
        // A frame apart, so every glyph but the last is a legal victim.
        pack_procedural(&device, &queue, &mut atlas, 40, 300, PerFrame::Yes);

        assert!(
            atlas.texture_dims().0 > initial,
            "the atlas grows past the glyphs it could have evicted instead",
        );
        assert_eq!(
            atlas.content_epoch(),
            0,
            "and nothing moved, so nothing built against it is stale",
        );
    }

    /// Past the budget the atlas stops growing and starts evicting, and what it
    /// may take is anything the current frame has not touched. A frame that
    /// packs more than the budget holds has touched all of it, so there is
    /// nothing to take and the atlas grows again instead.
    #[test]
    fn past_its_budget_only_the_frame_s_own_glyphs_are_safe() {
        let Some((device, queue)) = headless_device() else {
            eprintln!("atlas budget test: no wgpu adapter, skipping");
            return;
        };

        // Each glyph is a sixteenth of the budget across, so a few hundred of
        // them ask for more than the budget holds.
        let (glyph, count) = (ATLAS_BUDGET / 16, 400);

        let mut a_frame_apart = GlyphAtlas::new(&device);
        pack_procedural(
            &device,
            &queue,
            &mut a_frame_apart,
            glyph,
            count,
            PerFrame::Yes,
        );
        assert_eq!(
            a_frame_apart.texture_dims().0,
            ATLAS_BUDGET,
            "the growing stops at the budget",
        );
        assert_ne!(
            a_frame_apart.content_epoch(),
            0,
            "and a glyph no longer needed this frame makes the room instead",
        );

        let mut within_one_frame = GlyphAtlas::new(&device);
        within_one_frame.begin_frame();
        pack_procedural(
            &device,
            &queue,
            &mut within_one_frame,
            glyph,
            count,
            PerFrame::No,
        );
        assert!(
            within_one_frame.texture_dims().0 > ATLAS_BUDGET,
            "a frame using every glyph it packed leaves nothing to evict",
        );
        assert_eq!(
            within_one_frame.content_epoch(),
            0,
            "so the atlas grows past the budget rather than taking one back",
        );
    }

    /// Whether each glyph is packed on a frame of its own, which is what leaves
    /// the ones before it evictable.
    enum PerFrame {
        Yes,
        No,
    }

    /// Pack `count` solid `glyph`-square procedural glyphs, each under its own
    /// codepoint so none is a cache hit.
    fn pack_procedural(
        device: &Device,
        queue: &Queue,
        atlas: &mut GlyphAtlas,
        glyph: u32,
        count: u32,
        per_frame: PerFrame,
    ) {
        let coverage = (glyph * glyph) as usize;
        for cp in 0..count {
            if matches!(per_frame, PerFrame::Yes) {
                atlas.begin_frame();
            }
            atlas.get_or_insert_procedural(device, queue, cp, glyph, glyph, || {
                vec![255u8; coverage]
            });
        }
    }

    #[test]
    fn grow_copies_existing_glyphs_into_the_larger_texture() {
        let Some((device, queue)) = headless_device() else {
            eprintln!("atlas grow test: no wgpu adapter, skipping");
            return;
        };

        let mut atlas = GlyphAtlas::new(&device);
        let (initial, _) = atlas.texture_dims();
        let epoch_before = atlas.content_epoch();

        // Solid glyphs in one frame (no begin_frame, so none are evictable)
        // until the mask atlas overflows and grows.
        let glyph = 40u32;
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
        assert_eq!(
            atlas.content_epoch(),
            epoch_before,
            "nothing was evicted, so no instance built against this atlas is stale",
        );

        // The first glyph's coordinates are preserved across the grow, so its
        // solid coverage must survive the texture-to-texture copy. Its
        // rectangle is in texels, which the grow leaves where they were.
        let first = first.expect("first glyph packed");
        assert_eq!(
            atlas
                .get_or_insert_procedural(&device, &queue, 0, glyph, glyph, Vec::new)
                .map(|info| info.uv),
            Some(first.uv),
            "and looking it up in the grown atlas gives the same texels",
        );
        let x = first.uv[0].round() as u32;
        let y = first.uv[1].round() as u32;

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
