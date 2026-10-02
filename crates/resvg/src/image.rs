// Copyright 2018 the Resvg Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

/// Raster images decoded during one render, so that an image drawn several
/// times (e.g. through many `use` elements) is not decoded again each time.
///
/// An image is kept decoded only once it has been drawn twice: images drawn
/// a single time, the common case, cost no more memory than before.
///
/// usvg creates a new data buffer for every instance of an image (each `use`
/// decodes the data URL again), so entries are matched by content, after a
/// cheap pointer and length check.
#[derive(Default)]
pub struct RasterCache {
    #[cfg_attr(not(feature = "raster-images"), allow(dead_code))]
    entries: RefCell<Vec<(Arc<Vec<u8>>, CachedRaster)>>,
}

#[cfg_attr(not(feature = "raster-images"), allow(dead_code))]
enum CachedRaster {
    /// Drawn once, not kept.
    Seen,
    /// Drawn at least twice. `None` when decoding failed.
    Decoded(Option<Rc<tiny_skia::Pixmap>>),
}

pub fn render(
    image: &usvg::Image,
    #[allow(unused_variables)] ctx: &crate::render::Context,
    transform: tiny_skia::Transform,
    pixmap: &mut tiny_skia::PixmapMut,
) {
    if !image.is_visible() {
        return;
    }

    match image.kind() {
        usvg::ImageKind::SVG(tree) => {
            render_vector(tree, transform, pixmap);
        }
        #[cfg(feature = "raster-images")]
        kind => {
            if let Some(raster) = ctx.raster_cache.get_or_decode(kind) {
                raster_images::render_raster(&raster, transform, image.rendering_mode(), pixmap);
            }
        }
        #[cfg(not(feature = "raster-images"))]
        _ => {
            log::warn!("Images decoding was disabled by a build feature.");
        }
    }
}

#[cfg(feature = "raster-images")]
impl RasterCache {
    fn get_or_decode(&self, kind: &usvg::ImageKind) -> Option<Rc<tiny_skia::Pixmap>> {
        let data = match kind {
            usvg::ImageKind::JPEG(data)
            | usvg::ImageKind::PNG(data)
            | usvg::ImageKind::GIF(data)
            | usvg::ImageKind::WEBP(data) => data,
            usvg::ImageKind::SVG(_) => return None,
        };

        let mut entries = self.entries.borrow_mut();
        let entry = entries.iter_mut().find(|(cached, _)| {
            Arc::ptr_eq(cached, data) || (cached.len() == data.len() && cached[..] == data[..])
        });

        match entry {
            Some((_, CachedRaster::Decoded(raster))) => raster.clone(),
            Some((_, cached)) => {
                let raster = raster_images::decode_raster(kind).map(Rc::new);
                *cached = CachedRaster::Decoded(raster.clone());
                raster
            }
            None => {
                entries.push((data.clone(), CachedRaster::Seen));
                raster_images::decode_raster(kind).map(Rc::new)
            }
        }
    }
}

fn render_vector(
    tree: &usvg::Tree,
    transform: tiny_skia::Transform,
    pixmap: &mut tiny_skia::PixmapMut,
) -> Option<()> {
    let mut sub_pixmap = tiny_skia::Pixmap::new(pixmap.width(), pixmap.height()).unwrap();
    crate::render(tree, transform, &mut sub_pixmap.as_mut());
    pixmap.draw_pixmap(
        0,
        0,
        sub_pixmap.as_ref(),
        &tiny_skia::PixmapPaint::default(),
        tiny_skia::Transform::default(),
        None,
    );

    Some(())
}

#[cfg(feature = "raster-images")]
mod raster_images {
    use crate::OptionLog;
    use std::io::Cursor;
    use usvg::ImageRendering;

    pub(crate) fn decode_raster(image: &usvg::ImageKind) -> Option<tiny_skia::Pixmap> {
        match image {
            usvg::ImageKind::SVG(_) => None,
            usvg::ImageKind::JPEG(data) => {
                decode_jpeg(data).log_none(|| log::warn!("Failed to decode a JPEG image."))
            }
            usvg::ImageKind::PNG(data) => {
                decode_png(data).log_none(|| log::warn!("Failed to decode a PNG image."))
            }
            usvg::ImageKind::GIF(data) => {
                decode_gif(data).log_none(|| log::warn!("Failed to decode a GIF image."))
            }
            usvg::ImageKind::WEBP(data) => {
                decode_webp(data).log_none(|| log::warn!("Failed to decode a WebP image."))
            }
        }
    }

    fn decode_png(data: &[u8]) -> Option<tiny_skia::Pixmap> {
        tiny_skia::Pixmap::decode_png(data).ok()
    }

