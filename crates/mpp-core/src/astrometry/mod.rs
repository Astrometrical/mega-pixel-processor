//! Astrometric solutions: the full model of a plate solution, decoded from
//! a [`Header`].
//!
//! [`AstrometricSolution`] is one type over two forms:
//!
//! - [`PiSolution`]: PixInsight's astrometric solution. The XISF 1.0
//!   revision 1 standard block (`AstrometricSolution:*`, PixInsight ≥ 1.9.5)
//!   is decoded by `pixinsight.rs` (splines in [`distortion`]); the legacy
//!   block (`PCL:AstrometricSolution:*`, ≤ 1.9.4) by `legacy.rs`, into the
//!   same form. A header with a standard block is read through it alone.
//! - [`WcsSolution`]: FITS WCS with SIP. Reserved; nothing produces it yet.
//!
//! The decoders are copied from mega-merge-mosaic
//! (`mmm-core/src/astrometry/{standard,legacy,spline}.rs`), where every
//! convention below was verified on real PixInsight output; see that module's
//! docs for the verification record. mmm samples the models onto lookup
//! grids for bulk resampling; here they are evaluated directly.
//!
//! Conventions, shared by both blocks:
//!
//! - **Image coordinates** are PixInsight's: 0-based, rows top-down, pixel k
//!   spanning [k, k+1] (its centre at k + 0.5). FITS pixel coordinates are
//!   these plus 0.5 on both axes.
//! - **Projection-plane ("native") coordinates** are gnomonic tangent-plane
//!   offsets (ξ, η) in degrees about `ReferenceCelestialCoordinates`, ξ east,
//!   η north. Only the standard zenithal native frame (reference (0, 90),
//!   pole (180, 90)) and the Gnomonic projection are supported; anything else
//!   is refused rather than mapped wrongly.
//! - **Layers**: 1 is the linear solution (`LinearTransformationMatrix`
//!   about the reference point); 2 the projective 3×3 maps in both
//!   directions; 3 the RBF distortion residuals added to layer 2. Legacy
//!   files carry the distortion as PixInsight's precomputed
//!   `SplineWorldTransformation:PointGridInterpolation` grids instead of
//!   layers 2–3. A broken higher layer of a standard block degrades to the
//!   layer below with a note in [`PiSolution::notes`].

use serde::{Deserialize, Serialize};

use crate::formats::{KeywordValue, PropertyValue, XisfProperty};
use crate::header::Header;
use crate::wcs::tan_deproject;
use crate::{Error, Result};

pub mod distortion;
mod legacy;
mod pixinsight;

pub use distortion::{DistortionModel, Kernel, LocalTerm, ScalarSpline, VectorSpline};

/// A plate solution in one of the forms mpp understands.
///
/// `non_exhaustive`: when [`WcsSolution`] gains values, code outside this
/// crate already has a fallback arm for it.
// The size gap is to an uninhabited variant: there is nothing to box.
#[allow(clippy::large_enum_variant)]
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AstrometricSolution {
    /// PixInsight's solution: the 1.9.5 standard block, or a legacy (≤ 1.9.4)
    /// block decoded into the same form.
    PixInsight(PiSolution),
    /// FITS WCS with SIP distortion. Reserved: not produced yet.
    Wcs(WcsSolution),
}

/// FITS WCS (with SIP) form of a solution: a placeholder for a later
/// milestone, deliberately empty.
///
/// It is an enum with no variants, so no value of it exists yet: matching
/// on it needs no arm (`match w {}`), and the [`AstrometricSolution`] methods
/// are total without a fallback. The variant still reserves its slot in the
/// encoding (index 1 in postcard, `"Wcs"` in JSON).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WcsSolution {}

