//! 1 m grids in EPSG:25832 and their GeoTIFF I/O.
//!
//! Prepared rasters are aligned to whole meters: cell `(col, row)` covers
//! `E ∈ [min_e + col, min_e + col + 1)`, `N ∈ (max_n − row − 1, max_n − row]`.

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use tiff::decoder::{Decoder, DecodingResult};
use tiff::encoder::{Compression, DeflateLevel, TiffEncoder, colortype};
use tiff::tags::Tag;

use crate::geom::Bbox;

/// Value used for "no data" in prepared float rasters.
pub const NODATA: f32 = -9999.0;

/// Values at or below this are fill values, not heights: no terrain, water bed or surface
/// in the data lies below -1000 m NHN.
pub const FILL_BELOW: f32 = -1000.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Grid<T> {
    pub min_e: f64,
    pub max_n: f64,
    pub w: usize,
    pub h: usize,
    /// Row-major, row 0 is the northern edge.
    pub data: Vec<T>,
}

impl<T: Copy> Grid<T> {
    pub fn filled(bbox: &Bbox, value: T) -> Self {
        let w = bbox.width().round() as usize;
        let h = bbox.height().round() as usize;
        Self { min_e: bbox.min_e, max_n: bbox.max_n, w, h, data: vec![value; w * h] }
    }

    pub fn bbox(&self) -> Bbox {
        Bbox::new(self.min_e, self.max_n - self.h as f64, self.min_e + self.w as f64, self.max_n)
    }

    #[inline]
    pub fn get(&self, col: usize, row: usize) -> T {
        self.data[row * self.w + col]
    }

    #[inline]
    pub fn set(&mut self, col: usize, row: usize, v: T) {
        self.data[row * self.w + col] = v;
    }

    /// Cell containing the EPSG:25832 point, if inside.
    pub fn cell_at(&self, e: f64, n: f64) -> Option<(usize, usize)> {
        let c = (e - self.min_e).floor();
        let r = (self.max_n - n).floor();
        (c >= 0.0 && r >= 0.0 && (c as usize) < self.w && (r as usize) < self.h)
            .then_some((c as usize, r as usize))
    }

    pub fn at(&self, e: f64, n: f64) -> Option<T> {
        self.cell_at(e, n).map(|(c, r)| self.get(c, r))
    }

    /// Copies the window `bbox` (whole meters, inside this grid) into a new grid.
    pub fn window(&self, bbox: &Bbox) -> Result<Grid<T>> {
        let c0 = (bbox.min_e - self.min_e).round();
        let r0 = (self.max_n - bbox.max_n).round();
        let w = bbox.width().round() as usize;
        let h = bbox.height().round() as usize;
        ensure!(
            c0 >= 0.0 && r0 >= 0.0 && c0 as usize + w <= self.w && r0 as usize + h <= self.h,
            "window {bbox:?} outside grid {:?}",
            self.bbox()
        );
        let (c0, r0) = (c0 as usize, r0 as usize);
        let mut data = Vec::with_capacity(w * h);
        for r in 0..h {
            let s = (r0 + r) * self.w + c0;
            data.extend_from_slice(&self.data[s..s + w]);
        }
        Ok(Grid { min_e: bbox.min_e, max_n: bbox.max_n, w, h, data })
    }
}

/// Reads a single-band GeoTIFF with 1 m north-up pixels into a float grid, mapping the
/// file's NoData value (if any) to [`NODATA`].
pub fn read_f32(path: &Path) -> Result<Grid<f32>> {
    let mut dec = Decoder::new(BufReader::new(
        File::open(path).with_context(|| format!("opening {}", path.display()))?,
    ))?;
    let (w, h) = dec.dimensions()?;
    let (min_e, max_n) = georef(&mut dec, path)?;
    let nodata: Option<f32> = match dec.find_tag(Tag::GdalNodata)? {
        Some(v) => v.into_string()?.trim_matches(char::from(0)).trim().parse().ok(),
        None => None,
    };
    let data = match dec.read_image()? {
        DecodingResult::F32(v) => v,
        DecodingResult::F64(v) => v.into_iter().map(|x| x as f32).collect(),
        DecodingResult::I16(v) => v.into_iter().map(f32::from).collect(),
        DecodingResult::U8(v) => v.into_iter().map(f32::from).collect(),
        _ => bail!("{}: unsupported sample type", path.display()),
    };
    ensure!(data.len() == (w * h) as usize, "{}: expected one band", path.display());
    // Fill values: the declared NoData, NaN, and anything at or below FILL_BELOW (some
    // DGM1 border tiles store -9999 as data while declaring -3.4e38 as NoData).
    let data = data
        .into_iter()
        .map(|x| if Some(x) == nodata || x.is_nan() || x <= FILL_BELOW { NODATA } else { x })
        .collect();
    Ok(Grid { min_e, max_n, w: w as usize, h: h as usize, data })
}

