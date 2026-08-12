use std::fmt;

/// Conversion strategy selected for a GeoTIFF → COG request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvertPath {
    RemuxIdentity,
    TranscodeRemux,
    HybridCropRemux,
    PlanarBandPermute,
    ChunkyBandPermute,
    StripEncode,
    TiledEncode,
}

impl fmt::Display for ConvertPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RemuxIdentity => write!(f, "remux-identity"),
            Self::TranscodeRemux => write!(f, "transcode-remux"),
            Self::HybridCropRemux => write!(f, "hybrid-crop-remux"),
            Self::PlanarBandPermute => write!(f, "planar-band-permute"),
            Self::ChunkyBandPermute => write!(f, "chunky-band-permute"),
            Self::StripEncode => write!(f, "strip-encode"),
            Self::TiledEncode => write!(f, "tiled-encode"),
        }
    }
}

pub fn log_convert_path(path: ConvertPath, show: bool) {
    if show {
        eprintln!("path: {path}");
    }
}