impl AstrometricSolution {
    /// Decode the solution carried by `header`.
    ///
    /// The XISF 1.0 revision 1 standard block (`AstrometricSolution:Version`
    /// present) wins; otherwise the legacy `PCL:AstrometricSolution:*` block
    /// is read. `Ok(None)` when the header carries neither. An
    /// [`Error::Astrometry`] when it claims a solution that cannot be used:
    /// a required property missing or malformed, an unsupported major
    /// revision, projection or native frame, legacy distortion grids that
    /// are missing or fail their layout checks, or a model that disagrees
    /// with its own linear solution. A solution is never silently
    /// approximated by its linear part.
    ///
    /// The image size, needed for the consistency checks of a solution with
    /// a distortion or projective layer, is [`Header::image_size`], else the
    /// `NAXIS1`/`NAXIS2` keywords. Such a solution with neither is an error;
    /// a linear-only solution needs no size.
    pub fn from_header(header: &Header) -> Result<Option<Self>> {
        let props = &header.properties;
        let dims = header
            .image_size
            .map(|(w, h)| (u64::from(w), u64::from(h)))
            .or_else(|| image_dims(header));
        let sol = if pixinsight::has_standard_block(props) {
            pixinsight::decode(props, dims)
        } else if legacy::has_legacy_block(props) {
            legacy::decode(props, dims)
        } else {
            return Ok(None);
        };
        sol.map(|s| Some(AstrometricSolution::PixInsight(s)))
            .map_err(Error::astrometry)
    }

    /// Celestial coordinates (RA, Dec) of the reference point, in degrees.
    pub fn reference_celestial(&self) -> (f64, f64) {
        match self {
            Self::PixInsight(s) => (s.reference_celestial[0], s.reference_celestial[1]),
            Self::Wcs(w) => match *w {},
        }
    }

    /// Mean pixel scale at the reference point, arcseconds per pixel:
    /// `sqrt(|det M|)` of the linear transformation.
    pub fn scale_arcsec_per_px(&self) -> f64 {
        match self {
            Self::PixInsight(s) => {
                let m = s.linear;
                (m[0][0] * m[1][1] - m[0][1] * m[1][0]).abs().sqrt() * 3600.0
            }
            Self::Wcs(w) => match *w {},
        }
    }

    /// Map an image coordinate (PixInsight convention: 0-based, top-down,
    /// pixel centres at k + 0.5) to (RA, Dec) in degrees, through the
    /// distortion model when the solution has one.
    pub fn pixel_to_sky(&self, x: f64, y: f64) -> (f64, f64) {
        match self {
            Self::PixInsight(s) => s.pixel_to_sky(x, y),
            Self::Wcs(w) => match *w {},
        }
    }
}

/// Which property block a [`PiSolution`] was decoded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PiBlock {
    /// XISF 1.0 revision 1 standard block (`AstrometricSolution:*`,
    /// PixInsight ≥ 1.9.5).
    Standard,
    /// Legacy block (`PCL:AstrometricSolution:*`, PixInsight ≤ 1.9.4).
    Legacy,
}

/// PixInsight's astrometric solution, as decoded from its XISF properties.
///
/// Field names follow the property ids. Angles are degrees; image
/// coordinates follow the module conventions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PiSolution {
    /// The block this was decoded from.
    pub block: PiBlock,
    /// `Version` of the standard block (e.g. `1.0`); `None` for legacy.
    pub version: Option<String>,
    /// `ProjectionSystem`, e.g. `Gnomonic`.
    pub projection_system: String,
    /// `CelestialReferenceSystem`, e.g. `ICRS`.
    pub reference_system: String,
    /// `ReferenceCelestialCoordinates`: (RA, Dec) of the reference point.
    pub reference_celestial: [f64; 2],
    /// `ReferenceImageCoordinates`: the reference point in image coordinates.
    pub reference_image: [f64; 2],
    /// `ReferenceNativeCoordinates` (the standard (0, 90) when absent).
    pub reference_native: [f64; 2],
    /// `CelestialPoleNativeCoordinates` (the standard (180, 90) when absent).
    pub celestial_pole_native: [f64; 2],
    /// `LinearTransformationMatrix` (layer 1), degrees per pixel: maps image
    /// offsets from the reference point to projection-plane offsets (ξ, η);
    /// row 0 gives ξ.
    pub linear: [[f64; 2]; 2],
    /// `ProjectiveTransformation:*` (layer 2), when available.
    pub projective: Option<Projective>,
    /// The distortion: the RBF model (layer 3) of a standard block, or a
    /// legacy file's interpolation grids.
    pub distortion: Option<Distortion>,
    /// `ControlPoints:*`: the star matches the solution was fitted to.
    pub control_points: Option<ControlPoints>,
    /// `Catalog`, e.g. `Gaia DR2`.
    pub catalog: Option<String>,
    /// `CreatorApplication`, e.g. `PixInsight 1.9.5`.
    pub creator_application: Option<String>,
    /// `CreatorModule`, e.g. `ImageSolver 6.5.0`.
    pub creator_module: Option<String>,
    /// `CreatorOS`, e.g. `Linux`.
    pub creator_os: Option<String>,
    /// `CreationTime`, ISO 8601 as written.
    pub creation_time: Option<String>,
    /// Why a higher layer was dropped while decoding, if any was.
    pub notes: Vec<String>,
}

