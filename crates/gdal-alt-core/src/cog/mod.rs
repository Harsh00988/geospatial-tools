mod builder;
mod grid;
pub use mask::discover_dataset_masks;
pub(crate) mod mask;
mod options;
pub(crate) mod semantics;
pub(crate) mod tile_payload;
mod variant;

pub use builder::{
    apply_compression, configure_cog, configure_cog_with_layer_sizes,
    configure_cog_with_layer_sizes_masked, configure_cog_with_levels, layer_sizes, overview_levels,
    tile_encoding_from_opts,
};
pub use grid::{auto_overview_levels, tile_jobs, TileJob};
pub use options::{
    CompressionChoice, CogOutputOptions, LercAdditionalCompressionChoice, ResamplingChoice,
};
pub use semantics::{associated_alpha_band_index, detect_transparency, TransparencySource};
pub use variant::tiff_variant;