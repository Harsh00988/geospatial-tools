use anyhow::{Context, Result};
use geotiff_writer::cog::{collect_planar_packed_u8, LayerEncodePlan, PackedPlanarTile};
use jpeg2k::{DecodeArea, DecodeParameters, Image, ImageComponent};

pub const TILE_SIZE: u32 = 1024;

#[derive(Clone, Copy, Debug)]
pub struct Region {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

pub struct RgbPlanes {
    pub planes: [Vec<u8>; 3],
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

pub fn rgb_planes(image: &Image) -> Result<RgbPlanes> {
    let components: Vec<&ImageComponent> = image.components().iter().take(3).collect();
    if components.len() < 3 {
        anyhow::bail!("expected RGB JP2 image");
    }
    Ok(planes_from_components(
        components[0],
        components[1],
        components[2],
    ))
}

pub fn pack_rgb_planes(
    planes: &RgbPlanes,
    width: usize,
    height: usize,
    plan: LayerEncodePlan,
) -> Result<Vec<PackedPlanarTile>> {
    collect_planar_packed_u8(
        [&planes.planes[0], &planes.planes[1], &planes.planes[2]],
        width,
        height,
        plan,
    )
    .map_err(|err| anyhow::anyhow!(err))
}

fn planes_from_components(r: &ImageComponent, g: &ImageComponent, b: &ImageComponent) -> RgbPlanes {
    let width = r.width() as usize;
    RgbPlanes {
        planes: [component_to_u8(r), component_to_u8(g), component_to_u8(b)],
        width,
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