impl PiSolution {
    /// Map an image coordinate to the projection plane (ξ, η), degrees:
    /// through the legacy grid, or layer 2 plus the layer 3 residual, or the
    /// linear solution, whichever is the highest layer present. (A layer 3
    /// residual without layer 2, which no decoder produces, is added to the
    /// linear solution.)
    pub fn image_to_projection(&self, x: f64, y: f64) -> (f64, f64) {
        let linear = || {
            let (dx, dy) = (x - self.reference_image[0], y - self.reference_image[1]);
            let m = self.linear;
            (m[0][0] * dx + m[0][1] * dy, m[1][0] * dx + m[1][1] * dy)
        };
        let base = || match &self.projective {
            Some(p) => p.image_to_projection.apply(x, y),
            None => linear(),
        };
        match &self.distortion {
            Some(Distortion::Grids {
                image_to_native, ..
            }) => image_to_native.eval(x, y),
            Some(Distortion::Rbf {
                image_to_projection,
                ..
            }) => {
                let (px, py) = base();
                let (rx, ry) = image_to_projection.residual(x, y);
                (px + rx, py + ry)
            }
            None => base(),
        }
    }

    /// Map a projection-plane point (ξ, η), degrees, back to an image
    /// coordinate: the inverse of [`Self::image_to_projection`], through the
    /// legacy `NativeToImage` grid, or layer 2's projection→image matrix plus
    /// the layer 3 projection→image residual, or the inverted linear
    /// solution — the same layer choice as the forward direction.
    pub fn projection_to_image(&self, xi: f64, eta: f64) -> (f64, f64) {
        let linear = || {
            let m = self.linear;
            let det = m[0][0] * m[1][1] - m[0][1] * m[1][0];
            let dx = (m[1][1] * xi - m[0][1] * eta) / det;
            let dy = (-m[1][0] * xi + m[0][0] * eta) / det;
            (self.reference_image[0] + dx, self.reference_image[1] + dy)
        };
        let base = || match &self.projective {
            Some(p) => p.projection_to_image.apply(xi, eta),
            None => linear(),
        };
        match &self.distortion {
            Some(Distortion::Grids {
                native_to_image, ..
            }) => native_to_image.eval(xi, eta),
            Some(Distortion::Rbf {
                projection_to_image,
                ..
            }) => {
                let (px, py) = base();
                let (rx, ry) = projection_to_image.residual(xi, eta);
                (px + rx, py + ry)
            }
            None => base(),
        }
    }

    /// Map an image coordinate to (RA, Dec) in degrees: the projection-plane
    /// point from [`Self::image_to_projection`], deprojected (gnomonic) about
    /// the reference point.
    pub fn pixel_to_sky(&self, x: f64, y: f64) -> (f64, f64) {
        let (xi, eta) = self.image_to_projection(x, y);
        tan_deproject(self.reference_celestial, xi, eta)
    }
}

/// A 3×3 projective transformation on homogeneous coordinates, row-major.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Homography(pub [[f64; 3]; 3]);

impl Homography {
    /// Apply to `(x, y)` (divides by the homogeneous coordinate).
    pub fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        let h = &self.0;
        let w = h[2][0] * x + h[2][1] * y + h[2][2];
        (
            (h[0][0] * x + h[0][1] * y + h[0][2]) / w,
            (h[1][0] * x + h[1][1] * y + h[1][2]) / w,
        )
    }
}

/// Layer 2: the projective transformations in both directions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Projective {
    /// `ProjectiveTransformation:ImageToProjection`.
    pub image_to_projection: Homography,
    /// `ProjectiveTransformation:ProjectionToImage`.
    pub projection_to_image: Homography,
}

