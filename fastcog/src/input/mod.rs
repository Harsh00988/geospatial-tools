mod format;
mod profile;

pub use format::{detect, InputFormat};
pub use profile::{apply_georef, projected_georef, GeorefProfile, RasterProfile};
