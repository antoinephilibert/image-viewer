//! Vignettes : cache disque freedesktop (`~/.cache/thumbnails/large/`) et
//! calcul de la grille.
//!
//! Spec : https://specifications.freedesktop.org/thumbnail-spec/latest/
//! Nom de fichier = MD5 de l'URI `file://` ; PNG de 256 px max avec les
//! métadonnées `Thumb::URI` et `Thumb::MTime`.

use std::fs;
use std::io::{BufRead, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use eframe::egui::ColorImage;

use crate::formats;

/// Côté maximal des vignettes « large » de la spec.
pub const THUMB_SIZE: usize = 256;

/// URI `file://` avec encodage pourcent (octets non réservés conservés).
pub fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut uri = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                uri.push(b as char);
            }
            _ => uri.push_str(&format!("%{b:02X}")),
        }
    }
    uri
}

/// Clé du cache : MD5 hexadécimal de l'URI.
pub fn cache_key(uri: &str) -> String {
    format!("{:x}", md5::compute(uri.as_bytes()))
}

fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("thumbnails").join("large"))
}

fn cache_path(uri: &str) -> Option<PathBuf> {
    Some(cache_dir()?.join(format!("{}.png", cache_key(uri))))
}

fn mtime_secs(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_secs())
}

/// Métadonnées lues dans le PNG.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Meta {
    pub uri: Option<String>,
    pub mtime: Option<u64>,
}

/// Encode une vignette RGBA 8 bits (non prémultipliée) avec ses métadonnées.
pub fn encode_png(
    out: impl Write,
    width: u32,
    height: u32,
    rgba: &[u8],
    uri: &str,
    mtime: u64,
) -> Result<(), String> {
    let mut enc = png::Encoder::new(out, width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    for (k, v) in [
        ("Thumb::URI", uri.to_string()),
        ("Thumb::MTime", mtime.to_string()),
        ("Software", "vu".to_string()),
    ] {
        enc.add_text_chunk(k.to_string(), v)
            .map_err(|e| e.to_string())?;
    }
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgba).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())
}

/// Décode une vignette PNG (tout type de couleur 8 bits après expansion).
pub fn decode_png(input: impl BufRead + Seek) -> Result<(Meta, ColorImage), String> {
    let mut dec = png::Decoder::new(input);
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("taille invalide")?];
    let frame = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    // Les blocs texte peuvent suivre les données d'image.
    reader.finish().map_err(|e| e.to_string())?;
    let (w, h) = (frame.width as usize, frame.height as usize);
    let data = &buf[..frame.buffer_size()];

    let rgba: Vec<u8> = match frame.color_type {
        png::ColorType::Rgba => data.to_vec(),
        png::ColorType::Rgb => data
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => data
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => data.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return Err("PNG indexé non étendu".into()),
    };

    let info = reader.info();
    let mut meta = Meta::default();
    let texts = info
        .uncompressed_latin1_text
        .iter()
        .map(|c| (c.keyword.clone(), c.text.clone()))
        .chain(
            info.utf8_text
                .iter()
                .filter_map(|c| Some((c.keyword.clone(), c.get_text().ok()?))),
        );
    for (k, v) in texts {
        match k.as_str() {
            "Thumb::URI" => meta.uri = Some(v),
            "Thumb::MTime" => meta.mtime = v.trim().parse().ok(),
            _ => {}
        }
    }
    Ok((meta, ColorImage::from_rgba_unmultiplied([w, h], &rgba)))
}

/// Une vignette est à jour si URI et date de modification correspondent.
pub fn is_fresh(meta: &Meta, uri: &str, mtime: u64) -> bool {
    meta.uri.as_deref() == Some(uri) && meta.mtime == Some(mtime)
}

/// Charge la vignette d'une image : lue sur disque si à jour, sinon générée
/// puis écrite dans le cache partagé.
pub fn load(path: &Path) -> Result<ColorImage, String> {
    let uri = file_uri(path);
    let mtime = mtime_secs(path).ok_or("date de modification illisible")?;
    let cached = cache_path(&uri);

    if let Some(file) = cached.as_ref().and_then(|p| fs::File::open(p).ok())
        && let Ok((meta, img)) = decode_png(std::io::BufReader::new(file))
        && is_fresh(&meta, &uri, mtime)
    {
        return Ok(img);
    }

    let (rgba, _) = formats::decode_rgba(path, THUMB_SIZE)?;
    if let Some(dest) = cached {
        // Échec d'écriture sans gravité : la vignette reste utilisable.
        let _ = store(
            &dest,
            rgba.width(),
            rgba.height(),
            rgba.as_raw(),
            &uri,
            mtime,
        );
    }
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

/// Écriture atomique (fichier temporaire puis renommage), dossiers en 0700.
fn store(dest: &Path, w: u32, h: u32, rgba: &[u8], uri: &str, mtime: u64) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = dest.parent().ok_or("chemin invalide")?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| e.to_string())?;
    let tmp = dir.join(format!(".vu-{}-{}.tmp", std::process::id(), cache_key(uri)));
    let result = fs::File::create(&tmp)
        .map_err(|e| e.to_string())
        .and_then(|f| encode_png(std::io::BufWriter::new(f), w, h, rgba, uri, mtime))
        .and_then(|()| fs::rename(&tmp, dest).map_err(|e| e.to_string()));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Nombre de colonnes de la grille pour une largeur donnée.