/// The distortion part of a solution.
// One per solution, never stored in bulk: boxing would only add indirection.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Distortion {
    /// Layer 3 of a standard block: `DistortionModel:*` residuals, added to
    /// layer 2 in each direction.
    Rbf {
        /// `DistortionModel:ImageToProjection`.
        image_to_projection: DistortionModel,
        /// `DistortionModel:ProjectionToImage`.
        projection_to_image: DistortionModel,
    },
    /// A legacy file's `SplineWorldTransformation:PointGridInterpolation`
    /// lookup grids: the complete transformation in each direction.
    Grids {
        /// `ImageToNative`: image coordinates → (ξ, η).
        image_to_native: Grid2D,
        /// `NativeToImage`: (ξ, η) → image coordinates.
        native_to_image: Grid2D,
    },
}

/// `ControlPoints:*`: matched pairs, index for index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlPoints {
    /// `ControlPoints:Image`: image coordinates.
    pub image: Vec<[f64; 2]>,
    /// `ControlPoints:Celestial`: (RA, Dec), degrees.
    pub celestial: Vec<[f64; 2]>,
}

/// One direction of PixInsight's `PointGridInterpolation`: a dense lookup grid
/// of a 2-D coordinate transform. Layout (verified empirically in mmm):
/// `gx`/`gy` are row-major `rows × cols`; node `(r, c)` holds the transform
/// output for input point `(rect[0] + c·delta, rect[1] + r·delta)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Grid2D {
    /// Input-domain bounds `[x0, y0, x1, y1]`.
    pub rect: [f64; 4],
    /// Node spacing in input units.
    pub delta: f64,
    /// Node rows (input y steps).
    pub rows: u32,
    /// Node columns (input x steps).
    pub cols: u32,
    /// Output x at each node, row-major `rows × cols`.
    pub gx: Vec<f64>,
    /// Output y at each node, row-major `rows × cols`.
    pub gy: Vec<f64>,
}

impl Grid2D {
    /// Evaluate at `(x, y)` with bicubic Catmull-Rom interpolation.
    ///
    /// Inputs are clamped to `rect`. Out-of-range ghost nodes at the borders
    /// are linearly extrapolated from the edge node pair, which keeps affine
    /// fields exact through the border cells. A grid whose dimensions do not
    /// match its data (possible only for one built by hand or deserialized,
    /// since the decoder checks) evaluates to NaN.
    pub fn eval(&self, x: f64, y: f64) -> (f64, f64) {
        let n = self.rows as usize * self.cols as usize;
        if self.rows < 2 || self.cols < 2 || self.gx.len() != n || self.gy.len() != n {
            return (f64::NAN, f64::NAN);
        }
        let fx = (x.clamp(self.rect[0], self.rect[2]) - self.rect[0]) / self.delta;
        let fy = (y.clamp(self.rect[1], self.rect[3]) - self.rect[1]) / self.delta;
        let (c0, tx) = Self::cell(fx, self.cols);
        let (r0, ty) = Self::cell(fy, self.rows);
        let wx = catmull_rom_weights(tx);
        let wy = catmull_rom_weights(ty);
        let (mut ox, mut oy) = (0.0, 0.0);
        for (j, wj) in wy.iter().enumerate() {
            let r = r0 + j as i64 - 1;
            for (i, wi) in wx.iter().enumerate() {
                let c = c0 + i as i64 - 1;
                let w = wi * wj;
                ox += w * self.node(&self.gx, r, c);
                oy += w * self.node(&self.gy, r, c);
            }
        }
        (ox, oy)
    }

    /// Cell index (node at the cell's low edge) and fractional offset within.
    fn cell(f: f64, n: u32) -> (i64, f64) {
        let c = (f.floor() as i64).clamp(0, n as i64 - 2);
        (c, f - c as f64)
    }

