//! Format detection by extension and raster image decoding.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use eframe::egui::ColorImage;
use fast_image_resize::images::Image as FirImage;
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader, RgbaImage};
use resvg::{tiny_skia, usvg};

/// Format family recognised by extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Raster,
    Svg,
}

const RASTER_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp", "tif", "tiff"];

/// Returns the image kind from its extension, `None` if unsupported.
pub fn kind_of(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if ext == "svg" {
        Some(Kind::Svg)
    } else if RASTER_EXTENSIONS.contains(&ext.as_str()) {
        Some(Kind::Raster)
    } else {
        None
    }
}

/// Maximum side of an SVG rasterisation (bounds memory: 8192² × 4 = 256 MB).
pub const MAX_SVG_SIDE: usize = 8192;

/// System fonts, loaded once (expensive operation).
fn svg_options() -> usvg::Options<'static> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    let fonts = FONTS.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_system_fonts();
        Arc::new(db)
    });
    usvg::Options {
        fontdb: fonts.clone(),
        ..usvg::Options::default()
    }
}

/// Rasterises an SVG so that its longest side is `side` pixels.
/// Returns the pixmap (premultiplied alpha) and the SVG's intrinsic size.
fn render_svg(path: &Path, side: usize) -> Result<(tiny_skia::Pixmap, [usize; 2]), String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let tree = usvg::Tree::from_data(&data, &svg_options()).map_err(|e| e.to_string())?;
    let (w, h) = (tree.size().width(), tree.size().height());
    let scale = side.clamp(1, MAX_SVG_SIDE) as f32 / w.max(h);
    let (pw, ph) = (
        ((w * scale).ceil() as u32).max(1),
        ((h * scale).ceil() as u32).max(1),
    );
    let mut pixmap = tiny_skia::Pixmap::new(pw, ph).ok_or("invalid rasterisation size")?;
    let transform = tiny_skia::Transform::from_scale(pw as f32 / w, ph as f32 / h);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Ok((pixmap, [w.ceil() as usize, h.ceil() as usize]))
}

/// Rasterised SVG, ready to become a texture.
pub fn rasterize_svg(path: &Path, side: usize) -> Result<Decoded, String> {
    let (pixmap, original_size) = render_svg(path, side)?;
    let size = [pixmap.width() as usize, pixmap.height() as usize];
    Ok(Decoded {
        pixels: ColorImage::from_rgba_premultiplied(size, pixmap.data()),
        original_size,
    })
}

/// Rasterised SVG as non-premultiplied RGBA (for PNG thumbnails).
fn rasterize_svg_rgba(path: &Path, side: usize) -> Result<(RgbaImage, [usize; 2]), String> {
    let (pixmap, original_size) = render_svg(path, side)?;
    let mut data = pixmap.data().to_vec();
    for px in data.as_chunks_mut::<4>().0 {
        let a = u32::from(px[3]);
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    let img = RgbaImage::from_raw(pixmap.width(), pixmap.height(), data)
        .ok_or_else(|| "invalid buffer".to_string())?;
    Ok((img, original_size))
}

/// Decoded image, ready to become a texture.
pub struct Decoded {
    pub pixels: ColorImage,
    /// Real image dimensions (after EXIF orientation, before downscaling).
    pub original_size: [usize; 2],
}

/// Decodes a raster image, applies the EXIF orientation and downscales it if a
/// side exceeds `max_side` (GPU texture limit).
pub fn decode(path: &Path, max_side: usize) -> Result<Decoded, String> {
    let (rgba, original_size) = decode_rgba(path, max_side)?;
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(Decoded {
        pixels: ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()),
        original_size,
    })
}

/// Like [`decode`], but returns non-premultiplied RGBA pixels.
///
/// JPEGs go through `turbojpeg` (DCT-scaled decoding when the image is larger
/// than `max_side`); on failure (CMYK, unusual file) we fall back to the
/// generic decoder.
pub fn decode_rgba(path: &Path, max_side: usize) -> Result<(RgbaImage, [usize; 2]), String> {
    if kind_of(path) == Some(Kind::Svg) {
        return rasterize_svg_rgba(path, max_side);
    }
    if is_jpeg(path)
        && let Ok(decoded) = decode_jpeg(path, max_side)
    {
        return Ok(decoded);
    }
    decode_generic(path, max_side)
}

