//! Détection des formats par extension et décodage des images raster.

use std::path::Path;

use eframe::egui::ColorImage;
use image::{DynamicImage, ImageDecoder, ImageReader, imageops::FilterType};

/// Famille de format reconnue par extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Raster,
}

const RASTER_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "gif", "bmp", "tif", "tiff",
];

/// Renvoie le type de l'image selon l'extension, `None` si non supporté.
pub fn kind_of(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    RASTER_EXTENSIONS
        .contains(&ext.as_str())
        .then_some(Kind::Raster)
}

/// Image décodée, prête à devenir une texture.
pub struct Decoded {
    pub pixels: ColorImage,
    /// Dimensions réelles de l'image (après orientation EXIF, avant réduction).
    pub original_size: [usize; 2],
}

/// Décode une image raster, applique l'orientation EXIF et la réduit si un
/// côté dépasse `max_side` (limite de texture du GPU).
pub fn decode(path: &Path, max_side: usize) -> Result<Decoded, String> {
    let reader = ImageReader::open(path)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().map_err(|e| e.to_string())?;
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);

    let original_size = [img.width() as usize, img.height() as usize];
    let largest = original_size[0].max(original_size[1]);
    if largest > max_side {
        let ratio = max_side as f64 / largest as f64;
        let w = ((original_size[0] as f64 * ratio) as u32).max(1);
        let h = ((original_size[1] as f64 * ratio) as u32).max(1);
        img = img.resize_exact(w, h, FilterType::Triangle);
    }

    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(Decoded {
        pixels: ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()),
        original_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_extensions_case_insensitively() {
        assert_eq!(kind_of(Path::new("a/B.JPG")), Some(Kind::Raster));
        assert_eq!(kind_of(Path::new("x.webp")), Some(Kind::Raster));
        assert_eq!(kind_of(Path::new("notes.txt")), None);
        assert_eq!(kind_of(Path::new("noext")), None);
    }
}
