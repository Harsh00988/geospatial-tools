use anyhow::{bail, Context, Result};
use geotiff_core::GeoTransform;
use std::path::Path;

/// Parse EPSG code and geotransform from a GML JP2 metadata XML payload.
pub fn parse_gmljp2(xml: &str) -> Result<(u16, GeoTransform)> {
    let epsg = parse_epsg(xml).context("failed to parse EPSG from GML JP2 metadata")?;
    let origin = parse_pos(xml, "gml:origin").context("failed to parse gml:origin")?;
    let vectors = parse_offset_vectors(xml).context("failed to parse gml:offsetVector values")?;
    if vectors.len() < 2 {
        bail!("expected at least two gml:offsetVector elements");
    }

    let (px, py) = vectors[0];
    let (rx, ry) = vectors[1];

    let transform = GeoTransform {
        origin_x: origin.0 - px * 0.5,
        pixel_width: px,
        skew_x: py,
        origin_y: origin.1 - ry * 0.5,
        skew_y: rx,
        pixel_height: ry,
    };

    Ok((epsg, transform))
}

fn parse_epsg(xml: &str) -> Result<u16> {
    let marker = "EPSG::";
    let idx = xml
        .find(marker)
        .with_context(|| "EPSG URN not found in GML metadata")?;
    let rest = &xml[idx + marker.len()..];
    let digits = rest
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>();
    digits
        .parse::<u16>()
        .with_context(|| format!("invalid EPSG code in GML metadata: {digits}"))
}

fn parse_pos(xml: &str, tag: &str) -> Result<(f64, f64)> {
    let open = format!("<{tag}");
    let start = xml
        .find(&open)
        .with_context(|| format!("{tag} element not found"))?;
    let slice = &xml[start..];
    let pos_start = slice
        .find("<gml:pos>")
        .context("gml:pos element not found")?
        + "<gml:pos>".len();
    let pos_end = slice[pos_start..]
        .find("</gml:pos>")
        .context("gml:pos element not closed")?;
    parse_pair(&slice[pos_start..pos_start + pos_end])
}

fn parse_offset_vectors(xml: &str) -> Result<Vec<(f64, f64)>> {
    let mut vectors = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<gml:offsetVector") {
        let after_tag = &rest[start..];
        let content_start = after_tag
            .find('>')
            .context("malformed gml:offsetVector tag")?
            + 1;
        let content_end = after_tag[content_start..]
            .find("</gml:offsetVector>")
            .context("unclosed gml:offsetVector")?;
        vectors.push(parse_pair(
            after_tag[content_start..content_start + content_end].trim(),
        )?);
        rest = &after_tag[content_start + content_end..];
    }
    Ok(vectors)
}

fn parse_pair(text: &str) -> Result<(f64, f64)> {
    let mut parts = text.split_whitespace();
    let x = parts
        .next()
        .with_context(|| format!("missing first coordinate in '{text}'"))?
        .parse::<f64>()?;
    let y = parts
        .next()
        .with_context(|| format!("missing second coordinate in '{text}'"))?
        .parse::<f64>()?;
    Ok((x, y))
}

/// Parse a GDAL world file (`.tfw`, `.j2w`, `.wld`, etc.).
pub fn parse_world_file(contents: &str) -> Result<GeoTransform> {
    let values: Vec<f64> = contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(6)
        .map(|line| {
            line.parse::<f64>()
                .with_context(|| format!("invalid world file value '{line}'"))
        })
        .collect::<Result<Vec<_>>>()?;
    if values.len() < 6 {
        bail!("world file requires 6 numeric lines");
    }
    Ok(GeoTransform {
        pixel_width: values[0],
        skew_x: values[1],
        origin_x: values[4] - values[0] * 0.5,
        skew_y: values[2],
        pixel_height: values[3],
        origin_y: values[5] - values[3] * 0.5,
    })
}

/// Read a GDAL world file adjacent to `path`, if present.
pub fn read_world_file(path: &Path) -> Result<Option<GeoTransform>> {
    for ext in ["j2w", "jp2w", "wld", "tfw"] {
        let world_path = path.with_extension(ext);
        if world_path.is_file() {
            let contents = std::fs::read_to_string(&world_path)
                .with_context(|| format!("failed to read {}", world_path.display()))?;
            return parse_world_file(&contents).map(Some);
        }
    }
    Ok(None)
}

/// Read an EPSG code from a sidecar `.prj` file when available.
pub fn read_prj_epsg(path: &Path) -> Option<u16> {
    let prj_path = path.with_extension("prj");
    let contents = std::fs::read_to_string(prj_path).ok()?;
    let marker = "EPSG\",";
    let idx = contents.find(marker)?;
    let rest = &contents[idx + marker.len()..];
    let digits = rest
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>();
    digits.parse().ok()
}

