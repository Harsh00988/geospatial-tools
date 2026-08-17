use anyhow::{Context, Result};
use geotiff_writer::cog::{collect_planar_packed, LayerEncodePlan, PackedPlanarTile};
use jpeg2k::{DecodeArea, DecodeParameters, Image, ImageComponent};
use tiff_core::SampleFormat;

pub const TILE_SIZE: u32 = 1024;
const MAX_OPENJPEG_REDUCE: u32 = 4;

#[derive(Clone, Copy, Debug)]
pub struct Region {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

pub struct Planes<T> {
    pub planes: Vec<Vec<T>>,
    pub width: usize,
}

impl Region {
    pub fn decode(&self, data: &[u8]) -> Result<Image> {
        let area = DecodeArea::new(self.x0, self.y0, self.x1, self.y1);
        let params = DecodeParameters::new().decode_area(Some(area));
        Image::from_bytes_with(data, params).context("failed to decode JP2 region")
    }
}

pub fn regions(width: u32, height: u32, x_off: u32, y_off: u32) -> Vec<Region> {
    regions_in_rect(x_off, y_off, width, height)
}

pub fn regions_in_rect(x_off: u32, y_off: u32, width: u32, height: u32) -> Vec<Region> {
    let tiles_x = width.div_ceil(TILE_SIZE);
    let tiles_y = height.div_ceil(TILE_SIZE);
    (0..tiles_y)
        .flat_map(|ty| {
            (0..tiles_x).map(move |tx| {
                let local_x0 = tx * TILE_SIZE;
                let local_y0 = ty * TILE_SIZE;
                Region {
                    x0: x_off + local_x0,
                    y0: y_off + local_y0,
                    x1: x_off + (local_x0 + TILE_SIZE).min(width),
                    y1: y_off + (local_y0 + TILE_SIZE).min(height),
                }
            })
        })
        .collect()
}

pub fn decode_overview(data: &[u8], reduce: u32) -> Result<Image> {
    let params = DecodeParameters::new().reduce(reduce);
    Image::from_bytes_with(data, params).context("failed to decode JP2 overview")
}

pub fn openjpeg_reduce(level: u32) -> u32 {
    level.trailing_zeros().min(MAX_OPENJPEG_REDUCE)
}

pub fn planes_for_image<T: Jp2Sample>(
    image: &Image,
    sample_format: SampleFormat,
    bits_per_sample: u8,
    bands: Option<&[usize]>,
) -> Result<Planes<T>> {
    let components: Vec<&ImageComponent> = image.components().iter().collect();
    if components.is_empty() {
        anyhow::bail!("JP2 image has no components");
    }
    let width = components[0].width() as usize;
    let band_indices: Vec<usize> = match bands {
        None => (1..=components.len()).collect(),
        Some(selected) => selected.to_vec(),
    };
    let planes = band_indices
        .iter()
        .map(|band| {
            let component = components
                .get(band - 1)
                .ok_or_else(|| anyhow::anyhow!("missing JP2 component for band {band}"))?;
            T::component_to_vec(component, sample_format, bits_per_sample)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Planes { planes, width })
}

pub fn pack_planes<T: Jp2Sample>(
    planes: &Planes<T>,
    width: usize,
    height: usize,
    plan: LayerEncodePlan,
) -> Result<Vec<PackedPlanarTile>> {
    let refs: Vec<&[T]> = planes.planes.iter().map(Vec::as_slice).collect();
    collect_planar_packed(&refs, width, height, plan).map_err(|err| anyhow::anyhow!(err))
}

pub trait Jp2Sample: geotiff_writer::NumericSample + Copy + Default + Send + Sync {
    fn component_to_vec(
        component: &ImageComponent,
        sample_format: SampleFormat,
        bits_per_sample: u8,
    ) -> Result<Vec<Self>>;
    fn planes(
        image: &Image,
        sample_format: SampleFormat,
        bits_per_sample: u8,
        bands: Option<&[usize]>,
    ) -> Result<Planes<Self>> {
        planes_for_image(image, sample_format, bits_per_sample, bands)
    }
    fn pack_planes(
        planes: &Planes<Self>,
        width: usize,
        height: usize,
        plan: LayerEncodePlan,
    ) -> Result<Vec<PackedPlanarTile>>;
}

impl Jp2Sample for u8 {
    fn component_to_vec(
        component: &ImageComponent,
        _sample_format: SampleFormat,
        _bits_per_sample: u8,
    ) -> Result<Vec<Self>> {
        Ok(component_to_u8(component))
    }

    fn pack_planes(
        planes: &Planes<Self>,
        width: usize,
        height: usize,
        plan: LayerEncodePlan,
    ) -> Result<Vec<PackedPlanarTile>> {
        pack_planes(planes, width, height, plan)
    }
}

impl Jp2Sample for i8 {
    fn component_to_vec(
        component: &ImageComponent,
        _sample_format: SampleFormat,
        bits_per_sample: u8,
    ) -> Result<Vec<Self>> {
        Ok(component_to_i8(component, bits_per_sample))
    }

    fn pack_planes(
        planes: &Planes<Self>,
        width: usize,
        height: usize,
        plan: LayerEncodePlan,
    ) -> Result<Vec<PackedPlanarTile>> {
        pack_planes(planes, width, height, plan)
    }
}

impl Jp2Sample for u16 {
    fn component_to_vec(
        component: &ImageComponent,
        _sample_format: SampleFormat,
        _bits_per_sample: u8,
    ) -> Result<Vec<Self>> {
        Ok(component_to_u16(component))
    }

    fn pack_planes(
        planes: &Planes<Self>,
        width: usize,
        height: usize,
        plan: LayerEncodePlan,
    ) -> Result<Vec<PackedPlanarTile>> {
        pack_planes(planes, width, height, plan)
    }
}

impl Jp2Sample for i16 {
    fn component_to_vec(
        component: &ImageComponent,
        _sample_format: SampleFormat,
        bits_per_sample: u8,
    ) -> Result<Vec<Self>> {
        Ok(component_to_i16(component, bits_per_sample))
    }

    fn pack_planes(
        planes: &Planes<Self>,
        width: usize,
        height: usize,
        plan: LayerEncodePlan,
    ) -> Result<Vec<PackedPlanarTile>> {
        pack_planes(planes, width, height, plan)
    }
}

fn component_to_u8(component: &ImageComponent) -> Vec<u8> {
    let data = component.data();
    if component.precision() == 8 && !component.is_signed() {
        data.iter().map(|&value| value as u8).collect()
    } else {
        component.data_u8().collect()
    }
}

fn component_to_u16(component: &ImageComponent) -> Vec<u16> {
    if component.is_signed() {
        return component.data_u16().collect();
    }
    let data = component.data();
    if component.precision() <= 16 {
        data.iter().map(|&value| value as u16).collect()
    } else {
        component.data_u16().collect()
    }
}

fn component_to_i8(component: &ImageComponent, bits_per_sample: u8) -> Vec<i8> {
    let shift = 32usize.saturating_sub(bits_per_sample as usize);
    component
        .data()
        .iter()
        .map(|&value| (value << shift >> shift) as i8)
        .collect()
}

fn component_to_i16(component: &ImageComponent, bits_per_sample: u8) -> Vec<i16> {
    let shift = 32usize.saturating_sub(bits_per_sample as usize);
    component
        .data()
        .iter()
        .map(|&value| (value << shift >> shift) as i16)
        .collect()
}