    /// Node value; indices one step outside the grid (the bicubic support at
    /// border cells) are linearly extrapolated from the nearest node pair.
    fn node(&self, g: &[f64], r: i64, c: i64) -> f64 {
        let (rows, cols) = (self.rows as i64, self.cols as i64);
        let at = |r: i64, c: i64| g[(r * cols + c) as usize];
        let row = |r: i64| {
            if c < 0 {
                2.0 * at(r, 0) - at(r, 1)
            } else if c >= cols {
                2.0 * at(r, cols - 1) - at(r, cols - 2)
            } else {
                at(r, c)
            }
        };
        if r < 0 {
            2.0 * row(0) - row(1)
        } else if r >= rows {
            2.0 * row(rows - 1) - row(rows - 2)
        } else {
            row(r)
        }
    }
}

/// Catmull-Rom (Keys, a = −1/2) weights for nodes at offsets −1..=2 around
/// fractional position `t` ∈ [0, 1]. Exact for polynomials up to degree 2.
fn catmull_rom_weights(t: f64) -> [f64; 4] {
    let (t2, t3) = (t * t, t * t * t);
    [
        -0.5 * t3 + t2 - 0.5 * t,
        1.5 * t3 - 2.5 * t2 + 1.0,
        -1.5 * t3 + 2.0 * t2 + 0.5 * t,
        0.5 * t3 - 0.5 * t2,
    ]
}

/// PixInsight node count for a grid axis: `1 + ⌈extent/Δ⌉`, with a tolerance
/// for extents that are floating-point-exact multiples of `Δ`.
pub(crate) fn expected_nodes(extent: f64, delta: f64) -> Option<u32> {
    if !extent.is_finite() || extent <= 0.0 {
        return None;
    }
    let cells = extent / delta;
    let cells = if (cells - cells.round()).abs() < 1e-6 {
        cells.round()
    } else {
        cells.ceil()
    };
    (cells >= 1.0 && cells < f64::from(u32::MAX)).then(|| cells as u32 + 1)
}

/// Value of the property with id `id`, if present.
pub(crate) fn find_value<'a>(props: &'a [XisfProperty], id: &str) -> Option<&'a PropertyValue> {
    props.iter().find(|p| p.id == id).map(|p| &p.value)
}

/// A 2-vector property, if present and well formed.
pub(crate) fn vec2(props: &[XisfProperty], id: &str) -> Option<[f64; 2]> {
    match find_value(props, id)?.as_f64_vec()? {
        [a, b] => Some([*a, *b]),
        _ => None,
    }
}

/// An n×2 matrix property as points, if present and well formed.
pub(crate) fn points(props: &[XisfProperty], id: &str) -> Option<Vec<[f64; 2]>> {
    let (rows, cols, data) = find_value(props, id)?.as_f64_mat()?;
    (cols == 2 && data.len() == rows as usize * 2).then(|| data.as_chunks::<2>().0.to_vec())
}

/// A string property, if present.
pub(crate) fn string(props: &[XisfProperty], id: &str) -> Option<String> {
    find_value(props, id)?.as_str().map(str::to_string)
}

/// `(native reference, celestial pole native)` from the properties, defaulting
/// to the standard zenithal frame; `Err` unless both are that frame (any
/// other would change the deprojection, which is not implemented).
pub(crate) fn native_frame(
    props: &[XisfProperty],
    ref_id: &str,
    pole_id: &str,
) -> std::result::Result<([f64; 2], [f64; 2]), String> {
    let get = |id: &str, std: [f64; 2]| match find_value(props, id) {
        None => Ok(std),
        Some(_) => vec2(props, id).ok_or_else(|| format!("{id} is not a 2-vector")),
    };
    let r = get(ref_id, [0.0, 90.0])?;
    let p = get(pole_id, [180.0, 90.0])?;
    let close = |a: [f64; 2], b: [f64; 2]| (a[0] - b[0]).abs() < 1e-9 && (a[1] - b[1]).abs() < 1e-9;
    if !close(r, [0.0, 90.0]) || !close(p, [180.0, 90.0]) {
        return Err(format!(
            "non-standard native frame (reference {r:?}, pole {p:?}) is not supported"
        ));
    }
    Ok((r, p))
}

/// Require the Gnomonic projection: the only one [`PiSolution::pixel_to_sky`]
/// implements.
pub(crate) fn require_gnomonic(proj: &str) -> std::result::Result<(), String> {
    match crate::wcs::projection_code(proj) {
        Some("TAN") => Ok(()),
        Some(_) => Err(format!(
            "ProjectionSystem '{proj}' is not supported (only Gnomonic is)"
        )),
        None => Err(format!("unrecognized ProjectionSystem '{proj}'")),
    }
}

