//! Astrometric solutions: the linear WCS model and its extraction from
//! PixInsight XISF astrometric-solution properties.
//!
//! PixInsight (ImageSolver / MosaicByCoordinates) stores plate solutions as
//! XISF `<Property>` elements, not FITS keywords. Extraction to
//! [`LinearWcs`] keeps the stored top-down y axis:
//!
//! - `CRVAL1/2` = `PCL:AstrometricSolution:ReferenceCelestialCoordinates`,
//! - `CRPIX1/2` = `ReferenceImageCoordinates` + 0.5 on both axes
//!   (PixInsight pixel centers sit at k + 0.5 in 0-based coords; FITS pixel
//!   centers sit at integer 1-based coords),
//! - `CD1_1..CD2_2` = `LinearTransformationMatrix` exactly as stored,
//! - `CTYPE1/2` = `RA---`/`DEC--` + projection code (Gnomonic → `TAN`).
//!
//! These conventions were verified empirically against real PixInsight
//! output in the sibling MMM project (see mmm-core `astrometry.rs` for the
//! full verification record).
//!
//! **Limitation:** if a solution also carries a distortion model
//! (`PCL:AstrometricSolution:SplineWorldTransformation`), only the linear
//! part is extracted — `LinearTransformationMatrix` is PixInsight's own
//! linear approximation around the reference point, so residuals grow away
//! from it. Distortion support is a planned sidecar schema extension.

use crate::formats::XisfProperty;

/// A linear FITS WCS: `sky = project(crval, cd · (pixel − crpix))`, with
/// `pixel` in FITS convention (1-based, pixel centers at integers, rows in
/// stored order — top-down for our inputs and outputs).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LinearWcs {
    /// Reference sky coordinates [RA, Dec] in degrees.
    pub crval: [f64; 2],
    /// Reference pixel [x, y], FITS 1-based.
    pub crpix: [f64; 2],
    /// Linear transformation, degrees per pixel; `cd[0]` is axis 1 (ξ/RA).
    pub cd: [[f64; 2]; 2],
    /// Axis types, e.g. `RA---TAN` / `DEC--TAN`.
    pub ctype: [String; 2],
    /// Celestial reference system for the RADESYS card (e.g. `ICRS`).
    pub radesys: String,
}

impl LinearWcs {
    /// Map a FITS pixel coordinate to (RA, Dec) in degrees.
    ///
    /// Uses the gnomonic (TAN) deprojection — exact for `RA---TAN`, which is
    /// what MosaicByCoordinates canvases use. Intended for validation and ROI
    /// math, not high-volume transforms.
    pub fn pixel_to_sky(&self, x: f64, y: f64) -> (f64, f64) {
        let dx = x - self.crpix[0];
        let dy = y - self.crpix[1];
        let xi = self.cd[0][0] * dx + self.cd[0][1] * dy;
        let eta = self.cd[1][0] * dx + self.cd[1][1] * dy;
        tan_deproject(self.crval, xi, eta)
    }

    /// Map (RA, Dec) in degrees to a FITS pixel coordinate (TAN projection).
    pub fn sky_to_pixel(&self, ra: f64, dec: f64) -> (f64, f64) {
        let (xi, eta, _) = tan_project(self.crval, ra, dec);
        let det = self.cd[0][0] * self.cd[1][1] - self.cd[0][1] * self.cd[1][0];
        let dx = (self.cd[1][1] * xi - self.cd[0][1] * eta) / det;
        let dy = (self.cd[0][0] * eta - self.cd[1][0] * xi) / det;
        (self.crpix[0] + dx, self.crpix[1] + dy)
    }
}

/// Gnomonic (TAN) native plane (ξ, η) → celestial (RA, Dec) about `crval`,
/// everything in degrees. ξ grows toward east, η toward north — the standard
/// zenithal native frame with `ReferenceNativeCoordinates` (0, 90) and
/// `CelestialPoleNativeCoordinates` (180, 90).
fn tan_deproject(crval: [f64; 2], xi_deg: f64, eta_deg: f64) -> (f64, f64) {
    let (xi, eta) = (xi_deg.to_radians(), eta_deg.to_radians());
    let (a0, d0) = (crval[0].to_radians(), crval[1].to_radians());
    let den = d0.cos() - eta * d0.sin();
    let ra = a0 + xi.atan2(den);
    let dec = ((d0.sin() + eta * d0.cos()) / xi.hypot(den)).atan();
    (ra.to_degrees().rem_euclid(360.0), dec.to_degrees())
}

/// Celestial → TAN native plane about `crval`, degrees. The third value is the
/// projection denominator (cosine of the angular distance to the tangent
/// point): ≤ 0 means the point is 90° or more away and (ξ, η) are meaningless.
fn tan_project(crval: [f64; 2], ra: f64, dec: f64) -> (f64, f64, f64) {
    let (a0, d0) = (crval[0].to_radians(), crval[1].to_radians());
    let (a, d) = (ra.to_radians(), dec.to_radians());
    let cos_da = (a - a0).cos();
    let den = d0.sin() * d.sin() + d0.cos() * d.cos() * cos_da;
    let xi = (d.cos() * (a - a0).sin() / den).to_degrees();
    let eta = ((d0.cos() * d.sin() - d0.sin() * d.cos() * cos_da) / den).to_degrees();
    (xi, eta, den)
}