    fn decode_jpeg(data: &[u8]) -> Option<tiny_skia::Pixmap> {
        use zune_jpeg::zune_core::colorspace::ColorSpace;
        use zune_jpeg::zune_core::options::DecoderOptions;

        let cursor = Cursor::new(data);
        let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
        let mut decoder = zune_jpeg::JpegDecoder::new_with_options(cursor, options);
        decoder.decode_headers().ok()?;
        let output_cs = decoder.output_colorspace()?;

        let img_data = {
            let data = decoder.decode().ok()?;
            match output_cs {
                ColorSpace::RGBA => data,
                _ => return None,
            }
        };

        let info = decoder.info()?;

        let size = tiny_skia::IntSize::from_wh(info.width as u32, info.height as u32)?;
        tiny_skia::Pixmap::from_vec(img_data, size)
    }

    fn decode_gif(data: &[u8]) -> Option<tiny_skia::Pixmap> {
        let mut decoder = gif::DecodeOptions::new();
        decoder.set_color_output(gif::ColorOutput::RGBA);
        let mut decoder = decoder.read_info(data).ok()?;
        let first_frame = decoder.read_next_frame().ok()??;

        let size = tiny_skia::IntSize::from_wh(
            u32::from(first_frame.width),
            u32::from(first_frame.height),
        )?;

        let (w, h) = size.dimensions();
        let mut pixmap = tiny_skia::Pixmap::new(w, h)?;
        rgba_to_pixmap(&first_frame.buffer, &mut pixmap);
        Some(pixmap)
    }

    fn decode_webp(data: &[u8]) -> Option<tiny_skia::Pixmap> {
        let mut decoder = image_webp::WebPDecoder::new(std::io::Cursor::new(data)).ok()?;
        let mut first_frame = vec![0; decoder.output_buffer_size()?];
        decoder.read_image(&mut first_frame).ok()?;

        let (w, h) = decoder.dimensions();
        let mut pixmap = tiny_skia::Pixmap::new(w, h)?;

        if decoder.has_alpha() {
            rgba_to_pixmap(&first_frame, &mut pixmap);
        } else {
            rgb_to_pixmap(&first_frame, &mut pixmap);
        }

        Some(pixmap)
    }

    fn rgb_to_pixmap(data: &[u8], pixmap: &mut tiny_skia::Pixmap) {
        use rgb::FromSlice;

        let mut i = 0;
        let dst = pixmap.data_mut();
        for p in data.as_rgb() {
            dst[i + 0] = p.r;
            dst[i + 1] = p.g;
            dst[i + 2] = p.b;
            dst[i + 3] = 255;

            i += tiny_skia::BYTES_PER_PIXEL;
        }
    }

    fn rgba_to_pixmap(data: &[u8], pixmap: &mut tiny_skia::Pixmap) {
        use rgb::FromSlice;

        let mut i = 0;
        let dst = pixmap.data_mut();
        for p in data.as_rgba() {
            let a = p.a as f64 / 255.0;
            dst[i + 0] = (p.r as f64 * a + 0.5) as u8;
            dst[i + 1] = (p.g as f64 * a + 0.5) as u8;
            dst[i + 2] = (p.b as f64 * a + 0.5) as u8;
            dst[i + 3] = p.a;

            i += tiny_skia::BYTES_PER_PIXEL;
        }
    }

    pub(crate) fn render_raster(
        raster: &tiny_skia::Pixmap,
        transform: tiny_skia::Transform,
        rendering_mode: usvg::ImageRendering,
        pixmap: &mut tiny_skia::PixmapMut,
    ) -> Option<()> {
        let rect = tiny_skia::Size::from_wh(raster.width() as f32, raster.height() as f32)?
            .to_rect(0.0, 0.0)?;

        let quality = match rendering_mode {
            ImageRendering::OptimizeQuality => tiny_skia::FilterQuality::Bicubic,
            ImageRendering::OptimizeSpeed => tiny_skia::FilterQuality::Nearest,
            ImageRendering::Smooth => tiny_skia::FilterQuality::Bilinear,
            ImageRendering::HighQuality => tiny_skia::FilterQuality::Bicubic,
            ImageRendering::CrispEdges => tiny_skia::FilterQuality::Nearest,
            ImageRendering::Pixelated => tiny_skia::FilterQuality::Nearest,
        };

        let pattern = tiny_skia::Pattern::new(
            raster.as_ref(),
            tiny_skia::SpreadMode::Pad,
            quality,
            1.0,
            tiny_skia::Transform::default(),
        );
        let mut paint = tiny_skia::Paint::default();
        paint.shader = pattern;

        pixmap.fill_rect(rect, &paint, transform, None);

        Some(())
    }
}