pub fn grid_columns(width: f32, target_cell: f32) -> usize {
    ((width / target_cell).floor() as usize).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_is_percent_encoded() {
        assert_eq!(
            file_uri(Path::new("/home/a b/é.jpg")),
            "file:///home/a%20b/%C3%A9.jpg"
        );
    }

    #[test]
    fn key_is_md5_of_uri() {
        // Valeur de référence obtenue avec `md5sum` sur l'URI (sans retour à la ligne).
        assert_eq!(
            cache_key("file:///home/jens/photo/me.png"),
            "d40775e596682f2a16d1b834c221c0a2"
        );
    }

    #[test]
    fn png_roundtrip_keeps_metadata() {
        let rgba: Vec<u8> = (0..4 * 3 * 4).map(|i| i as u8).collect();
        let mut out = Vec::new();
        encode_png(&mut out, 4, 3, &rgba, "file:///x.png", 1234).unwrap();
        let (meta, img) = decode_png(std::io::Cursor::new(out)).unwrap();
        assert_eq!(meta.uri.as_deref(), Some("file:///x.png"));
        assert_eq!(meta.mtime, Some(1234));
        assert_eq!(img.size, [4, 3]);
        assert!(is_fresh(&meta, "file:///x.png", 1234));
        assert!(!is_fresh(&meta, "file:///x.png", 1235));
        assert!(!is_fresh(&meta, "file:///y.png", 1234));
    }

    #[test]
    fn columns_adapt_to_width() {
        assert_eq!(grid_columns(50.0, 110.0), 1);
        assert_eq!(grid_columns(280.0, 110.0), 2);
        assert_eq!(grid_columns(400.0, 110.0), 3);
    }

    /// Mesure (à lancer à la main) : `VU_BENCH_DIR=<dossier> cargo test --release
    /// bench_jpeg -- --ignored --nocapture`. Image synthétique de 20 Mpx.
    #[test]
    #[ignore = "mesure manuelle"]
    fn bench_jpeg_thumbnail_generation() {
        use std::time::Instant;
        let dir = PathBuf::from(std::env::var("VU_BENCH_DIR").expect("VU_BENCH_DIR"));
        let path = dir.join("bench-20mp.jpg");
        let (w, h) = (5472u32, 3648u32);
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            // Dégradé + bruit déterministe pour une entropie proche d'une photo.
            let n = (x.wrapping_mul(2654435761) ^ y.wrapping_mul(40503)) >> 24;
            image::Rgb([
                ((x * 255 / w) as u8).wrapping_add(n as u8 / 8),
                ((y * 255 / h) as u8).wrapping_add(n as u8 / 8),
                (((x + y) * 255 / (w + h)) as u8).wrapping_add(n as u8 / 8),
            ])
        });
        let f = std::io::BufWriter::new(fs::File::create(&path).unwrap());
        image::codecs::jpeg::JpegEncoder::new_with_quality(f, 90)
            .encode_image(&img)
            .unwrap();
        println!(
            "fichier : {} Mo",
            fs::metadata(&path).unwrap().len() / 1_000_000
        );

        for _ in 0..3 {
            // Chemin générique : décodage complet puis réduction.
            let t = Instant::now();
            let (full, _) = formats::decode_generic(&path, usize::MAX).unwrap();
            let t_full = t.elapsed();
            let t = Instant::now();
            let small = formats::resize_rgba(full, 256, 171).unwrap();
            let t_resize = t.elapsed();
            // Chemin turbojpeg : décodage réduit par DCT.
            let t = Instant::now();
            let (fast, _) = formats::decode_rgba(&path, THUMB_SIZE).unwrap();
            let t_turbo = t.elapsed();
            let t = Instant::now();
            let mut out = Vec::new();
            encode_png(&mut out, 256, 171, small.as_raw(), "file:///x", 1).unwrap();
            let t_png = t.elapsed();
            println!(
                "générique : décodage {t_full:?} + réduction {t_resize:?} = {:?} | turbojpeg : {t_turbo:?} ({}x{}) | écriture PNG {t_png:?}",
                t_full + t_resize,
                fast.width(),
                fast.height()
            );
        }
    }
}