/// Extract the first `xml ` box payload from a JP2 file, including nested superboxes.
pub fn extract_jp2_xml(data: &[u8]) -> Result<String> {
    find_xml_box(data, 0, data.len())?
        .context("no xml box found in JP2 file")
        .and_then(|bytes| String::from_utf8(bytes).context("JP2 xml box is not valid UTF-8"))
}

fn find_xml_box(data: &[u8], start: usize, end: usize) -> Result<Option<Vec<u8>>> {
    let mut offset = start;
    while offset + 8 <= end {
        let header = read_box_header(data, offset, end)?;
        if &header.kind == b"xml " {
            return Ok(Some(
                data[header.payload_start..header.payload_end].to_vec(),
            ));
        }

        if is_container_box(&header.kind) && header.payload_end > header.payload_start {
            if let Some(xml) = find_xml_box(data, header.payload_start, header.payload_end)? {
                return Ok(Some(xml));
            }
        }

        if header.box_end <= offset {
            bail!("invalid JP2 box layout at offset {offset}");
        }
        offset = header.box_end;
    }
    Ok(None)
}

struct BoxHeader {
    kind: [u8; 4],
    payload_start: usize,
    payload_end: usize,
    box_end: usize,
}

fn is_container_box(kind: &[u8; 4]) -> bool {
    matches!(kind, b"jp2h" | b"asoc" | b"uinf" | b"rreq" | b"list")
}

fn read_box_header(data: &[u8], offset: usize, limit: usize) -> Result<BoxHeader> {
    if offset + 8 > limit {
        bail!("truncated JP2 box header at offset {offset}");
    }

    let mut size = read_u32_be(data, offset)? as u64;
    let kind = data[offset + 4..offset + 8]
        .try_into()
        .expect("box type is four bytes");
    let mut header_end = offset + 8;

    if size == 1 {
        if offset + 16 > limit {
            bail!("truncated JP2 extended box length at offset {offset}");
        }
        size = read_u64_be(data, offset + 8)?;
        header_end = offset + 16;
    } else if size == 0 {
        size = (limit - offset) as u64;
    }

    if size < (header_end - offset) as u64 {
        bail!("invalid JP2 box size at offset {offset}");
    }

    let box_end = offset
        .checked_add(size as usize)
        .context("JP2 box size overflow")?;
    if box_end > limit {
        bail!("truncated JP2 box at offset {offset}");
    }

    Ok(BoxHeader {
        kind,
        payload_start: header_end,
        payload_end: box_end,
        box_end,
    })
}

fn read_u32_be(data: &[u8], offset: usize) -> Result<u32> {
    let bytes: [u8; 4] = data[offset..offset + 4]
        .try_into()
        .context("unexpected end of JP2 header")?;
    Ok(u32::from_be_bytes(bytes))
}

fn read_u64_be(data: &[u8], offset: usize) -> Result<u64> {
    let bytes: [u8; 8] = data[offset..offset + 8]
        .try_into()
        .context("unexpected end of JP2 extended header")?;
    Ok(u64::from_be_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_world_file() {
        let contents = "10\n0\n0\n-10\n300005\n2600035\n";
        let transform = parse_world_file(contents).unwrap();
        assert!((transform.pixel_width - 10.0).abs() < 1e-6);
        assert!((transform.pixel_height + 10.0).abs() < 1e-6);
        assert!((transform.origin_x - 300000.0).abs() < 1e-6);
        assert!((transform.origin_y - 2600040.0).abs() < 1e-6);
    }

    #[test]
    fn parses_sentinel2_gml_sample() {
        let xml = r#"<?xml version='1.0' encoding='UTF-8'?>
<gml:FeatureCollection xmlns:gml="http://www.opengis.net/gml">
  <gml:origin>
    <gml:Point srsName="urn:ogc:def:crs:EPSG::32642">
      <gml:pos>300005 2600035</gml:pos>
    </gml:Point>
  </gml:origin>
  <gml:offsetVector srsName="urn:ogc:def:crs:EPSG::32642">10 0</gml:offsetVector>
  <gml:offsetVector srsName="urn:ogc:def:crs:EPSG::32642">0 -10</gml:offsetVector>
</gml:FeatureCollection>"#;
        let (epsg, transform) = parse_gmljp2(xml).unwrap();
        assert_eq!(epsg, 32642);
        assert!((transform.origin_x - 300000.0).abs() < 1e-6);
        assert!((transform.pixel_width - 10.0).abs() < 1e-6);
        assert!((transform.origin_y - 2600040.0).abs() < 1e-6);
        assert!((transform.pixel_height + 10.0).abs() < 1e-6);
    }
}