/// Decode-time consistency check of a solution against its own linear layer
/// (mmm's `validate_model` and `validate_grids`, without grid sampling): the
/// reference pixel must map to ≈ (0, 0) on the projection plane (36″
/// allowance) and back (5 px), and the image corners must agree with the
/// linear solution within the 180″ distortion allowance (a wrong layout or
/// orientation is off by about a degree). Checking needs the image size
/// (`dims`): a solution with anything beyond the linear layer and no size is
/// refused.
pub(crate) fn validate(
    sol: &PiSolution,
    dims: Option<(u64, u64)>,
) -> std::result::Result<(), String> {
    if let (
        Some((w, h)),
        Some(Distortion::Grids {
            image_to_native, ..
        }),
    ) = (dims, &sol.distortion)
    {
        let r = image_to_native.rect;
        if r[0].abs() > 1.0
            || r[1].abs() > 1.0
            || (r[2] - w as f64).abs() > 1.0
            || (r[3] - h as f64).abs() > 1.0
        {
            return Err(format!(
                "the image-to-native grid spans {r:?}, not the {w}×{h} image"
            ));
        }
    }
    let inverse = |xi: f64, eta: f64| match (&sol.distortion, &sol.projective) {
        (
            Some(Distortion::Grids {
                native_to_image, ..
            }),
            _,
        ) => Some(native_to_image.eval(xi, eta)),
        (
            Some(Distortion::Rbf {
                projection_to_image,
                ..
            }),
            Some(p),
        ) => {
            let (px, py) = p.projection_to_image.apply(xi, eta);
            let (rx, ry) = projection_to_image.residual(xi, eta);
            Some((px + rx, py + ry))
        }
        (None, Some(p)) => Some(p.projection_to_image.apply(xi, eta)),
        _ => None,
    };
    let Some((ix, iy)) = inverse(0.0, 0.0) else {
        return Ok(()); // linear only: nothing to check
    };
    let refimg = sol.reference_image;
    let (xi, eta) = sol.image_to_projection(refimg[0], refimg[1]);
    if !xi.is_finite() || !eta.is_finite() || xi.hypot(eta) > 0.01 {
        return Err(format!(
            "the model maps the reference pixel to ({xi}, {eta}) deg, not the reference point"
        ));
    }
    if !ix.is_finite() || !iy.is_finite() || (ix - refimg[0]).hypot(iy - refimg[1]) > 5.0 {
        return Err(format!(
            "the inverse model maps the reference point to ({ix}, {iy}), not the reference pixel"
        ));
    }
    let Some((w, h)) = dims else {
        return Err(
            "the image size is unknown (no image geometry or NAXIS1/NAXIS2), so the \
                    distortion cannot be checked against the linear solution"
                .into(),
        );
    };
    let rect = [0.0, 0.0, w as f64, h as f64];
    let m = sol.linear;
    for (cx, cy) in [
        (rect[0], rect[1]),
        (rect[2], rect[1]),
        (rect[0], rect[3]),
        (rect[2], rect[3]),
    ] {
        let (gx, gy) = sol.image_to_projection(cx, cy);
        let (dx, dy) = (cx - refimg[0], cy - refimg[1]);
        let (lx, ly) = (m[0][0] * dx + m[0][1] * dy, m[1][0] * dx + m[1][1] * dy);
        if !gx.is_finite() || !gy.is_finite() || (gx - lx).hypot(gy - ly) > 0.05 {
            return Err(format!(
                "the model disagrees with its own linear solution at image corner ({cx}, {cy})"
            ));
        }
    }
    Ok(())
}

/// Image width and height from the `NAXIS1`/`NAXIS2` keywords, if both are
/// positive integers.
fn image_dims(header: &Header) -> Option<(u64, u64)> {
    let dim = |name: &str| match header.keyword(name)?.typed_value() {
        KeywordValue::Int(n) if n > 0 => Some(n as u64),
        _ => None,
    };
    Some((dim("NAXIS1")?, dim("NAXIS2")?))
}

#[cfg(test)]
pub(crate) mod testprops {
    //! Property builders shared by the decoder tests.
    use crate::formats::{FitsKeyword, PropertyValue, XisfProperty};
    use crate::header::Header;

