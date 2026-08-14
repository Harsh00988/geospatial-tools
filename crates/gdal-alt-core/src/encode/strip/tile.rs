use ndarray::{Array2, Array3};

pub(crate) enum StripTile<T> {
    Single(Array2<T>),
    Multi(Array3<T>),
}

/// On-disk or in-memory cache of decoded overview tiles for pyramid chaining.
pub(crate) struct DecodedTileSpool<T> {
    pub(super) inner: std::sync::Arc<std::sync::Mutex<super::decode_cache::DecodedTileStorage<T>>>,
    pub(super) _marker: std::marker::PhantomData<T>,
}
