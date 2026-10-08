//! Legacy (PixInsight ≤ 1.9.4) astrometric solution: `PCL:AstrometricSolution:*`
//! properties, with the spline distortion carried as PixInsight's
//! `SplineWorldTransformation:PointGridInterpolation` lookup grids.
//!
//! Copied from mmm (`mmm-core/src/astrometry/legacy.rs`), where everything
//! was verified on real 1.9.4 files. The linear part is read by mpp's
//! existing [`wcs::from_xisf_properties`](crate::wcs::from_xisf_properties).
//! PixInsight 1.9.5 no longer writes these ids, but old data stays in
//! circulation, so this reader is kept indefinitely.

use super::{
    Distortion, Grid2D, PiBlock, PiSolution, expected_nodes, find_value, native_frame,
    require_gnomonic, string, validate, vec2,
};
use crate::formats::XisfProperty;

/// Prefix of every legacy solution property.
const PREFIX: &str = "PCL:AstrometricSolution:";
/// Prefix of the legacy spline block.
const SPLINE_PREFIX: &str = "PCL:AstrometricSolution:SplineWorldTransformation:";
/// The properties a legacy solution needs.
const REQUIRED: [&str; 3] = [
    "PCL:AstrometricSolution:ReferenceCelestialCoordinates",
    "PCL:AstrometricSolution:ReferenceImageCoordinates",
    "PCL:AstrometricSolution:LinearTransformationMatrix",
];

/// True when any required legacy property or any legacy spline property is
/// present: the header claims a legacy solution.
pub(crate) fn has_legacy_block(props: &[XisfProperty]) -> bool {
    props
        .iter()
        .any(|p| REQUIRED.contains(&p.id.as_str()) || p.id.starts_with(SPLINE_PREFIX))
}

/// Decode and check the legacy block (see
/// [`AstrometricSolution::from_header`](super::AstrometricSolution::from_header)).
///
/// Linear-only solutions decode without a distortion. If any
/// `SplineWorldTransformation` property is present, both interpolation grids
/// must be present, well formed, and consistent with the linear solution;
/// otherwise the whole solution is refused.
pub(crate) fn decode(
    props: &[XisfProperty],
    dims: Option<(u64, u64)>,
) -> Result<PiSolution, String> {
    let lin = crate::wcs::from_xisf_properties(props).ok_or_else(|| why_unusable(props))?;
    // A missing projection is Gnomonic, as for the linear reader.
    let projection_system =
        string(props, "PCL:AstrometricSolution:ProjectionSystem").unwrap_or("Gnomonic".into());
    require_gnomonic(&projection_system)?;
    // Present and a 2-vector: the linear reader checked.
    let reference_image =
        vec2(props, REQUIRED[1]).ok_or("malformed reference image coordinates")?;
    let (reference_native, celestial_pole_native) = native_frame(
        props,
        "PCL:AstrometricSolution:ReferenceNativeCoordinates",
        "PCL:AstrometricSolution:CelestialPoleNativeCoordinates",
    )?;
    let s = |suffix: &str| string(props, &format!("{PREFIX}{suffix}"));
    let mut sol = PiSolution {
        block: PiBlock::Legacy,
        version: None,
        projection_system,
        reference_system: lin.radesys,
        reference_celestial: lin.crval,
        reference_image,
        reference_native,
        celestial_pole_native,
        linear: lin.cd,
        projective: None,
        distortion: None,
        control_points: None,
        catalog: s("Catalog"),
        creator_application: s("CreatorApplication"),
        creator_module: s("CreatorModule"),
        creator_os: s("CreatorOS"),
        creation_time: s("CreationTime"),
        notes: Vec::new(),
    };
    if props.iter().any(|p| p.id.starts_with(SPLINE_PREFIX)) {
        let grids = grid_from_properties(props, "ImageToNative")
            .zip(grid_from_properties(props, "NativeToImage"));
        let Some((image_to_native, native_to_image)) = grids else {
            return Err(
                "legacy spline solution present but its interpolation grids are missing or \
                 malformed"
                    .into(),
            );
        };
        sol.distortion = Some(Distortion::Grids {
            image_to_native,
            native_to_image,
        });
    }
    validate(&sol, dims)?;
    Ok(sol)
}

/// Why the linear reader refused the legacy properties, for the error.
fn why_unusable(props: &[XisfProperty]) -> String {
    let missing: Vec<&str> = REQUIRED
        .into_iter()
        .filter(|id| find_value(props, id).is_none())
        .collect();
    if missing.is_empty() {
        "legacy astrometric solution properties are present but invalid (unsupported \
         projection or malformed values)"
            .into()
    } else {
        format!(
            "incomplete legacy astrometric solution (missing: {})",
            missing.join(", ")
        )
    }
}

