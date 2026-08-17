pub mod cog;
pub mod crop;
pub mod encode;
pub mod footprint;
pub mod geo;
pub mod info;
pub mod input;
pub mod jp2;
pub mod open;
pub mod path;
pub mod progress;
pub mod chunky_permute;
pub mod hybrid_crop;
pub mod resample;
pub mod remux;
pub mod spool;
pub mod stats;
pub mod transcode;
pub mod util;
pub mod validate;
pub mod write;

pub use cog::{
    associated_alpha_band_index, tile_encoding_from_opts, CogOutputOptions, CompressionChoice,
    detect_transparency, LercAdditionalCompressionChoice, ResamplingChoice, TileJob,
    TransparencySource,
};
pub use crop::{shift_transform, window_from_projwin, window_from_srcwin, WriteWindow};
pub use footprint::{
    extract_footprint, extract_footprint_geotiff, bbox_iou, metrics_close, ring_metrics_from_geojson,
    FootprintGeorefChoice, FootprintOptions, FootprintOutputFormat, FootprintResult,
    ResolvedValiditySource, RingMetrics, ValiditySourceChoice,
};
pub use info::{format_text, gather, gather_path, RasterInfo};
pub use input::{detect, detect_source, input_dimensions, InputFormat, RasterProfile, projected_georef};
pub use jp2::Jp2Raster;
pub use open::{open_geotiff, open_input, GeoTiffHandle, is_http_source};
pub use path::{log_convert_path, ConvertPath};
pub use stats::{print_json, ConvertStats};
pub use validate::{format_report, validate_cog, ValidationLevel, ValidationReport};
pub use util::{ensure_parent_dir, thread_pool};
pub use write::{convert_geotiff, ConvertRequest, ConvertResult};
