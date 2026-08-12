mod format;
mod profile;

pub use format::{detect, detect_source, InputFormat};
pub use profile::{
    apply_georef, projected_georef, GeorefProfile, RasterProfile, SampleLayout,
};