fn is_jpeg(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"))
}

/// Strongest DCT scaling factor that keeps the longest side >= `max_side`.
fn jpeg_scaling(width: usize, height: usize, max_side: usize) -> turbojpeg::ScalingFactor {
    let largest = width.max(height);
    turbojpeg::Decompressor::supported_scaling_factors()
        .into_iter()
        .filter(|f| f.num() <= f.denom() && f.scale(largest) >= max_side)
        .min_by_key(|f| f.scale(largest))
        .unwrap_or(turbojpeg::ScalingFactor::ONE)
}

fn swaps_axes(o: Orientation) -> bool {
    matches!(
        o,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    )
}

fn decode_jpeg(path: &Path, max_side: usize) -> Result<(RgbaImage, [usize; 2]), String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    // Header only: the EXIF orientation does not require decoding the pixels.
    let orientation = ImageReader::new(std::io::Cursor::new(&data))
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .into_decoder()
        .and_then(|mut d| d.orientation())
        .unwrap_or(Orientation::NoTransforms);

    let mut dec = turbojpeg::Decompressor::new().map_err(|e| e.to_string())?;
    let header = dec.read_header(&data).map_err(|e| e.to_string())?;
    let original_size = if swaps_axes(orientation) {
        [header.height, header.width]
    } else {
        [header.width, header.height]
    };
    let largest = header.width.max(header.height);
    let factor = if largest > max_side {
        jpeg_scaling(header.width, header.height, max_side)
    } else {
        turbojpeg::ScalingFactor::ONE
    };
    dec.set_scaling_factor(factor).map_err(|e| e.to_string())?;
    let (w, h) = (factor.scale(header.width), factor.scale(header.height));
    let mut image = turbojpeg::Image {
        pixels: vec![0u8; 4 * w * h],
        width: w,
        pitch: 4 * w,
        height: h,
        format: turbojpeg::PixelFormat::RGBA,
    };
    dec.decompress(&data, image.as_deref_mut())
        .map_err(|e| e.to_string())?;
    let mut rgba = RgbaImage::from_raw(w as u32, h as u32, image.pixels)
        .ok_or_else(|| "invalid buffer".to_string())?;

    // DCT scaling stops at a multiple of 1/8: finish at the exact size.
    if w.max(h) > max_side {
        let ratio = max_side as f64 / w.max(h) as f64;
        let nw = ((w as f64 * ratio) as u32).max(1);
        let nh = ((h as f64 * ratio) as u32).max(1);
        rgba = resize_rgba(rgba, nw, nh)?;
    }
    let mut img = DynamicImage::ImageRgba8(rgba);
    img.apply_orientation(orientation);
    Ok((img.into_rgba8(), original_size))
}

pub(crate) fn decode_generic(
    path: &Path,
    max_side: usize,
) -> Result<(RgbaImage, [usize; 2]), String> {
    let reader = ImageReader::open(path)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().map_err(|e| e.to_string())?;
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);

    let original_size = [img.width() as usize, img.height() as usize];
    let mut rgba = img.into_rgba8();
    let largest = original_size[0].max(original_size[1]);
    if largest > max_side {
        let ratio = max_side as f64 / largest as f64;
        let w = ((original_size[0] as f64 * ratio) as u32).max(1);
        let h = ((original_size[1] as f64 * ratio) as u32).max(1);
        rgba = resize_rgba(rgba, w, h)?;
    }
    Ok((rgba, original_size))
}

