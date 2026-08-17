use std::path::Path;

use crate::open::is_http_source;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFormat {
    Jp2,
    GeoTiff,
}

pub fn detect_source(source: &str) -> InputFormat {
    if is_http_source(source) {
        return detect_http_source(source);
    }
    detect(Path::new(source))
}

fn detect_http_source(source: &str) -> InputFormat {
    let path = source.split('?').next().unwrap_or(source);
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".jp2") || lower.ends_with(".j2k") || lower.ends_with(".j2c") {
        InputFormat::Jp2
    } else {
        InputFormat::GeoTiff
    }
}

pub fn detect(path: &Path) -> InputFormat {
    sniff_magic(path).unwrap_or_else(|| {
        if is_jp2_extension(path) {
            InputFormat::Jp2
        } else {
            InputFormat::GeoTiff
        }
    })
}

fn is_jp2_extension(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("jp2" | "j2k" | "j2c")
    )
}

fn sniff_magic(path: &Path) -> Option<InputFormat> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut header = [0u8; 16];
    let read = file.read(&mut header).ok()?;
    if read < 4 {
        return None;
    }

    if header.starts_with(b"II\x2A\x00") || header.starts_with(b"MM\x00\x2A") {
        return Some(InputFormat::GeoTiff);
    }

    if read >= 12 && &header[4..8] == b"jP  " {
        return Some(InputFormat::Jp2);
    }

    if read >= 8 && header[4..8] == *b"ftyp" {
        return Some(InputFormat::Jp2);
    }

    None
}
