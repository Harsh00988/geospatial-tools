mod dimensions;
mod format;
mod profile;

pub use dimensions::{input_dimensions, is_jp2_path};

pub use format::{detect, detect_source, InputFormat};
pub use profile::{
    apply_georef, projected_georef, GeorefProfile, RasterProfile, SampleLayout,
};
