use std::path::Path;

use anyhow::{Context, Result};
use geotiff_reader::GeoTiffFile;
use tiff_core::Compression;

use crate::cog::mask::validate_dataset_masks;
use crate::open::open_geotiff;

#[derive(Debug, Clone)]
pub struct ValidationIssue {
    pub level: ValidationLevel,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationLevel {
    Error,
    Warning,
}

#[derive(Debug, Clone)]
pub struct ValidationReport {
    pub path: String,
    pub issues: Vec<ValidationIssue>,
}

impl ValidationReport {
    pub fn is_valid(&self) -> bool {
        !self
            .issues
            .iter()
            .any(|issue| issue.level == ValidationLevel::Error)
    }

    pub fn error_count(&self) -> usize {
        self.issues
            .iter()
            .filter(|issue| issue.level == ValidationLevel::Error)
            .count()
    }

    pub fn warning_count(&self) -> usize {
        self.issues
            .iter()
            .filter(|issue| issue.level == ValidationLevel::Warning)
            .count()
    }
}

pub fn validate_cog(path: &Path, mmap: bool) -> Result<ValidationReport> {
    let input = open_geotiff(path, mmap)?;
    Ok(validate_geotiff(&input, path))
}

pub fn validate_geotiff(input: &GeoTiffFile, path: &Path) -> ValidationReport {
    let mut issues = Vec::new();
    let base = input
        .tiff()
        .ifd(input.base_ifd_index())
        .expect("base IFD must exist");

    if !base.is_tiled() {
        issues.push(issue(
            ValidationLevel::Error,
            "COG requires tiled layout; base image is striped",
        ));
    }

    if input.overview_count() == 0 {
        issues.push(issue(
            ValidationLevel::Error,
            "COG requires internal overviews; none found",
        ));
    }

    let compression = base.compression();
    if !is_web_friendly_compression(compression) {
        issues.push(issue(
            ValidationLevel::Warning,
            format!(
                "compression {:?} may not be web-friendly for COG",
                Compression::from_code(compression)
                    .map(|c| c.name())
                    .unwrap_or("unknown")
            ),
        ));
    }

    if let (Some(tx), Some(ty)) = (base.tile_width(), base.tile_height()) {
        if !tx.is_multiple_of(16) || !ty.is_multiple_of(16) {
            issues.push(issue(
                ValidationLevel::Warning,
                format!("tile size {tx}x{ty} is not a multiple of 16"),
            ));
        }
        if !tx.is_power_of_two() || !ty.is_power_of_two() {
            issues.push(issue(
                ValidationLevel::Warning,
                format!("tile size {tx}x{ty} is not a power of two"),
            ));
        }
    }

    if input.transform().is_none() {
        issues.push(issue(
            ValidationLevel::Warning,
            "no georeferencing found (GeoTransform / GeoKeys)",
        ));
    }

    let mut prev_w = base.width();
    let mut prev_h = base.height();
    for index in 0..input.overview_count() {
        let ov = input
            .overview_ifd(index)
            .with_context(|| format!("overview {index}"))
            .unwrap();
        if !ov.is_tiled() {
            issues.push(issue(
                ValidationLevel::Error,
                format!("overview {index} is not tiled"),
            ));
        }
        if ov.compression() != compression {
            issues.push(issue(
                ValidationLevel::Warning,
                format!("overview {index} uses different compression than base image"),
            ));
        }
        let w = ov.width();
        let h = ov.height();
        if w >= prev_w || h >= prev_h {
            issues.push(issue(
                ValidationLevel::Warning,
                format!(
                    "overview {index} size {w}x{h} is not smaller than previous level {prev_w}x{prev_h}"
                ),
            ));
        }
        prev_w = w;
        prev_h = h;
    }

    validate_dataset_masks(input, &mut issues);

    if let Ok(source) = std::fs::read(path) {
        if !source.windows(4).any(|window| {
            window.starts_with(b"GDAL_STRUCTURAL_METADATA_SIZE=")
                || source
                    .get(8..)
                    .and_then(|bytes| std::str::from_utf8(bytes).ok())
                    .map(|text| text.contains("GDAL_STRUCTURAL_METADATA_SIZE="))
                    .unwrap_or(false)
        }) {
            issues.push(issue(
                ValidationLevel::Warning,
                "missing GDAL COG structural metadata ghost area",
            ));
        }
    }

    ValidationReport {
        path: path.display().to_string(),
        issues,
    }
}

pub fn format_report(report: &ValidationReport) -> String {
    let mut out = String::new();
    if report.is_valid() {
        out.push_str("COG validation: PASSED");
        if report.warning_count() > 0 {
            out.push_str(" (with warnings)");
        }
    } else {
        out.push_str("COG validation: FAILED");
    }
    out.push('\n');
    out.push_str(&format!("File: {}\n", report.path));
    for issue in &report.issues {
        let tag = match issue.level {
            ValidationLevel::Error => "ERROR",
            ValidationLevel::Warning => "WARN",
        };
        out.push_str(&format!("  [{tag}] {}\n", issue.message));
    }
    out
}

pub(crate) fn issue(level: ValidationLevel, message: impl Into<String>) -> ValidationIssue {
    ValidationIssue {
        level,
        message: message.into(),
    }
}

fn is_web_friendly_compression(code: u16) -> bool {
    matches!(
        Compression::from_code(code),
        Some(
            Compression::None
                | Compression::Lzw
                | Compression::Deflate
                | Compression::Jpeg
                | Compression::Zstd
                | Compression::Lerc
        )
    )
}