/// Reads a single-band 8-bit GeoTIFF.
pub fn read_u8(path: &Path) -> Result<Grid<u8>> {
    let mut dec = Decoder::new(BufReader::new(
        File::open(path).with_context(|| format!("opening {}", path.display()))?,
    ))?;
    let (w, h) = dec.dimensions()?;
    let (min_e, max_n) = georef(&mut dec, path)?;
    let DecodingResult::U8(data) = dec.read_image()? else {
        bail!("{}: not an 8-bit raster", path.display());
    };
    ensure!(data.len() == (w * h) as usize, "{}: expected one band", path.display());
    Ok(Grid { min_e, max_n, w: w as usize, h: h as usize, data })
}

/// Reads a single-band 16-bit GeoTIFF (8-bit files are widened).
pub fn read_u16(path: &Path) -> Result<Grid<u16>> {
    let mut dec = Decoder::new(BufReader::new(
        File::open(path).with_context(|| format!("opening {}", path.display()))?,
    ))?;
    let (w, h) = dec.dimensions()?;
    let (min_e, max_n) = georef(&mut dec, path)?;
    let data = match dec.read_image()? {
        DecodingResult::U16(v) => v,
        DecodingResult::U8(v) => v.into_iter().map(u16::from).collect(),
        _ => bail!("{}: not a 16-bit raster", path.display()),
    };
    ensure!(data.len() == (w * h) as usize, "{}: expected one band", path.display());
    Ok(Grid { min_e, max_n, w: w as usize, h: h as usize, data })
}

/// Reads an 8-bit RGB(A) GeoTIFF into a grid of `[r, g, b, a]` (a = 255 without alpha band).
pub fn read_rgba(path: &Path) -> Result<Grid<[u8; 4]>> {
    let mut dec = Decoder::new(BufReader::new(
        File::open(path).with_context(|| format!("opening {}", path.display()))?,
    ))?;
    let (w, h) = dec.dimensions()?;
    let (min_e, max_n) = georef(&mut dec, path)?;
    let DecodingResult::U8(data) = dec.read_image()? else {
        bail!("{}: not an 8-bit raster", path.display());
    };
    let n = (w * h) as usize;
    let ch = data.len() / n;
    ensure!(ch == 3 || ch == 4, "{}: expected RGB or RGBA, got {ch} samples", path.display());
    let data = data
        .chunks_exact(ch)
        .map(|p| [p[0], p[1], p[2], if ch == 4 { p[3] } else { 255 }])
        .collect();
    Ok(Grid { min_e, max_n, w: w as usize, h: h as usize, data })
}

/// Upper-left corner from ModelPixelScale/ModelTiepoint, requiring 1 m pixels.
fn georef<R: std::io::Read + std::io::Seek>(
    dec: &mut Decoder<R>,
    path: &Path,
) -> Result<(f64, f64)> {
    let scale = dec
        .find_tag(Tag::ModelPixelScaleTag)?
        .with_context(|| format!("{}: no ModelPixelScale", path.display()))?
        .into_f64_vec()?;
    let tie = dec
        .find_tag(Tag::ModelTiepointTag)?
        .with_context(|| format!("{}: no ModelTiepoint", path.display()))?
        .into_f64_vec()?;
    ensure!(
        (scale[0] - 1.0).abs() < 1e-9 && (scale[1] - 1.0).abs() < 1e-9,
        "{}: pixel size {}×{} is not 1 m",
        path.display(),
        scale[0],
        scale[1]
    );
    let (min_e, max_n) = (tie[3] - tie[0], tie[4] + tie[1]);
    ensure!(
        (min_e - min_e.round()).abs() < 1e-6 && (max_n - max_n.round()).abs() < 1e-6,
        "{}: origin {min_e},{max_n} is not on whole meters",
        path.display()
    );
    Ok((min_e.round(), max_n.round()))
}

/// GeoKeyDirectory for EPSG:25832, pixel-is-area.
const GEOKEYS_25832: [u16; 16] = [1, 1, 0, 3, 1024, 0, 1, 1, 1025, 0, 1, 1, 3072, 0, 1, 25832];