/// Read one `PointGridInterpolation` direction (`ImageToNative` or
/// `NativeToImage`) into a [`Grid2D`], validating dimensional consistency.
fn grid_from_properties(props: &[XisfProperty], dir: &str) -> Option<Grid2D> {
    let get = |suffix: &str| {
        find_value(
            props,
            &format!("{SPLINE_PREFIX}PointGridInterpolation:{dir}:{suffix}"),
        )
    };
    let rect_v = get("Rect")?.as_f64_vec()?;
    let delta = get("Delta")?.as_f64()?;
    let (rows, cols, gx) = get("GridX")?.as_f64_mat()?;
    let (ry, cy, gy) = get("GridY")?.as_f64_mat()?;
    if rect_v.len() != 4
        || !delta.is_finite()
        || delta <= 0.0
        || (rows, cols) != (ry, cy)
        || rows < 2
        || cols < 2
    {
        return None;
    }
    let rect = [rect_v[0], rect_v[1], rect_v[2], rect_v[3]];
    let n = rows as usize * cols as usize;
    if gx.len() != n || gy.len() != n {
        return None; // empty data ⇒ unresolved attachment block
    }
    // Node counts must match Rect + Delta: 1 + ⌈extent/Δ⌉ per axis (PixInsight
    // formula, verified across panels with both exact and fractional extents).
    if expected_nodes(rect[2] - rect[0], delta) != Some(cols)
        || expected_nodes(rect[3] - rect[1], delta) != Some(rows)
    {
        return None;
    }
    Some(Grid2D {
        rect,
        delta,
        rows,
        cols,
        gx: gx.to_vec(),
        gy: gy.to_vec(),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::astrometry::testprops::{header, prop};
    use crate::astrometry::{AstrometricSolution, Grid2D};
    use crate::formats::PropertyValue;

    /// Property set replicating the real Orion mosaic canvas solution.
    pub(crate) fn orion_props() -> Vec<XisfProperty> {
        const S: f64 = 4.436849683786501e-4;
        vec![
            prop(
                "PCL:AstrometricSolution:ReferenceCelestialCoordinates",
                "F64Vector",
                PropertyValue::F64Vec(vec![84.19917732858325, -3.240007111323072]),
            ),
            prop(
                "PCL:AstrometricSolution:ReferenceImageCoordinates",
                "F64Vector",
                PropertyValue::F64Vec(vec![4627.5, 9155.0]),
            ),
            prop(
                "PCL:AstrometricSolution:LinearTransformationMatrix",
                "F64Matrix",
                PropertyValue::F64Mat {
                    rows: 2,
                    cols: 2,
                    data: vec![-S, 0.0, 0.0, S],
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

    fn solve(props: Vec<XisfProperty>, dims: Option<(u64, u64)>) -> crate::Result<PiSolution> {
        match AstrometricSolution::from_header(&header(props, dims))?.expect("a solution") {
            AstrometricSolution::PixInsight(s) => Ok(s),
            AstrometricSolution::Wcs(w) => match w {},
        }
    }

    #[test]
    fn extracts_the_solution_from_pixinsight_properties() {
        let s = solve(orion_props(), Some((9255, 18310))).unwrap();
        assert_eq!(s.block, PiBlock::Legacy);
        assert_eq!(
            s.reference_celestial,
            [84.19917732858325, -3.240007111323072]
        );
        // FITS CRPIX would be [4628.0, 9155.5]: image coordinates + 0.5.
        assert_eq!(s.reference_image, [4627.5, 9155.0]);
        assert_eq!(s.linear[0][0], -4.436849683786501e-4);
        assert_eq!(s.linear[1][1], 4.436849683786501e-4);
        assert_eq!(s.linear[0][1], 0.0);
        assert_eq!(s.projection_system, "Gnomonic");
        assert_eq!(s.reference_system, "ICRS");
        assert!(s.distortion.is_none() && s.projective.is_none());
        assert_eq!(s.version, None);
    }

    #[test]
    fn missing_or_unknown_pieces_are_errors() {
        let mut p = orion_props();
        p.retain(|p| !p.id.ends_with("LinearTransformationMatrix"));
        let e = solve(p, None).unwrap_err().to_string();
        assert!(
            e.contains("LinearTransformationMatrix"),
            "matrix required: {e}"
        );

        let mut p = orion_props();
        p.iter_mut()
            .find(|p| p.id.ends_with("ProjectionSystem"))
            .unwrap()
            .value = PropertyValue::Str("FancyUnknownProjection".into());
        assert!(
            solve(p, None).is_err(),
            "unknown projection must not decode"
        );

        // A missing projection is Gnomonic.
        let mut p = orion_props();
        p.retain(|p| !p.id.ends_with("ProjectionSystem"));
        assert_eq!(solve(p, None).unwrap().projection_system, "Gnomonic");
    }

    #[test]
    fn center_pixel_maps_to_reference_coordinates() {
        let sol = AstrometricSolution::from_header(&header(orion_props(), None))
            .unwrap()
            .unwrap();
        // Canvas 9255×18310: the reference point is its centre pixel, at
        // FITS (4628.0, 9155.5) = image coordinates (4627.5, 9155.0).
        let (ra, dec) = sol.pixel_to_sky(4627.5, 9155.0);
        assert!((ra - 84.19917732858325).abs() < 1e-12);
        assert!((dec - -3.240007111323072).abs() < 1e-12);
        // Pixel scale ≈ 1.6″/px for this rig.
        assert!((sol.scale_arcsec_per_px() - 1.597).abs() < 0.001);
    }

    #[test]
    fn linear_path_matches_the_existing_linear_wcs() {
        let props = orion_props();
        let w = crate::wcs::from_xisf_properties(&props).unwrap();
        let sol = AstrometricSolution::from_header(&header(props, None))
            .unwrap()
            .unwrap();
        for (x, y) in [(0.0, 0.0), (100.5, 17000.25), (9254.0, 18309.0)] {
            let (a, b) = (sol.pixel_to_sky(x, y), w.pixel_to_sky(x + 0.5, y + 0.5));
            assert!((a.0 - b.0).abs() < 1e-12 && (a.1 - b.1).abs() < 1e-12);
        }
    }

    // ---- spline (grid) solutions ----

    fn grid_from_fn(rect: [f64; 4], delta: f64, f: impl Fn(f64, f64) -> (f64, f64)) -> Grid2D {
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

    fn grid_props(dir: &str, g: &Grid2D) -> Vec<XisfProperty> {
        let base = format!(
            "PCL:AstrometricSolution:SplineWorldTransformation:PointGridInterpolation:{dir}"
        );
        vec![
            prop(
                &format!("{base}:Rect"),
                "F64Vector",
                PropertyValue::F64Vec(g.rect.to_vec()),
            ),
            prop(
                &format!("{base}:Delta"),
                "Float64",
                PropertyValue::F64(g.delta),
            ),
            prop(
                &format!("{base}:GridX"),
                "F64Matrix",
                PropertyValue::F64Mat {
                    rows: g.rows,
                    cols: g.cols,
                    data: g.gx.clone(),
                },
            ),
            prop(
                &format!("{base}:GridY"),
                "F64Matrix",
                PropertyValue::F64Mat {
                    rows: g.rows,
                    cols: g.cols,
                    data: g.gy.clone(),
                },
            ),
        ]
    }

    /// Property set mimicking a raw solved panel (rotated ~90° like the real
    /// Orion raw panels), with distortion-free grids generated from the linear
    /// mapping itself — the grid path must then agree with the linear path.
    fn spline_props(w: f64, h: f64) -> Vec<XisfProperty> {
        const M: [[f64; 2]; 2] = [[-6.577e-6, 4.437e-4], [4.437e-4, 6.639e-6]];
        let crval = [82.895, -0.287];
        let refimg = [w / 2.0 - 3.7, h / 2.0 + 1.2];
        let lin = move |x: f64, y: f64| {
            let (dx, dy) = (x - refimg[0], y - refimg[1]);
            (M[0][0] * dx + M[0][1] * dy, M[1][0] * dx + M[1][1] * dy)
        };
        let det = M[0][0] * M[1][1] - M[0][1] * M[1][0];
        let inv = move |xi: f64, eta: f64| {
            (
                refimg[0] + (M[1][1] * xi - M[0][1] * eta) / det,
                refimg[1] + (M[0][0] * eta - M[1][0] * xi) / det,
            )
        };
        let i2n = grid_from_fn([0.0, 0.0, w, h], 64.0, lin);
        // Native bounds of the image (linear map ⇒ corners suffice), padded.
        let corners = [lin(0.0, 0.0), lin(w, 0.0), lin(0.0, h), lin(w, h)];
        let pad = 0.01;
        let nx0 = corners.iter().map(|c| c.0).fold(f64::INFINITY, f64::min) - pad;
        let nx1 = corners
            .iter()
            .map(|c| c.0)
            .fold(f64::NEG_INFINITY, f64::max)
            + pad;
        let ny0 = corners.iter().map(|c| c.1).fold(f64::INFINITY, f64::min) - pad;
        let ny1 = corners
            .iter()
            .map(|c| c.1)
            .fold(f64::NEG_INFINITY, f64::max)
            + pad;
        let n2i = grid_from_fn([nx0, ny0, nx1, ny1], 0.01, inv);

        let mut props = vec![
            prop(
                "PCL:AstrometricSolution:ReferenceCelestialCoordinates",
                "F64Vector",
                PropertyValue::F64Vec(crval.to_vec()),
            ),
            prop(
                "PCL:AstrometricSolution:ReferenceImageCoordinates",
                "F64Vector",
                PropertyValue::F64Vec(refimg.to_vec()),
            ),
            prop(
                "PCL:AstrometricSolution:LinearTransformationMatrix",
                "F64Matrix",
                PropertyValue::F64Mat {
                    rows: 2,
                    cols: 2,
                    data: vec![M[0][0], M[0][1], M[1][0], M[1][1]],
                },
            ),
            prop(
                "PCL:AstrometricSolution:ProjectionSystem",
                "String",
                PropertyValue::Str("Gnomonic".into()),
            ),
            prop(
                "PCL:AstrometricSolution:ReferenceNativeCoordinates",
                "F64Vector",
                PropertyValue::F64Vec(vec![0.0, 90.0]),
            ),
            prop(
                "PCL:AstrometricSolution:CelestialPoleNativeCoordinates",
                "F64Vector",
                PropertyValue::F64Vec(vec![180.0, 90.0]),
            ),
            prop(
                "PCL:AstrometricSolution:SplineWorldTransformation:Version",
                "String",
                PropertyValue::Str("2.0".into()),
            ),
        ];
        props.extend(grid_props("ImageToNative", &i2n));
        props.extend(grid_props("NativeToImage", &n2i));
        props
    }

    #[test]
    fn spline_model_grid_path_matches_linear_for_linear_grids() {
        let (w, h) = (1000u64, 700u64);
        let s = solve(spline_props(w as f64, h as f64), Some((w, h))).unwrap();
        assert!(matches!(s.distortion, Some(Distortion::Grids { .. })));
        let refimg = s.reference_image;
        for &(x, y) in &[(0.0, 0.0), (500.0, 350.0), (999.0, 699.0), (13.7, 683.2)] {
            let (ra_g, dec_g) = s.pixel_to_sky(x, y);
            let m = s.linear;
            let (dx, dy) = (x - refimg[0], y - refimg[1]);
            let (ra_l, dec_l) = crate::wcs::tan_deproject(
                s.reference_celestial,
                m[0][0] * dx + m[0][1] * dy,
                m[1][0] * dx + m[1][1] * dy,
            );
            assert!(
                (ra_g - ra_l).abs() < 1e-9 && (dec_g - dec_l).abs() < 1e-9,
                "({x},{y}): grid ({ra_g},{dec_g}) vs linear ({ra_l},{dec_l})"
            );
        }
        // The grids decode without the image size too (corner checks then
        // use the grid's own domain).
        assert!(solve(spline_props(w as f64, h as f64), None).is_ok());
    }

    #[test]
    fn spline_model_rejects_unverifiable_layouts() {
        let (w, h) = (1000u64, 700u64);
        let base = spline_props(w as f64, h as f64);
        let dims = Some((w, h));

        // Untouched base parses (guards the guards below).
        assert!(solve(base.clone(), dims).is_ok());

        // Spline marker present but grids missing: never approximate silently.
        let p: Vec<_> = base
            .iter()
            .filter(|p| !p.id.contains("PointGridInterpolation"))
            .cloned()
            .collect();
        assert!(solve(p, dims).is_err(), "grids required once spline");

        // Transposed matrix dims are inconsistent with Rect/Delta.
        let mut p = base.clone();
        for q in &mut p {
            if (q.id.contains(":GridX") || q.id.contains(":GridY"))
                && let PropertyValue::F64Mat { rows, cols, .. } = &mut q.value
            {
                std::mem::swap(rows, cols);
            }
        }
        assert!(solve(p, dims).is_err(), "transposed dims");

        // Corrupted grid values fail the reference-point/corner checks.
        let mut p = base.clone();
        for q in &mut p {
            if q.id.contains("ImageToNative:GridX")
                && let PropertyValue::F64Mat { data, .. } = &mut q.value
            {
                for v in data.iter_mut() {
                    *v += 0.5;
                }
            }
        }
        assert!(solve(p, dims).is_err(), "corrupt grid values");

        // Non-standard native frame: rotation math would differ.
        let mut p = base.clone();
        p.iter_mut()
            .find(|q| q.id.ends_with("ReferenceNativeCoordinates"))
            .unwrap()
            .value = PropertyValue::F64Vec(vec![0.0, 45.0]);
        assert!(solve(p, dims).is_err(), "non-standard native frame");

        // Geometry mismatch: grid rect is no longer the image bounds.
        assert!(
            solve(base.clone(), Some((2 * w, h))).is_err(),
            "geometry mismatch"
        );
    }
}