/// SIMD downscale (`fast_image_resize`), alpha handled by the library.
pub fn resize_rgba(src: RgbaImage, width: u32, height: u32) -> Result<RgbaImage, String> {
    let (sw, sh) = (src.width(), src.height());
    let src = FirImage::from_vec_u8(sw, sh, src.into_raw(), PixelType::U8x4)
        .map_err(|e| e.to_string())?;
    let mut dst = FirImage::new(width, height, PixelType::U8x4);
    let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear));
    Resizer::new()
        .resize(&src, &mut dst, &options)
        .map_err(|e| e.to_string())?;
    RgbaImage::from_raw(width, height, dst.into_vec()).ok_or_else(|| "invalid buffer".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_halves_dimensions() {
        let img = RgbaImage::from_pixel(8, 4, image::Rgba([10, 20, 30, 255]));
        let out = resize_rgba(img, 4, 2).unwrap();
        assert_eq!(out.dimensions(), (4, 2));
        assert_eq!(out.get_pixel(0, 0).0, [10, 20, 30, 255]);
    }

    #[test]
    fn jpeg_scaling_keeps_largest_side_above_target() {
        // 5472 px, target 256: 1/8 gives 684 >= 256, the strongest factor.
        let f = jpeg_scaling(5472, 3648, 256);
        assert_eq!((f.num(), f.denom()), (1, 8));
        // Target close to the native size: no downscaling possible.
        let f = jpeg_scaling(1000, 800, 1000);
        assert_eq!(f.scale(1000), 1000);
        // Target 700 on 5472: 1/8 (684) is too small, so pick a factor >= 700.
        let f = jpeg_scaling(5472, 3648, 700);
        assert!(f.scale(5472) >= 700 && f.scale(5472) < 5472);
    }

    /// The turbojpeg path and the generic decoder must give the same image,
    /// EXIF orientation included (here 6 = 90° rotation).
    #[test]
    fn turbojpeg_matches_generic_decoder_with_exif_rotation() {
        use image::ImageEncoder;
        let (w, h) = (64u32, 32u32);
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x * 4) as u8, (y * 8) as u8, ((x + y) * 3) as u8])
        });
        // Minimal little-endian TIFF containing only the Orientation = 6 tag.
        let exif: Vec<u8> = vec![
            0x49, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00, 0x01, 0x00, 0x12, 0x01, 0x03, 0x00,
            0x01, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let mut bytes = Vec::new();
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 95);
        enc.set_exif_metadata(exif).unwrap();
        enc.write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .unwrap();
        let dir = std::env::temp_dir().join(format!("vu-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rot.jpg");
        std::fs::write(&path, &bytes).unwrap();

        let (fast, fast_size) = decode_jpeg(&path, 4096).unwrap();
        let (slow, slow_size) = decode_generic(&path, 4096).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(fast_size, [32, 64]);
        assert_eq!(fast_size, slow_size);
        assert_eq!(fast.dimensions(), (32, 64));
        assert_eq!(fast.dimensions(), slow.dimensions());
        let diff: u64 = fast
            .as_raw()
            .iter()
            .zip(slow.as_raw())
            .map(|(a, b)| u64::from(a.abs_diff(*b)))
            .sum();
        let mean = diff as f64 / fast.as_raw().len() as f64;
        assert!(mean < 3.0, "mean difference {mean}");
    }

    #[test]
    fn svg_rasterizes_at_requested_side() {
        let dir = std::env::temp_dir().join(format!("vu-svg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("r.svg");
        std::fs::write(
            &path,
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><rect width="20" height="10" fill="#ff0000"/></svg>"##,
        )
        .unwrap();
        let d = rasterize_svg(&path, 40).unwrap();
        let (img, size) = rasterize_svg_rgba(&path, 40).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(d.original_size, [20, 10]);
        assert_eq!(d.pixels.size, [40, 20]);
        assert_eq!(size, [20, 10]);
        assert_eq!(img.get_pixel(20, 10).0, [255, 0, 0, 255]);
    }

    #[test]
    fn svg_is_a_supported_kind() {
        assert_eq!(kind_of(Path::new("logo.SVG")), Some(Kind::Svg));
    }

    #[test]
    fn jpeg_extension_detection() {
        assert!(is_jpeg(Path::new("a.JPG")) && is_jpeg(Path::new("b.jpeg")));
        assert!(!is_jpeg(Path::new("c.png")));
    }

    #[test]
    fn detects_extensions_case_insensitively() {
        assert_eq!(kind_of(Path::new("a/B.JPG")), Some(Kind::Raster));
        assert_eq!(kind_of(Path::new("x.webp")), Some(Kind::Raster));
        assert_eq!(kind_of(Path::new("notes.txt")), None);
        assert_eq!(kind_of(Path::new("noext")), None);
    }
}