macro_rules! write_fn {
    ($name:ident, $t:ty, $ct:ty) => {
        /// Writes a deflate-compressed single-band GeoTIFF (EPSG:25832, 1 m pixels).
        pub fn $name(path: &Path, g: &Grid<$t>, nodata: Option<f64>) -> Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("tif.tmp");
            {
                let f = BufWriter::new(File::create(&tmp)?);
                let mut enc = TiffEncoder::new(f)?
                    .with_compression(Compression::Deflate(DeflateLevel::default()));
                let mut img = enc.new_image::<$ct>(g.w as u32, g.h as u32)?;
                img.encoder().write_tag(Tag::ModelPixelScaleTag, &[1.0f64, 1.0, 0.0][..])?;
                img.encoder().write_tag(
                    Tag::ModelTiepointTag,
                    &[0.0f64, 0.0, 0.0, g.min_e, g.max_n, 0.0][..],
                )?;
                img.encoder().write_tag(Tag::GeoKeyDirectoryTag, &GEOKEYS_25832[..])?;
                if let Some(nd) = nodata {
                    img.encoder().write_tag(Tag::GdalNodata, &*format!("{nd}"))?;
                }
                img.write_data(&g.data)?;
            }
            std::fs::rename(&tmp, path)?;
            Ok(())
        }
    };
}

write_fn!(write_u8, u8, colortype::Gray8);
write_fn!(write_u16, u16, colortype::Gray16);
write_fn!(write_f32, f32, colortype::Gray32Float);

/// Writes a deflate-compressed RGBA GeoTIFF (EPSG:25832, 1 m pixels).
pub fn write_rgba(path: &Path, g: &Grid<[u8; 4]>) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tif.tmp");
    {
        let f = BufWriter::new(File::create(&tmp)?);
        let mut enc =
            TiffEncoder::new(f)?.with_compression(Compression::Deflate(DeflateLevel::default()));
        let mut img = enc.new_image::<colortype::RGBA8>(g.w as u32, g.h as u32)?;
        img.encoder().write_tag(Tag::ModelPixelScaleTag, &[1.0f64, 1.0, 0.0][..])?;
        img.encoder()
            .write_tag(Tag::ModelTiepointTag, &[0.0f64, 0.0, 0.0, g.min_e, g.max_n, 0.0][..])?;
        img.encoder().write_tag(Tag::GeoKeyDirectoryTag, &GEOKEYS_25832[..])?;
        let flat: Vec<u8> = g.data.iter().flatten().copied().collect();
        img.write_data(&flat)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geotiff_round_trip() {
        let dir = std::env::temp_dir().join(format!("hhmc-raster-{}", std::process::id()));
        let b = Bbox::new(563680.0, 5933568.0, 563690.0, 5933580.0);
        let mut g = Grid::filled(&b, 1.5f32);
        g.set(3, 4, NODATA);
        g.set(9, 11, -17.25);
        write_f32(&dir.join("f.tif"), &g, Some(NODATA as f64)).unwrap();
        let back = read_f32(&dir.join("f.tif")).unwrap();
        assert_eq!(back, g);
        let mut u = Grid::filled(&b, 0u8);
        u.set(0, 0, 7);
        write_u8(&dir.join("u.tif"), &u, None).unwrap();
        assert_eq!(read_u8(&dir.join("u.tif")).unwrap(), u);
        let mut w = Grid::filled(&b, 0u16);
        w.set(1, 2, 3394);
        write_u16(&dir.join("w.tif"), &w, None).unwrap();
        assert_eq!(read_u16(&dir.join("w.tif")).unwrap(), w);
        assert_eq!(read_u16(&dir.join("u.tif")).unwrap().get(0, 0), 7, "8-bit files widen");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fill_values_below_the_declared_nodata_are_nodata() {
        // As in DGM1 border tiles: -9999 stored as data, NoData declared as -3.4e38.
        let dir = std::env::temp_dir().join(format!("hhmc-raster-fill-{}", std::process::id()));
        let b = Bbox::new(0.0, 0.0, 3.0, 1.0);
        let g = Grid { min_e: 0.0, max_n: 1.0, w: 3, h: 1, data: vec![-9999.0, -9998.999, 4.5] };
        write_f32(&dir.join("f.tif"), &g, Some(f64::from(f32::MIN))).unwrap();
        let back = read_f32(&dir.join("f.tif")).unwrap();
        assert_eq!(back.data, vec![NODATA, NODATA, 4.5]);
        assert_eq!(back.bbox(), b);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn window_and_lookup() {
        let b = Bbox::new(0.0, 0.0, 4.0, 3.0);
        let mut g = Grid::filled(&b, 0i32);
        for (i, v) in g.data.iter_mut().enumerate() {
            *v = i as i32;
        }
        // Row 0 is the north edge.
        assert_eq!(g.at(0.5, 2.5), Some(0));
        assert_eq!(g.at(3.5, 0.5), Some(11));
        assert_eq!(g.at(4.0, 0.5), None);
        let w = g.window(&Bbox::new(1.0, 0.0, 3.0, 2.0)).unwrap();
        assert_eq!(w.data, vec![5, 6, 9, 10]);
    }
}
