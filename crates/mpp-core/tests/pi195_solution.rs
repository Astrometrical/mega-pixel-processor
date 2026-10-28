//! Decoding a real PixInsight 1.9.5 ImageSolver result.
//!
//! `fixtures/pi195_solved_header.json` is the serde form of a [`Header`]
//! captured from PixInsight 1.9.5 after ImageSolver solved a 4944×3284
//! frame of NGC 3199 (Gaia DR2, thin-plate-spline distortion). The values
//! were read through PixInsight's own scripting API, so the XISF type names
//! are inferred from the value shapes. Two cuts keep the fixture small: the
//! control points are the first 300 of 1,927, and PixInsight's private
//! `PCL:AstrometricSolution:Grid:*` lookup tables (20 MB) are left out.

use mpp_core::astrometry::{AstrometricSolution, Distortion, Kernel};
use mpp_core::header::Header;

fn header() -> Header {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/pi195_solved_header.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn pi(h: &Header) -> mpp_core::astrometry::PiSolution {
    match AstrometricSolution::from_header(h).unwrap().unwrap() {
        AstrometricSolution::PixInsight(s) => s,
        // `non_exhaustive`: downstream keeps a fallback arm.
        _ => panic!("expected a PixInsight solution"),
    }
}

/// Small-angle separation in arcseconds.
fn arcsec_apart(a: (f64, f64), b: (f64, f64)) -> f64 {
    let dra = ((a.0 - b.0 + 180.0).rem_euclid(360.0) - 180.0) * a.1.to_radians().cos();
    dra.hypot(a.1 - b.1) * 3600.0
}

#[test]
fn header_lookups() {
    let h = header();
    assert_eq!(h.keywords.len(), 59);
    assert_eq!(h.image_size, Some((4944, 3284)));
    assert_eq!(h.keyword("FOCALLEN").unwrap().value, "1678.9564");
    assert_eq!(
        h.keyword("FOCALLEN").unwrap().typed_value().as_f64(),
        Some(1678.9564)
    );
    assert_eq!(
        h.property("AstrometricSolution:Catalog")
            .unwrap()
            .value
            .as_str(),
        Some("Gaia DR2")
    );
    assert!(h.keyword("CRVAL1").is_none());
}

#[test]
fn decodes_every_layer_of_the_standard_block() {
    let s = pi(&header());
    assert!(s.notes.is_empty(), "{:?}", s.notes);
    assert_eq!(s.version.as_deref(), Some("1.0"));
    assert_eq!(s.projection_system, "Gnomonic");
    assert_eq!(s.reference_system, "ICRS");
    assert_eq!(
        s.reference_celestial,
        [160.7047966025759, -59.56630355500377]
    );
    assert_eq!(s.reference_image, [2472.122002839547, 1641.889425427978]);
    assert_eq!(s.reference_native, [0.0, 90.0]);
    assert_eq!(s.celestial_pole_native, [180.0, 90.0]);
    assert_eq!(
        s.linear,
        [
            [0.0001631012934272613, -1.027370935730033e-06],
            [1.046855682311985e-06, 0.0001631350388628197]
        ]
    );
    let p = s.projective.as_ref().expect("layer 2");
    assert_eq!(p.image_to_projection.0[2][2], 1.0);
    assert_eq!(p.projection_to_image.0[0][2], 2472.122024300335);
    let Some(Distortion::Rbf {
        image_to_projection,
        projection_to_image,
    }) = &s.distortion
    else {
        panic!("expected the RBF distortion model, got {:?}", s.distortion);
    };
    for m in [image_to_projection, projection_to_image] {
        let g = m.global.as_ref().expect("Global term");
        assert!(m.local.is_empty() && m.fallback.is_none());
        assert_eq!(g.x.kernel, Kernel::ThinPlateSpline);
        assert_eq!(g.x.order, 2);
        assert!(g.x.polynomial);
        assert!(g.x.nodes.len() > 1000 && g.y.nodes.len() > 1000);
    }
    let g = image_to_projection.global.as_ref().unwrap();
    assert_eq!(
        [g.x.x0, g.x.y0, g.x.r0],
        [2691.511303569924, 1864.563991393052, 0.0003171045129188285]
    );
    let cp = s.control_points.as_ref().expect("control points");
    assert_eq!(cp.image.len(), 300);
    assert_eq!(cp.celestial.len(), 300);
    assert_eq!(cp.image[0], [4831.56222945306, 2089.433310314643]);
    assert_eq!(s.catalog.as_deref(), Some("Gaia DR2"));
    assert_eq!(s.creator_application.as_deref(), Some("PixInsight 1.9.5"));
    assert_eq!(s.creator_module.as_deref(), Some("ImageSolver 6.5.0"));
    assert_eq!(s.creator_os.as_deref(), Some("Linux"));
    assert_eq!(s.creation_time.as_deref(), Some("2026-10-07T12:01:25.527Z"));
}

#[test]
fn common_queries() {
    let sol = AstrometricSolution::from_header(&header())
        .unwrap()
        .unwrap();
    assert_eq!(
        sol.reference_celestial(),
        (160.7047966025759, -59.56630355500377)
    );
    // 4.78 µm pixels at about 1679 mm: 0.587″/px.
    let scale = sol.scale_arcsec_per_px();
    assert!((scale - 0.5872).abs() < 0.0005, "scale {scale}");
    // The reference pixel maps to the reference point, give or take the
    // distortion residual there: 0.097″ on this frame (mmm measured 0.05″ on
    // its Orion panels).
    let (ra, dec) = sol.pixel_to_sky(2472.122002839547, 1641.889425427978);
    let sep = arcsec_apart((ra, dec), sol.reference_celestial());
    assert!(
        sep < 0.2,
        "reference pixel is {sep}″ from the reference point"
    );
}

/// The control points are the star matches the solution was fitted to, so
/// mapping each image point must land on its catalogue position to within
/// the fit residuals (sub-arcsecond on this frame).
#[test]
fn pixel_to_sky_reproduces_the_control_points() {
    let h = header();
    let sol = AstrometricSolution::from_header(&h).unwrap().unwrap();
    let cp = pi(&h).control_points.unwrap();
    let mut seps: Vec<f64> = cp
        .image
        .iter()
        .zip(&cp.celestial)
        .map(|(p, c)| arcsec_apart(sol.pixel_to_sky(p[0], p[1]), (c[0], c[1])))
        .collect();
    seps.sort_by(f64::total_cmp);
    let median = seps[seps.len() / 2];
    let worst = seps[seps.len() - 1];
    eprintln!("control-point residuals: median {median:.3}″, worst {worst:.3}″");
    // Measured: 0.095″ / 0.90″.
    assert!(median < 0.15, "median control-point residual {median}″");
    assert!(worst < 1.2, "worst control-point residual {worst}″");

    // The distortion model earns its keep: without layer 3 (projective
    // only) the median is 0.25″ on this frame.
    let mut projective_only = pi(&h);
    projective_only.distortion = None;
    let mut seps2: Vec<f64> = cp
        .image
        .iter()
        .zip(&cp.celestial)
        .map(|(p, c)| arcsec_apart(projective_only.pixel_to_sky(p[0], p[1]), (c[0], c[1])))
        .collect();
    seps2.sort_by(f64::total_cmp);
    let median2 = seps2[seps2.len() / 2];
    eprintln!("projective-only median {median2:.3}″");
    assert!(
        median < 0.6 * median2,
        "full model median {median}″ vs projective-only {median2}″"
    );
}

#[test]
fn solution_round_trips_through_serde() {
    let sol = AstrometricSolution::from_header(&header())
        .unwrap()
        .unwrap();
    let json = serde_json::to_string(&sol).unwrap();
    let back: AstrometricSolution = serde_json::from_str(&json).unwrap();
    assert_eq!(back, sol);
    assert_eq!(back.pixel_to_sky(10.0, 20.0), sol.pixel_to_sky(10.0, 20.0));
}

#[test]
fn projection_to_image_inverts_image_to_projection() {
    let mut s = pi(&header());
    let (w, h) = (4944.0, 3284.0);
    // PixInsight fits the two directions' splines independently, so with
    // layer 3 the round trip is only as good as that fit (≈ 0.1–0.4 px at
    // worst on this solve); layers 1–2 invert exactly.
    let check = |s: &mpp_core::astrometry::PiSolution, what: &str, tol: f64| {
        let mut worst = 0f64;
        for j in 0..9 {
            for i in 0..9 {
                let (x, y) = (w * i as f64 / 8.0, h * j as f64 / 8.0);
                let (xi, eta) = s.image_to_projection(x, y);
                let (bx, by) = s.projection_to_image(xi, eta);
                worst = worst.max((bx - x).hypot(by - y));
            }
        }
        assert!(worst < tol, "{what}: round trip off by {worst} px");
    };
    assert!(matches!(s.distortion, Some(Distortion::Rbf { .. })));
    check(&s, "layers 1-3", 0.5);
    s.distortion = None;
    check(&s, "layers 1-2", 1e-3);
    s.projective = None;
    check(&s, "layer 1", 1e-9);
}