/// Extract the linear WCS from PixInsight astrometric-solution properties.
///
/// Returns `None` if any required property (reference celestial/image
/// coordinates, linear transformation matrix) is missing or malformed, or if
/// the projection system is one we cannot express as a FITS CTYPE code.
pub fn from_xisf_properties(props: &[XisfProperty]) -> Option<LinearWcs> {
    let find = |id: &str| props.iter().find(|p| p.id == id).map(|p| &p.value);

    let crval = find("PCL:AstrometricSolution:ReferenceCelestialCoordinates")?.as_f64_vec()?;
    let refimg = find("PCL:AstrometricSolution:ReferenceImageCoordinates")?.as_f64_vec()?;
    let (rows, cols, m) =
        find("PCL:AstrometricSolution:LinearTransformationMatrix")?.as_f64_mat()?;
    if crval.len() != 2 || refimg.len() != 2 || (rows, cols) != (2, 2) {
        return None;
    }

    // Missing projection property defaults to Gnomonic — the only projection
    // MosaicByCoordinates produces; an explicit unknown one refuses (better no
    // WCS than a wrong CTYPE).
    let proj =
        find("PCL:AstrometricSolution:ProjectionSystem").map_or(Some("Gnomonic"), |v| v.as_str());
    let code = projection_code(proj?)?;

    let radesys = find("Observation:CelestialReferenceSystem")
        .and_then(|v| v.as_str())
        .unwrap_or("ICRS")
        .to_string();

    Some(LinearWcs {
        crval: [crval[0], crval[1]],
        // PixInsight image coords (pixel centers at k + 0.5, 0-based) →
        // FITS pixel coords (centers at integers, 1-based): +0.5 both axes.
        crpix: [refimg[0] + 0.5, refimg[1] + 0.5],
        cd: [[m[0], m[1]], [m[2], m[3]]],
        ctype: [
            format!("{:-<5}{code}", "RA"),
            format!("{:-<5}{code}", "DEC"),
        ],
        radesys,
    })
}

/// PixInsight projection-system name → 3-letter FITS projection code.
fn projection_code(name: &str) -> Option<&'static str> {
    Some(match name {
        "Gnomonic" => "TAN",
        "Stereographic" => "STG",
        "Orthographic" => "SIN",
        "ZenithalEqualArea" => "ZEA",
        "Mercator" => "MER",
        "PlateCarree" => "CAR",
        "HammerAitoff" => "AIT",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::{PropertyValue, XisfProperty};

    fn prop(id: &str, type_: &str, value: PropertyValue) -> XisfProperty {
        XisfProperty {
            id: id.into(),
            type_: type_.into(),
            value,
            location: None,
        }
    }

    fn solution_props() -> Vec<XisfProperty> {
        vec![
            prop(
                "PCL:AstrometricSolution:ReferenceCelestialCoordinates",
                "F64Vector",
                PropertyValue::F64Vec(vec![84.199, -3.240]),
            ),
            prop(
                "PCL:AstrometricSolution:ReferenceImageCoordinates",
                "F64Vector",
                PropertyValue::F64Vec(vec![100.0, 200.0]),
            ),
            prop(
                "PCL:AstrometricSolution:LinearTransformationMatrix",
                "F64Matrix",
                PropertyValue::F64Mat {
                    rows: 2,
                    cols: 2,
                    data: vec![-4.4368e-4, 0.0, 0.0, 4.4368e-4],
                },
            ),
            prop(
                "PCL:AstrometricSolution:ProjectionSystem",
                "String",
                PropertyValue::Str("Gnomonic".into()),
            ),
            prop(
                "Observation:CelestialReferenceSystem",
                "String",
                PropertyValue::Str("ICRS".into()),
            ),
        ]
    }

    #[test]
    fn extracts_linear_wcs_from_pixinsight_properties() {
        let w = from_xisf_properties(&solution_props()).unwrap();
        assert_eq!(w.crval, [84.199, -3.240]);
        // PixInsight image coords (centers at k + 0.5, 0-based) → FITS
        // pixel coords (centers at integers, 1-based): +0.5 on both axes.
        assert_eq!(w.crpix, [100.5, 200.5]);
        assert_eq!(w.cd, [[-4.4368e-4, 0.0], [0.0, 4.4368e-4]]);
        assert_eq!(w.ctype, ["RA---TAN".to_string(), "DEC--TAN".to_string()]);
        assert_eq!(w.radesys, "ICRS");
    }

    #[test]
    fn missing_projection_defaults_to_gnomonic_unknown_refuses() {
        let mut props = solution_props();
        props.retain(|p| p.id != "PCL:AstrometricSolution:ProjectionSystem");
        assert_eq!(from_xisf_properties(&props).unwrap().ctype[0], "RA---TAN");

        let mut props = solution_props();
        props
            .iter_mut()
            .find(|p| p.id == "PCL:AstrometricSolution:ProjectionSystem")
            .unwrap()
            .value = PropertyValue::Str("Bonne".into());
        assert!(from_xisf_properties(&props).is_none());
    }

    #[test]
    fn incomplete_solution_yields_none() {
        let mut props = solution_props();
        props.retain(|p| p.id != "PCL:AstrometricSolution:LinearTransformationMatrix");
        assert!(from_xisf_properties(&props).is_none());
    }

    #[test]
    fn pixel_sky_roundtrip() {
        let w = from_xisf_properties(&solution_props()).unwrap();
        let (ra, dec) = w.pixel_to_sky(3000.0, 4000.0);
        let (x, y) = w.sky_to_pixel(ra, dec);
        assert!((x - 3000.0).abs() < 1e-6, "x {x}");
        assert!((y - 4000.0).abs() < 1e-6, "y {y}");
        // Reference pixel maps exactly to the reference coordinates.
        let (ra0, dec0) = w.pixel_to_sky(w.crpix[0], w.crpix[1]);
        assert!((ra0 - 84.199).abs() < 1e-9);
        assert!((dec0 - -3.240).abs() < 1e-9);
    }
}
