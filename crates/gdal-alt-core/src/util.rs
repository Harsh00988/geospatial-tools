use anyhow::{Context, Result};
use memmap2::Mmap;
use rayon::ThreadPool;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

pub fn thread_pool(jobs: usize) -> Result<ThreadPool> {
    let mut builder = rayon::ThreadPoolBuilder::new();
    if jobs > 0 {
        builder = builder.num_threads(jobs);
    }
    builder.build().context("failed to create thread pool")
}

/// Memory-map `path` for read-only access.
///
/// The caller must not modify the file while it is mapped.
pub fn map_file(path: &Path) -> Result<Arc<Mmap>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mmap = unsafe { Mmap::map(&file) }.context("failed to mmap file")?;
    Ok(Arc::new(mmap))
}