    pub(crate) fn prop(id: &str, type_: &str, value: PropertyValue) -> XisfProperty {
        XisfProperty {
            id: id.into(),
            type_: type_.into(),
            value,
            location: None,
        }
    }

    /// A header holding `props`, with `NAXIS1`/`NAXIS2` when `dims` is given.
    pub(crate) fn header(props: Vec<XisfProperty>, dims: Option<(u64, u64)>) -> Header {
        let kw = |name: &str, v: u64| FitsKeyword {
            name: name.into(),
            value: v.to_string(),
            comment: String::new(),
        };
        Header {
            keywords: dims
                .map(|(w, h)| vec![kw("NAXIS1", w), kw("NAXIS2", h)])
                .unwrap_or_default(),
            properties: props,
            image_size: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testprops::{header, prop};
    use super::*;

    /// Sample an analytic mapping on the PixInsight node layout.
    pub(crate) fn grid_from_fn(
        rect: [f64; 4],
        delta: f64,
        f: impl Fn(f64, f64) -> (f64, f64),
    ) -> Grid2D {
        let cols = expected_nodes(rect[2] - rect[0], delta).unwrap();
        let rows = expected_nodes(rect[3] - rect[1], delta).unwrap();
        let mut gx = Vec::with_capacity((rows * cols) as usize);
        let mut gy = Vec::with_capacity((rows * cols) as usize);
        for r in 0..rows {
            for c in 0..cols {
                let (ox, oy) = f(
                    rect[0] + f64::from(c) * delta,
                    rect[1] + f64::from(r) * delta,
                );
                gx.push(ox);
                gy.push(oy);
            }
        }
        Grid2D {
            rect,
            delta,
            rows,
            cols,
            gx,
            gy,
        }
    }

    #[test]
    fn grid_eval_reproduces_node_values() {
        let g = grid_from_fn([0.0, 0.0, 40.0, 24.0], 8.0, |x, y| {
            (0.3 * x - y, 0.01 * y * y + x)
        });
        for r in 0..g.rows {
            for c in 0..g.cols {
                let (x, y) = (f64::from(c) * 8.0, f64::from(r) * 8.0);
                let i = (r * g.cols + c) as usize;
                let (ox, oy) = g.eval(x, y);
                assert!(
                    (ox - g.gx[i]).abs() < 1e-12,
                    "node ({r},{c}) x: {ox} vs {}",
                    g.gx[i]
                );
                assert!(
                    (oy - g.gy[i]).abs() < 1e-12,
                    "node ({r},{c}) y: {oy} vs {}",
                    g.gy[i]
                );
            }
        }
    }

    #[test]
    fn grid_eval_exact_for_affine_fields_everywhere() {
        // Linear ghost-node extrapolation keeps affine fields exact even in
        // the border cells.
        let f = |x: f64, y: f64| (1.5 + 0.25 * x - 0.75 * y, -2.0 + 0.5 * x + 0.125 * y);
        let g = grid_from_fn([0.0, 0.0, 100.0, 60.0], 10.0, f);
        for &(x, y) in &[
            (0.0, 0.0),
            (0.3, 0.7),
            (99.9, 59.9),
            (50.0, 30.0),
            (3.1, 59.0),
            (97.0, 1.0),
        ] {
            let (ox, oy) = g.eval(x, y);
            let (ex, ey) = f(x, y);
            assert!(
                (ox - ex).abs() < 1e-9 && (oy - ey).abs() < 1e-9,
                "({x},{y}): ({ox},{oy})"
            );
        }
    }

    #[test]
    fn grid_eval_exact_for_quadratics_in_interior() {
        // Catmull-Rom (Keys a = −1/2) reproduces degree-2 polynomials exactly
        // wherever the full 4×4 support lies inside the grid.
        let f = |x: f64, y: f64| {
            (
                0.01 * x * x + 0.02 * x * y - 0.005 * y * y + x,
                0.5 * y + 0.003 * y * y - 0.001 * x * x,
            )
        };
        let g = grid_from_fn([0.0, 0.0, 100.0, 100.0], 10.0, f);
        for &(x, y) in &[
            (15.0, 15.0),
            (50.5, 42.3),
            (84.9, 15.1),
            (33.3, 66.6),
            (20.0, 80.0),
        ] {
            let (ox, oy) = g.eval(x, y);
            let (ex, ey) = f(x, y);
            assert!(
                (ox - ex).abs() < 1e-9 && (oy - ey).abs() < 1e-9,
                "({x},{y}): ({ox},{oy})"
            );
        }
    }

    #[test]
    fn grid_eval_clamps_to_rect() {
        let f = |x: f64, y: f64| (2.0 * x + y, x - 3.0 * y);
        let g = grid_from_fn([0.0, 0.0, 100.0, 60.0], 10.0, f);
        assert_eq!(g.eval(-50.0, 30.0), g.eval(0.0, 30.0));
        assert_eq!(g.eval(120.0, 70.0), g.eval(100.0, 60.0));
    }

    #[test]
    fn grid_eval_of_an_inconsistent_grid_is_nan_not_a_panic() {
        let mut g = grid_from_fn([0.0, 0.0, 100.0, 60.0], 10.0, |x, y| (x, y));
        g.gx.pop();
        assert!(g.eval(5.0, 5.0).0.is_nan());
    }

    #[test]
    fn no_solution_is_none() {
        assert!(
            AstrometricSolution::from_header(&Header::default())
                .unwrap()
                .is_none()
        );
        let unrelated = header(
            vec![prop(
                "Observation:Center:RA",
                "Float64",
                PropertyValue::F64(160.7),
            )],
            Some((100, 100)),
        );
        assert!(
            AstrometricSolution::from_header(&unrelated)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn standard_block_takes_precedence_over_legacy_ids() {
        use pixinsight::fixtures::layer1;
        let mut props = legacy::tests::orion_props(); // legacy ids
        props.extend(layer1(
            [10.0, 20.0],
            [5.0, 6.0],
            [[-1e-3, 0.0], [0.0, 1e-3]],
        ));
        let sol = AstrometricSolution::from_header(&header(props, Some((100, 100))))
            .unwrap()
            .unwrap();
        assert_eq!(sol.reference_celestial(), (10.0, 20.0));
        let AstrometricSolution::PixInsight(s) = &sol else {
            unreachable!()
        };
        assert_eq!(s.block, PiBlock::Standard);
        assert!(s.distortion.is_none() && s.projective.is_none());
    }

    #[test]
    fn unsupported_standard_major_version_is_an_error_even_with_legacy_ids() {
        let mut props = legacy::tests::orion_props();
        props.push(prop(
            "AstrometricSolution:Version",
            "String",
            PropertyValue::Str("2.0".into()),
        ));
        let e = AstrometricSolution::from_header(&header(props, None)).unwrap_err();
        assert!(matches!(e, Error::Astrometry { .. }), "{e}");
        assert!(e.to_string().contains("major"), "{e}");
    }

    #[test]
    fn standard_projective_only_solution_maps_through_layer_2() {
        use pixinsight::fixtures::{layer1, layer2};
        const S: f64 = 1e-3;
        let (rx, ry) = (50.0, 40.0);
        let mut props = layer1([10.0, 20.0], [rx, ry], [[-S, 0.0], [0.0, S]]);
        layer2(
            &mut props,
            [[-S, 0.0, rx * S], [0.0, S, -ry * S], [0.0, 0.0, 1.0]],
            [[-1.0 / S, 0.0, rx], [0.0, 1.0 / S, ry], [0.0, 0.0, 1.0]],
        );
        let m = AstrometricSolution::from_header(&header(props, Some((100, 80))))
            .unwrap()
            .unwrap();
        let (ra, dec) = m.pixel_to_sky(rx, ry);
        assert!((ra - 10.0).abs() < 1e-9 && (dec - 20.0).abs() < 1e-9);
        assert!((m.scale_arcsec_per_px() - 3.6).abs() < 1e-12);
    }

    #[test]
    fn the_wcs_variant_keeps_its_place_in_the_encoding() {
        // No WcsSolution value exists yet; the variant name is reserved.
        let e = serde_json::from_str::<AstrometricSolution>(r#"{"Wcs":null}"#).unwrap_err();
        assert!(!e.to_string().contains("unknown variant"), "{e}");
    }
}
