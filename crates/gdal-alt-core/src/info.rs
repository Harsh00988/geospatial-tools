use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use geotiff_core::GeoTransform;
use tiff_core::Compression;
use tiff_core::SampleFormat;

use crate::geo;
use crate::input::{detect_source, InputFormat};
use crate::jp2::Jp2Header;
use crate::open::open_input;
use crate::util;

#[derive(Debug, Clone)]
pub struct RasterInfo {
    pub driver: String,
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub bands: u32,
    pub bits_per_sample: u16,
    pub sample_format: String,
    pub photometric: String,
    pub compression: String,
    pub tiled: bool,
    pub block_x: Option<u32>,
    pub block_y: Option<u32>,
    pub nodata: Option<String>,
    pub epsg: Option<u32>,
    pub crs_wkt: Option<String>,
    pub transform: Option<GeoTransform>,
    pub bounds: Option<[f64; 4]>,
    pub overviews: Vec<OverviewInfo>,
}

#[derive(Debug, Clone)]
pub struct OverviewInfo {
    pub index: usize,
    pub width: u32,
    pub height: u32,
    pub block_x: Option<u32>,
    pub block_y: Option<u32>,
}

pub fn gather(source: &str, mmap: bool) -> Result<RasterInfo> {
    match detect_source(source) {
        InputFormat::GeoTiff => geotiff_info_source(source, mmap),
        InputFormat::Jp2 => jp2_info(Path::new(source)),
    }
}

pub fn gather_path(path: &Path, mmap: bool) -> Result<RasterInfo> {
    gather(&path.to_string_lossy(), mmap)
}

pub fn format_text(info: &RasterInfo) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Driver: {}", info.driver);
    let _ = writeln!(out, "Files: {}", info.path);
    let _ = writeln!(out, "Size is {}, {}", info.width, info.height);
    let _ = writeln!(
        out,
        "Band count: {} ({}-bit {})",
        info.bands, info.bits_per_sample, info.sample_format
    );
    let _ = writeln!(out, "Photometric: {}", info.photometric);
    let _ = writeln!(
        out,
        "Layout: {}{}",
        if info.tiled { "tiled" } else { "striped" },
        info.block_size_text()
    );
    let _ = writeln!(out, "Compression: {}", info.compression);
    if let Some(nodata) = &info.nodata {
        let _ = writeln!(out, "NoData: {nodata}");
    }
    if let Some(epsg) = info.epsg {
        let _ = writeln!(out, "EPSG: {epsg}");
    }
    if let Some(bounds) = info.bounds {
        let _ = writeln!(
            out,
            "Bounds: ({:.6}, {:.6}, {:.6}, {:.6})",
            bounds[0], bounds[1], bounds[2], bounds[3]
        );
    }
    if let Some(transform) = info.transform {
        let _ = writeln!(
            out,
            "GeoTransform: [{:.10}, {:.10}, {:.10}, {:.10}, {:.10}, {:.10}]",
            transform.origin_x,
            transform.pixel_width,
            transform.skew_x,
            transform.origin_y,
            transform.skew_y,
            transform.pixel_height
        );
    }
    if info.overviews.is_empty() {
        let _ = writeln!(out, "Overviews: none");
    } else {
        let _ = writeln!(out, "Overviews: {}", info.overviews.len());
        for ov in &info.overviews {
            let _ = writeln!(
                out,
                "  {}: {}x{}{}",
                ov.index,
                ov.width,
                ov.height,
                ov.block_size_text()
            );
        }
    }
    out
}

impl RasterInfo {
    fn block_size_text(&self) -> String {
        match (self.block_x, self.block_y) {
            (Some(x), Some(y)) => format!(", block {x}x{y}"),
            _ => String::new(),
        }
    }
}

impl OverviewInfo {
    fn block_size_text(&self) -> String {
        match (self.block_x, self.block_y) {
            (Some(x), Some(y)) => format!(", block {x}x{y}"),
            _ => String::new(),
        }
    }
}

fn geotiff_info_source(source: &str, mmap: bool) -> Result<RasterInfo> {
    let handle = open_input(source, mmap)?;
    let input = handle.as_file();
    let ifd = input
        .tiff()
        .ifd(input.base_ifd_index())
        .context("failed to read base IFD")?;
    let layout = ifd.raster_layout().context("failed to read raster layout")?;
    let sample_format = SampleFormat::from_code(layout.sample_format)
        .map(sample_format_name)
        .unwrap_or_else(|| format!("code {}", layout.sample_format));
    let compression = Compression::from_code(ifd.compression())
        .map(|c| c.name().to_owned())
        .unwrap_or_else(|| format!("code {}", ifd.compression()));
    let photometric = ifd
        .photometric_interpretation_enum()
        .map(|p| format!("{p:?}"))
        .unwrap_or_else(|| "unknown".to_owned());

    let overviews = (0..input.overview_count())
        .filter_map(|index| {
            let ov = input.overview_ifd(index).ok()?;
            Some(OverviewInfo {
                index,
                width: ov.width(),
                height: ov.height(),
                block_x: ov.tile_width(),
                block_y: ov.tile_height(),
            })
        })
        .collect();

    Ok(RasterInfo {
        driver: "GeoTIFF".to_owned(),
        path: source.to_owned(),
        width: input.width(),
        height: input.height(),
        bands: input.band_count(),
        bits_per_sample: layout.bits_per_sample,
        sample_format,
        photometric,
        compression,
        tiled: ifd.is_tiled(),
        block_x: ifd.tile_width(),
        block_y: ifd.tile_height(),
        nodata: input.nodata().map(str::to_owned),
        epsg: input.epsg(),
        crs_wkt: None,
        transform: input.transform().cloned(),
        bounds: input.geo_bounds(),
        overviews,
    })
}

fn jp2_info(path: &Path) -> Result<RasterInfo> {
    let mmap = util::map_file(path)?;
    let header = Jp2Header::from_bytes(mmap.as_ref())?;
    let xml = geo::extract_jp2_xml(mmap.as_ref()).ok();
    let (epsg, transform, bounds) = if let Some(xml) = xml.as_deref() {
        match geo::parse_gmljp2(xml) {
            Ok((epsg, transform)) => {
                let bounds = transform.bounds(header.width, header.height);
                (Some(epsg as u32), Some(transform), Some(bounds))
            }
            Err(_) => (None, None, None),
        }
    } else {
        (None, None, None)
    };

    Ok(RasterInfo {
        driver: "JP2OpenJPEG".to_owned(),
        path: path.display().to_string(),
        width: header.width,
        height: header.height,
        bands: header.bands,
        bits_per_sample: header.bits_per_sample as u16,
        sample_format: "UInt".to_owned(),
        photometric: format!("{:?}", header.photometric),
        compression: "JPEG2000".to_owned(),
        tiled: true,
        block_x: None,
        block_y: None,
        nodata: None,
        epsg,
        crs_wkt: None,
        transform,
        bounds,
        overviews: Vec::new(),
    })
}

fn sample_format_name(format: SampleFormat) -> String {
    match format {
        SampleFormat::Uint => "UInt".to_owned(),
        SampleFormat::Int => "Int".to_owned(),
        SampleFormat::Float => "Float".to_owned(),
    }
}
