use anyhow::{Context, Result};
use geotiff_writer::cog::{collect_planar_packed, LayerEncodePlan, PackedPlanarTile};
use jpeg2k::{DecodeArea, DecodeParameters, Image, ImageComponent};

pub const TILE_SIZE: u32 = 1024;

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

pub fn regions(width: u32, height: u32) -> Vec<Region> {
    let tiles_x = width.div_ceil(TILE_SIZE);
    let tiles_y = height.div_ceil(TILE_SIZE);
    (0..tiles_y)
        .flat_map(|ty| {
            (0..tiles_x).map(move |tx| {
                let x0 = tx * TILE_SIZE;
                let y0 = ty * TILE_SIZE;
                Region {
                    x0,
                    y0,
                    x1: (x0 + TILE_SIZE).min(width),
                    y1: (y0 + TILE_SIZE).min(height),
                }
            })
        })
        .collect()
}

pub fn decode_overview(data: &[u8], reduce: u32) -> Result<Image> {
    let params = DecodeParameters::new().reduce(reduce);
    Image::from_bytes_with(data, params).context("failed to decode JP2 overview")
}

pub fn planes_u8(image: &Image) -> Result<Planes<u8>> {
    let components: Vec<&ImageComponent> = image.components().iter().collect();
    if components.is_empty() {
        anyhow::bail!("JP2 image has no components");
    }
    let width = components[0].width() as usize;
    Ok(Planes {
        planes: components
            .iter()
            .map(|component| component_to_u8(component))
            .collect(),
        width,
    })
}

pub fn planes_u16(image: &Image) -> Result<Planes<u16>> {
    let components: Vec<&ImageComponent> = image.components().iter().collect();
    if components.is_empty() {
        anyhow::bail!("JP2 image has no components");
    }
    let width = components[0].width() as usize;
    Ok(Planes {
        planes: components
            .iter()
            .map(|component| component_to_u16(component))
            .collect(),
        width,
    })
}

pub fn pack_planes_u8(
    planes: &Planes<u8>,
    width: usize,
    height: usize,
    plan: LayerEncodePlan,
) -> Result<Vec<PackedPlanarTile>> {
    let refs: Vec<&[u8]> = planes.planes.iter().map(Vec::as_slice).collect();
    collect_planar_packed(&refs, width, height, plan).map_err(|err| anyhow::anyhow!(err))
}

pub fn pack_planes_u16(
    planes: &Planes<u16>,
    width: usize,
    height: usize,
    plan: LayerEncodePlan,
) -> Result<Vec<PackedPlanarTile>> {
    let refs: Vec<&[u16]> = planes.planes.iter().map(Vec::as_slice).collect();
    collect_planar_packed(&refs, width, height, plan).map_err(|err| anyhow::anyhow!(err))
}

pub trait Jp2Sample: geotiff_writer::NumericSample + Copy + Default + Send + Sync {
    fn planes(image: &Image) -> Result<Planes<Self>>;
    fn pack_planes(
        planes: &Planes<Self>,
        width: usize,
        height: usize,
        plan: LayerEncodePlan,
    ) -> Result<Vec<PackedPlanarTile>>;
}

impl Jp2Sample for u8 {
    fn planes(image: &Image) -> Result<Planes<Self>> {
        planes_u8(image)
    }

    fn pack_planes(
        planes: &Planes<Self>,
        width: usize,
        height: usize,
        plan: LayerEncodePlan,
    ) -> Result<Vec<PackedPlanarTile>> {
        pack_planes_u8(planes, width, height, plan)
    }
}

impl Jp2Sample for u16 {
    fn planes(image: &Image) -> Result<Planes<Self>> {
        planes_u16(image)
    }

    fn pack_planes(
        planes: &Planes<Self>,
        width: usize,
        height: usize,
        plan: LayerEncodePlan,
    ) -> Result<Vec<PackedPlanarTile>> {
        pack_planes_u16(planes, width, height, plan)
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
