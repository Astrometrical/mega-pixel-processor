//! XISF 1.0 revision 1 standard astrometric solution (`AstrometricSolution:*`,
//! spec §11.5.3.7), the format PixInsight ≥ 1.9.5 writes. Copied from mmm
//! (`mmm-core/src/astrometry/standard.rs`), where it was verified on a real
//! 1.9.5-regenerated Orion panel (Global-only `ThinPlateSpline`, order 2,
//! separate X/Y node sets, 3.0k–3.2k nodes per component).
//!
//! Layers: 1 projection (required), 2 projective 3×3 in both directions,
//! 3 RBF distortion model in both directions (requires 2). A layer with a
//! missing/inconsistent property or an unrecognized vocabulary identifier is
//! *unavailable* and every higher layer with it; the decoder steps down and
//! records why in [`PiSolution::notes`]. Layer 4 (control points and
//! provenance) is read when present; the PixInsight-private
//! `PCL:AstrometricSolution:{Generation,Grid}:*` extras are ignored.

use super::distortion::{DistortionModel, Kernel, LocalTerm, ScalarSpline, VectorSpline};
use super::{
    ControlPoints, Distortion, Homography, PiBlock, PiSolution, Projective, find_value,
    native_frame, points, require_gnomonic, string, validate,
};
use crate::formats::{PropertyValue, XisfProperty};

/// Prefix of every standard solution property.
pub(crate) const STD_PREFIX: &str = "AstrometricSolution:";

/// True when the standard block's required `Version` property is present.
pub(crate) fn has_standard_block(props: &[XisfProperty]) -> bool {
    find_value(props, "AstrometricSolution:Version").is_some()
}

fn require<'a>(props: &'a [XisfProperty], suffix: &str) -> Result<&'a PropertyValue, String> {
    find_value(props, &format!("{STD_PREFIX}{suffix}"))
        .ok_or_else(|| format!("missing AstrometricSolution:{suffix}"))
}

fn version(props: &[XisfProperty]) -> Result<String, String> {
    let v = require(props, "Version")?
        .as_str()
        .ok_or("AstrometricSolution:Version is not a string")?;
    let major = v
        .trim()
        .split('.')
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .ok_or_else(|| format!("malformed AstrometricSolution:Version '{v}'"))?;
    if major != 1 {
        return Err(format!(
            "unsupported AstrometricSolution major revision {major} (this decoder implements 1.x)"
        ));
    }
    Ok(v.to_string())
}

/// Layer 1 alone, as a FITS-convention [`LinearWcs`](crate::wcs::LinearWcs):
/// deliberately lenient — any projection with a FITS code, native frame
/// unchecked — so a solution whose higher layers are unusable still yields
/// its linear WCS (e.g. for an output header).
pub(crate) fn linear_layer(props: &[XisfProperty]) -> Result<crate::wcs::LinearWcs, String> {
    version(props)?;
    let proj = require(props, "ProjectionSystem")?
        .as_str()
        .ok_or("ProjectionSystem is not a string")?;
    let code = crate::wcs::projection_code(proj)
        .ok_or_else(|| format!("unrecognized ProjectionSystem '{proj}'"))?;
    let crval = require(props, "ReferenceCelestialCoordinates")?
        .as_f64_vec()
        .ok_or("ReferenceCelestialCoordinates is not a vector")?;
    let refimg = require(props, "ReferenceImageCoordinates")?
        .as_f64_vec()
        .ok_or("ReferenceImageCoordinates is not a vector")?;
    let (rows, cols, m) = require(props, "LinearTransformationMatrix")?
        .as_f64_mat()
        .ok_or("LinearTransformationMatrix is not a matrix")?;
    if crval.len() != 2 || refimg.len() != 2 || (rows, cols) != (2, 2) || m.len() != 4 {
        return Err("layer 1 vector/matrix dimensions are inconsistent".into());
    }
    if crval.iter().chain(refimg).chain(m).any(|v| !v.is_finite()) {
        return Err("layer 1 carries non-finite values".into());
    }
    Ok(crate::wcs::LinearWcs {
        crval: [crval[0], crval[1]],
        // PixInsight image coords (centres at k + 0.5, 0-based) -> FITS
        // (centres at integers, 1-based): +0.5 on both axes.
        crpix: [refimg[0] + 0.5, refimg[1] + 0.5],
        cd: [[m[0], m[1]], [m[2], m[3]]],
        ctype: [
            format!("{:-<5}{code}", "RA"),
            format!("{:-<5}{code}", "DEC"),
        ],
        radesys: string(props, &format!("{STD_PREFIX}CelestialReferenceSystem"))
            .unwrap_or_else(|| "ICRS".into()),
    })
}

/// Layer 1, plus the native frame, into a solution with no higher layers.
fn decode_layer1(props: &[XisfProperty]) -> Result<PiSolution, String> {
    let version = version(props)?;
    let proj = require(props, "ProjectionSystem")?
        .as_str()
        .ok_or("ProjectionSystem is not a string")?;
    require_gnomonic(proj)?;
    let crval = require(props, "ReferenceCelestialCoordinates")?
        .as_f64_vec()
        .ok_or("ReferenceCelestialCoordinates is not a vector")?;
    let refimg = require(props, "ReferenceImageCoordinates")?
        .as_f64_vec()
        .ok_or("ReferenceImageCoordinates is not a vector")?;
    let (rows, cols, m) = require(props, "LinearTransformationMatrix")?
        .as_f64_mat()
        .ok_or("LinearTransformationMatrix is not a matrix")?;
    if crval.len() != 2 || refimg.len() != 2 || (rows, cols) != (2, 2) || m.len() != 4 {
        return Err("layer 1 vector/matrix dimensions are inconsistent".into());
    }
    if crval.iter().chain(refimg).chain(m).any(|v| !v.is_finite()) {
        return Err("layer 1 carries non-finite values".into());
    }
    let (reference_native, celestial_pole_native) = native_frame(
        props,
        "AstrometricSolution:ReferenceNativeCoordinates",
        "AstrometricSolution:CelestialPoleNativeCoordinates",
    )?;
    let s = |suffix: &str| string(props, &format!("{STD_PREFIX}{suffix}"));
    Ok(PiSolution {
        block: PiBlock::Standard,
        version: Some(version),
        projection_system: proj.to_string(),
        // The standard block's own property only; the legacy
        // Observation:CelestialReferenceSystem does not apply here.
        reference_system: s("CelestialReferenceSystem").unwrap_or_else(|| "ICRS".into()),
        reference_celestial: [crval[0], crval[1]],
        reference_image: [refimg[0], refimg[1]],
        reference_native,
        celestial_pole_native,
        linear: [[m[0], m[1]], [m[2], m[3]]],
        projective: None,
        distortion: None,
        control_points: None,
        catalog: s("Catalog"),
        creator_application: s("CreatorApplication"),
        creator_module: s("CreatorModule"),
        creator_os: s("CreatorOS"),
        creation_time: s("CreationTime"),
        notes: Vec::new(),
    })
}

fn homography(props: &[XisfProperty], suffix: &str) -> Result<Homography, String> {
    let (r, c, d) = require(props, suffix)?
        .as_f64_mat()
        .ok_or_else(|| format!("{suffix} is not a matrix"))?;
    if (r, c) != (3, 3) || d.len() != 9 || d.iter().any(|v| !v.is_finite()) {
        return Err(format!("{suffix} must be a finite 3×3 matrix"));
    }
    Ok(Homography([
        [d[0], d[1], d[2]],
        [d[3], d[4], d[5]],
        [d[6], d[7], d[8]],
    ]))
}

/// Kernel/order/polynomial settings shared by every spline of a direction.
#[derive(Clone, Copy)]
struct Basis {
    kernel: Kernel,
    order: u32,
    polynomial: bool,
}

/// Read one scalar spline record at prefix `p` (e.g. `…:Global:X:`), sharing
/// nodes/normalization/shape from `shared` (`…:Global:X:`) when absent.
fn scalar_spline(
    props: &[XisfProperty],
    p: &str,
    shared: Option<&str>,
    basis: Basis,
) -> Result<ScalarSpline, String> {
    let get = |pre: &str, s: &str| find_value(props, &format!("{pre}{s}"));
    let (nodes_v, norm_v, shape_v) = match (get(p, "Nodes"), get(p, "Normalization")) {
        (Some(n), Some(nm)) => (n, nm, get(p, "ShapeParameter")),
        _ => {
            let sh = shared.ok_or_else(|| format!("missing {p}Nodes"))?;
            (
                get(sh, "Nodes").ok_or_else(|| format!("missing {sh}Nodes"))?,
                get(sh, "Normalization").ok_or_else(|| format!("missing {sh}Normalization"))?,
                get(p, "ShapeParameter").or_else(|| get(sh, "ShapeParameter")),
            )
        }
    };
    let (r, c, nd) = nodes_v
        .as_f64_mat()
        .ok_or_else(|| format!("{p}Nodes is not a matrix"))?;
    if c != 2 || nd.len() != (r as usize) * 2 {
        return Err(format!("{p}Nodes must be n×2"));
    }
    let nodes: Vec<[f64; 2]> = nd.as_chunks::<2>().0.to_vec();
    let norm = norm_v
        .as_f64_vec()
        .ok_or_else(|| format!("{p}Normalization is not a vector"))?;
    if norm.len() != 3 {
        return Err(format!("{p}Normalization must have 3 elements"));
    }
    let coef = get(p, "Coefficients")
        .ok_or_else(|| format!("missing {p}Coefficients"))?
        .as_f64_vec()
        .ok_or_else(|| format!("{p}Coefficients is not a vector"))?
        .to_vec();
    let eps2 = if basis.kernel.has_shape() {
        let e = shape_v
            .ok_or_else(|| format!("missing {p}ShapeParameter"))?
            .as_f64()
            .ok_or_else(|| format!("{p}ShapeParameter is not a number"))?;
        e * e
    } else {
        if shape_v.is_some() {
            return Err(format!(
                "{p}ShapeParameter must not be specified for this basis function"
            ));
        }
        0.0
    };
    let s = ScalarSpline {
        kernel: basis.kernel,
        order: basis.order,
        polynomial: basis.polynomial,
        x0: norm[0],
        y0: norm[1],
        r0: norm[2],
        nodes,
        coef,
        eps2,
    };
    s.validate().map_err(|e| format!("{p}: {e}"))?;
    Ok(s)
}

fn vector_spline(props: &[XisfProperty], t: &str, basis: Basis) -> Result<VectorSpline, String> {
    let px = format!("{t}X:");
    let py = format!("{t}Y:");
    let x = scalar_spline(props, &px, None, basis)?;
    let y = scalar_spline(props, &py, Some(&px), basis)?;
    Ok(VectorSpline { x, y })
}

/// Layer 3 for one direction; `Err` means unavailable (with the reason).
fn distortion_model(props: &[XisfProperty], dir: &str) -> Result<DistortionModel, String> {
    let p = format!("{STD_PREFIX}DistortionModel:{dir}:");
    let get = |s: &str| find_value(props, &format!("{p}{s}"));
    let kernel_id = get("BasisFunction")
        .ok_or_else(|| format!("missing {p}BasisFunction"))?
        .as_str()
        .ok_or("BasisFunction is not a string")?;
    let kernel = Kernel::parse(kernel_id)
        .ok_or_else(|| format!("unrecognized BasisFunction '{kernel_id}'"))?;
    let order = get("Order")
        .ok_or_else(|| format!("missing {p}Order"))?
        .as_f64()
        .ok_or("Order is not a number")?;
    if !(2.0..=16.0).contains(&order) || order.fract() != 0.0 {
        return Err(format!("invalid Order {order}"));
    }
    let polynomial = match get("Polynomial") {
        None => true,
        Some(v) => v
            .as_f64()
            .map(|b| b != 0.0)
            .ok_or("Polynomial is not a boolean")?,
    };
    let basis = Basis {
        kernel,
        order: order as u32,
        polynomial,
    };
    let terms_s = get("Terms")
        .ok_or_else(|| format!("missing {p}Terms"))?
        .as_str()
        .ok_or("Terms is not a string")?;
    let (mut has_global, mut has_local, mut has_fallback) = (false, false, false);
    for kind in terms_s.lines().map(str::trim).filter(|s| !s.is_empty()) {
        match kind {
            "Global" => has_global = true,
            "Local" => has_local = true,
            "Fallback" => has_fallback = true,
            other => return Err(format!("unrecognized term kind '{other}' in {p}Terms")),
        }
    }
    let global = if has_global {
        Some(vector_spline(props, &format!("{p}Global:"), basis)?)
    } else {
        None
    };
    let local = if has_local {
        local_terms(props, &p, basis)?
    } else {
        Vec::new()
    };
    let fallback = if has_fallback {
        let t0 = get("Fallback:Threshold")
            .ok_or_else(|| format!("missing {p}Fallback:Threshold"))?
            .as_f64()
            .ok_or("Fallback:Threshold is not a number")?;
        Some((t0, vector_spline(props, &format!("{p}Fallback:"), basis)?))
    } else {
        None
    };
    DistortionModel::new(global, local, fallback)
}

/// One component's packed Local arrays, unpacked into per-term splines.
fn local_component(
    props: &[XisfProperty],
    p: &str,
    c: &str,
    n: usize,
    basis: Basis,
) -> Result<Vec<ScalarSpline>, String> {
    let need = |s: &str| {
        find_value(props, &format!("{p}Local:{c}:{s}"))
            .ok_or_else(|| format!("missing {p}Local:{c}:{s}"))
    };
    let q = ScalarSpline::poly_terms(basis.order, basis.polynomial);
    let (nr, nc, norm) = need("Normalization")?
        .as_f64_mat()
        .ok_or_else(|| format!("Local:{c}:Normalization is not a matrix"))?;
    if (nr as usize, nc) != (n, 3) || norm.len() != n * 3 {
        return Err(format!("Local:{c}:Normalization must be N×3"));
    }
    let off = need("NodeOffsets")?
        .as_f64_vec()
        .ok_or_else(|| format!("Local:{c}:NodeOffsets is not a vector"))?;
    if off.len() != n + 1
        || off[0] != 0.0
        || off
            .windows(2)
            .any(|w| w[1] - w[0] < 3.0 || w[1].fract() != 0.0)
    {
        return Err(format!("invalid Local:{c}:NodeOffsets"));
    }
    let total = off[n] as usize;
    let (tr, tc, nodes) = need("Nodes")?
        .as_f64_mat()
        .ok_or_else(|| format!("Local:{c}:Nodes is not a matrix"))?;
    if tc != 2 || tr as usize != total || nodes.len() != total * 2 {
        return Err(format!("Local:{c}:Nodes must be {total}×2"));
    }
    let coef = need("Coefficients")?
        .as_f64_vec()
        .ok_or_else(|| format!("Local:{c}:Coefficients is not a vector"))?;
    if coef.len() != total + n * q {
        return Err(format!(
            "Local:{c}:Coefficients length != total nodes + N·q"
        ));
    }
    let shape_prop = find_value(props, &format!("{p}Local:{c}:ShapeParameter"));
    let shapes = if basis.kernel.has_shape() {
        let s = shape_prop
            .ok_or_else(|| format!("missing {p}Local:{c}:ShapeParameter"))?
            .as_f64_vec()
            .ok_or_else(|| format!("Local:{c}:ShapeParameter is not a vector"))?;
        if s.len() != n {
            return Err(format!("Local:{c}:ShapeParameter length != N"));
        }
        Some(s)
    } else {
        if shape_prop.is_some() {
            return Err(format!("Local:{c}:ShapeParameter must not be specified"));
        }
        None
    };
    let mut out = Vec::with_capacity(n);
    for k in 0..n {
        let (o0, o1) = (off[k] as usize, off[k + 1] as usize);
        let s = ScalarSpline {
            kernel: basis.kernel,
            order: basis.order,
            polynomial: basis.polynomial,
            x0: norm[k * 3],
            y0: norm[k * 3 + 1],
            r0: norm[k * 3 + 2],
            nodes: nodes[o0 * 2..o1 * 2].as_chunks::<2>().0.to_vec(),
            coef: coef[o0 + k * q..o1 + (k + 1) * q].to_vec(),
            eps2: shapes.map_or(0.0, |s| s[k] * s[k]),
        };
        s.validate()
            .map_err(|e| format!("Local term {k} {c}: {e}"))?;
        out.push(s);
    }
    Ok(out)
}

/// Unpack the packed Local term arrays (spec §11.5.3.7.4.4).
fn local_terms(props: &[XisfProperty], p: &str, basis: Basis) -> Result<Vec<LocalTerm>, String> {
    let get = |s: &str| find_value(props, &format!("{p}Local:{s}"));
    let (n, cc, center) = get("Center")
        .ok_or_else(|| format!("missing {p}Local:Center"))?
        .as_f64_mat()
        .ok_or("Local:Center is not a matrix")?;
    let n = n as usize;
    if cc != 2 || n < 1 || center.len() != n * 2 {
        return Err("Local:Center must be N×2 with N ≥ 1".into());
    }
    let radius = get("Radius")
        .ok_or_else(|| format!("missing {p}Local:Radius"))?
        .as_f64_vec()
        .ok_or("Local:Radius is not a vector")?;
    if radius.len() != n {
        return Err("Local:Radius length != N".into());
    }
    let q = ScalarSpline::poly_terms(basis.order, basis.polynomial);

    let xs = local_component(props, p, "X", n, basis)?;
    let ys = if get("Y:Nodes").is_some() {
        local_component(props, p, "Y", n, basis)?
    } else {
        // Y shares X's nodes, normalization and shape; only coefficients differ.
        let coef = get("Y:Coefficients")
            .ok_or_else(|| format!("missing {p}Local:Y:Coefficients"))?
            .as_f64_vec()
            .ok_or("Local:Y:Coefficients is not a vector")?;
        let total: usize = xs.iter().map(|s| s.nodes.len()).sum();
        if coef.len() != total + n * q {
            return Err("Local:Y:Coefficients length != total nodes + N·q".into());
        }
        let mut out = Vec::with_capacity(n);
        let mut o = 0;
        for (k, x) in xs.iter().enumerate() {
            let m = x.nodes.len();
            let mut y = x.clone();
            y.coef = coef[o + k * q..o + m + (k + 1) * q].to_vec();
            o += m;
            out.push(y);
        }
        out
    };
    Ok(xs
        .into_iter()
        .zip(ys)
        .enumerate()
        .map(|(k, (x, y))| LocalTerm {
            center: [center[k * 2], center[k * 2 + 1]],
            radius: radius[k],
            spline: VectorSpline { x, y },
        })
        .collect())
}

/// Decode the standard block without the consistency check. `Err` only when
/// layer 1 is unusable (or the major revision, projection or native frame is
/// unsupported); higher layers degrade with a note.
pub(crate) fn parse(props: &[XisfProperty]) -> Result<PiSolution, String> {
    let mut sol = decode_layer1(props)?;
    sol.control_points = control_points(props, &mut sol.notes);
    let has_layer2 = props.iter().any(|p| {
        p.id.starts_with("AstrometricSolution:ProjectiveTransformation:")
    });
    let has_layer3 = props
        .iter()
        .any(|p| p.id.starts_with("AstrometricSolution:DistortionModel:"));
    let projective =
        homography(props, "ProjectiveTransformation:ImageToProjection").and_then(|a| {
            homography(props, "ProjectiveTransformation:ProjectionToImage").map(|b| (a, b))
        });
    let (i2p, p2i) = match projective {
        Ok(h) => h,
        Err(e) => {
            if has_layer2 || has_layer3 {
                sol.notes.push(format!(
                    "layer 2 (projective transformation) unavailable: {e}; using the linear solution"
                ));
            }
            return Ok(sol);
        }
    };
    sol.projective = Some(Projective {
        image_to_projection: i2p,
        projection_to_image: p2i,
    });
    if has_layer3 {
        match (
            distortion_model(props, "ImageToProjection"),
            distortion_model(props, "ProjectionToImage"),
        ) {
            (Ok(a), Ok(b)) => {
                sol.distortion = Some(Distortion::Rbf {
                    image_to_projection: a,
                    projection_to_image: b,
                })
            }
            (Err(e), _) | (_, Err(e)) => sol.notes.push(format!(
                "layer 3 (distortion model) unavailable: {e}; using the projective transformation"
            )),
        }
    }
    Ok(sol)
}

/// `ControlPoints:Image` and `:Celestial`, when both are present and match.
fn control_points(props: &[XisfProperty], notes: &mut Vec<String>) -> Option<ControlPoints> {
    let image_id = "AstrometricSolution:ControlPoints:Image";
    let sky_id = "AstrometricSolution:ControlPoints:Celestial";
    if find_value(props, image_id).is_none() && find_value(props, sky_id).is_none() {
        return None;
    }
    match (points(props, image_id), points(props, sky_id)) {
        (Some(image), Some(celestial)) if image.len() == celestial.len() => {
            Some(ControlPoints { image, celestial })
        }
        _ => {
            notes.push(
                "control points unavailable: ControlPoints:Image and :Celestial must be \
                 n×2 matrices of equal length"
                    .into(),
            );
            None
        }
    }
}

/// Decode and check the standard block (see
/// [`AstrometricSolution::from_header`](super::AstrometricSolution::from_header)).
pub(crate) fn decode(
    props: &[XisfProperty],
    dims: Option<(u64, u64)>,
) -> Result<PiSolution, String> {
    let sol = parse(props)?;
    validate(&sol, dims)?;
    Ok(sol)
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! Property-array builders for standard-block tests (also used by the
    //! `mod.rs` dispatch tests).
    use super::*;
    use crate::astrometry::testprops::prop;

    fn vec2(id: &str, v: [f64; 2]) -> XisfProperty {
        prop(id, "F64Vector", PropertyValue::F64Vec(v.to_vec()))
    }
    fn mat(id: &str, rows: u32, cols: u32, data: Vec<f64>) -> XisfProperty {
        prop(id, "F64Matrix", PropertyValue::F64Mat { rows, cols, data })
    }
    fn s(id: &str, v: &str) -> XisfProperty {
        prop(id, "String", PropertyValue::Str(v.into()))
    }

    /// Layer 1 (Gnomonic, ICRS).
    pub(crate) fn layer1(
        crval: [f64; 2],
        refimg: [f64; 2],
        cd: [[f64; 2]; 2],
    ) -> Vec<XisfProperty> {
        vec![
            s("AstrometricSolution:Version", "1.0"),
            s("AstrometricSolution:ProjectionSystem", "Gnomonic"),
            vec2("AstrometricSolution:ReferenceCelestialCoordinates", crval),
            vec2("AstrometricSolution:ReferenceImageCoordinates", refimg),
            mat(
                "AstrometricSolution:LinearTransformationMatrix",
                2,
                2,
                vec![cd[0][0], cd[0][1], cd[1][0], cd[1][1]],
            ),
            s("AstrometricSolution:CelestialReferenceSystem", "ICRS"),
        ]
    }

    /// Layer 2: both 3×3 matrices.
    pub(crate) fn layer2(props: &mut Vec<XisfProperty>, i2p: [[f64; 3]; 3], p2i: [[f64; 3]; 3]) {
        let flat = |m: [[f64; 3]; 3]| m.iter().flatten().copied().collect::<Vec<_>>();
        props.push(mat(
            "AstrometricSolution:ProjectiveTransformation:ImageToProjection",
            3,
            3,
            flat(i2p),
        ));
        props.push(mat(
            "AstrometricSolution:ProjectiveTransformation:ProjectionToImage",
            3,
            3,
            flat(p2i),
        ));
    }

    fn kernel_id(k: Kernel) -> &'static str {
        match k {
            Kernel::ThinPlateSpline => "ThinPlateSpline",
            Kernel::VariableOrder => "VariableOrder",
            Kernel::Gaussian => "Gaussian",
            Kernel::Multiquadric => "Multiquadric",
            Kernel::InverseMultiquadric => "InverseMultiquadric",
            Kernel::InverseQuadratic => "InverseQuadratic",
        }
    }

    fn scalar_record(props: &mut Vec<XisfProperty>, p: &str, sp: &ScalarSpline, with_nodes: bool) {
        if with_nodes {
            props.push(prop(
                &format!("{p}Normalization"),
                "F64Vector",
                PropertyValue::F64Vec(vec![sp.x0, sp.y0, sp.r0]),
            ));
            props.push(mat(
                &format!("{p}Nodes"),
                sp.nodes.len() as u32,
                2,
                sp.nodes.iter().flatten().copied().collect(),
            ));
        }
        props.push(prop(
            &format!("{p}Coefficients"),
            "F64Vector",
            PropertyValue::F64Vec(sp.coef.clone()),
        ));
        if sp.kernel.has_shape() {
            props.push(prop(
                &format!("{p}ShapeParameter"),
                "Float64",
                PropertyValue::F64(sp.eps2.sqrt()),
            ));
        }
    }

    fn header(props: &mut Vec<XisfProperty>, p: &str, sp: &ScalarSpline, terms: &str) {
        props.push(s(&format!("{p}BasisFunction"), kernel_id(sp.kernel)));
        props.push(prop(
            &format!("{p}Order"),
            "Int32",
            PropertyValue::I64(sp.order as i64),
        ));
        props.push(prop(
            &format!("{p}Polynomial"),
            "Boolean",
            PropertyValue::I64(sp.polynomial as i64),
        ));
        props.push(s(&format!("{p}Terms"), terms));
    }

    /// `dir` is `ImageToProjection` or `ProjectionToImage`. Y shares X's nodes
    /// only when they are identical (then Y:Nodes/Normalization are omitted).
    pub(crate) fn layer3_global(props: &mut Vec<XisfProperty>, dir: &str, v: &VectorSpline) {
        let p = format!("AstrometricSolution:DistortionModel:{dir}:");
        header(props, &p, &v.x, "Global");
        scalar_record(props, &format!("{p}Global:X:"), &v.x, true);
        let shared = v.x.nodes == v.y.nodes
            && v.x.x0 == v.y.x0
            && v.x.y0 == v.y.y0
            && v.x.r0 == v.y.r0
            && v.x.eps2 == v.y.eps2;
        scalar_record(props, &format!("{p}Global:Y:"), &v.y, !shared);
    }

    /// Local terms packed per the spec (shared Y nodes when every term's Y
    /// spline shares its X nodes; otherwise separate Y arrays), plus an
    /// optional Fallback record and an optional extra Global term.
    pub(crate) fn layer3_local(
        props: &mut Vec<XisfProperty>,
        dir: &str,
        terms: &[LocalTerm],
        fallback: Option<(f64, &VectorSpline)>,
        extra_global: Option<&VectorSpline>,
    ) {
        let p = format!("AstrometricSolution:DistortionModel:{dir}:");
        let s0 = &terms[0].spline.x;
        let mut kinds = Vec::new();
        if extra_global.is_some() {
            kinds.push("Global");
        }
        kinds.push("Local");
        if fallback.is_some() {
            kinds.push("Fallback");
        }
        header(props, &p, s0, &kinds.join("\n"));
        if let Some(g) = extra_global {
            scalar_record(props, &format!("{p}Global:X:"), &g.x, true);
            scalar_record(props, &format!("{p}Global:Y:"), &g.y, true);
        }
        let separate = terms.iter().any(|t| t.spline.x.nodes != t.spline.y.nodes);
        let (mut center, mut radius, mut norm, mut off, mut nodes, mut cx) =
            (vec![], vec![], vec![], vec![0.0], vec![], vec![]);
        let (mut norm2, mut off2, mut nodes2, mut cy) = (vec![], vec![0.0], vec![], vec![]);
        let mut shapes = vec![];
        let mut shapes2 = vec![];
        for t in terms {
            center.extend(t.center);
            radius.push(t.radius);
            let x = &t.spline.x;
            norm.extend([x.x0, x.y0, x.r0]);
            nodes.extend(x.nodes.iter().flatten().copied());
            off.push(off.last().unwrap() + x.nodes.len() as f64);
            cx.extend(&x.coef);
            shapes.push(x.eps2.sqrt());
            let y = &t.spline.y;
            if separate {
                norm2.extend([y.x0, y.y0, y.r0]);
                nodes2.extend(y.nodes.iter().flatten().copied());
                off2.push(off2.last().unwrap() + y.nodes.len() as f64);
                shapes2.push(y.eps2.sqrt());
            }
            cy.extend(&y.coef);
        }
        let n = terms.len() as u32;
        props.push(mat(&format!("{p}Local:Center"), n, 2, center));
        props.push(prop(
            &format!("{p}Local:Radius"),
            "F64Vector",
            PropertyValue::F64Vec(radius),
        ));
        props.push(mat(&format!("{p}Local:X:Normalization"), n, 3, norm));
        props.push(prop(
            &format!("{p}Local:X:NodeOffsets"),
            "I32Vector",
            PropertyValue::F64Vec(off),
        ));
        props.push(mat(
            &format!("{p}Local:X:Nodes"),
            (nodes.len() / 2) as u32,
            2,
            nodes,
        ));
        props.push(prop(
            &format!("{p}Local:X:Coefficients"),
            "F64Vector",
            PropertyValue::F64Vec(cx),
        ));
        if s0.kernel.has_shape() {
            props.push(prop(
                &format!("{p}Local:X:ShapeParameter"),
                "F64Vector",
                PropertyValue::F64Vec(shapes),
            ));
        }
        if separate {
            props.push(mat(&format!("{p}Local:Y:Normalization"), n, 3, norm2));
            props.push(prop(
                &format!("{p}Local:Y:NodeOffsets"),
                "I32Vector",
                PropertyValue::F64Vec(off2),
            ));
            props.push(mat(
                &format!("{p}Local:Y:Nodes"),
                (nodes2.len() / 2) as u32,
                2,
                nodes2,
            ));
            if s0.kernel.has_shape() {
                props.push(prop(
                    &format!("{p}Local:Y:ShapeParameter"),
                    "F64Vector",
                    PropertyValue::F64Vec(shapes2),
                ));
            }
        }
        props.push(prop(
            &format!("{p}Local:Y:Coefficients"),
            "F64Vector",
            PropertyValue::F64Vec(cy),
        ));
        if let Some((t0, f)) = fallback {
            props.push(prop(
                &format!("{p}Fallback:Threshold"),
                "Float64",
                PropertyValue::F64(t0),
            ));
            scalar_record(props, &format!("{p}Fallback:X:"), &f.x, true);
            scalar_record(props, &format!("{p}Fallback:Y:"), &f.y, true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use crate::astrometry::distortion::testfit::fit;
    use crate::astrometry::testprops::prop;

    const S: f64 = 4.4e-4;
    fn base() -> Vec<XisfProperty> {
        layer1([84.2, -3.24], [2449.0, 1615.0], [[-S, 0.0], [0.0, S]])
    }
    fn affine_h(a: [[f64; 2]; 2], t: [f64; 2]) -> [[f64; 3]; 3] {
        [
            [a[0][0], a[0][1], t[0]],
            [a[1][0], a[1][1], t[1]],
            [0.0, 0.0, 1.0],
        ]
    }
    fn lattice(n: usize, w: f64, h: f64) -> Vec<[f64; 2]> {
        let mut v = Vec::new();
        for i in 0..n {
            for j in 0..n {
                v.push([i as f64 * w / (n - 1) as f64, j as f64 * h / (n - 1) as f64]);
            }
        }
        v
    }
    fn vector_from(nodes: &[[f64; 2]], f: impl Fn(f64, f64) -> (f64, f64)) -> VectorSpline {
        let zx: Vec<f64> = nodes.iter().map(|p| f(p[0], p[1]).0).collect();
        let zy: Vec<f64> = nodes.iter().map(|p| f(p[0], p[1]).1).collect();
        VectorSpline {
            x: fit(Kernel::ThinPlateSpline, 2, true, 0.0, nodes, &zx),
            y: fit(Kernel::ThinPlateSpline, 2, true, 0.0, nodes, &zy),
        }
    }
    fn exact_layer2(props: &mut Vec<XisfProperty>) {
        layer2(
            props,
            affine_h([[-S, 0.0], [0.0, S]], [0.0, 0.0]),
            affine_h([[-1.0 / S, 0.0], [0.0, 1.0 / S]], [0.0, 0.0]),
        );
    }
    fn rbf(sol: &PiSolution) -> (&DistortionModel, &DistortionModel) {
        match &sol.distortion {
            Some(Distortion::Rbf {
                image_to_projection,
                projection_to_image,
            }) => (image_to_projection, projection_to_image),
            other => panic!("expected an RBF distortion, got {other:?}"),
        }
    }

    #[test]
    fn layer1_gives_the_linear_solution_with_standard_conventions() {
        let sol = parse(&base()).unwrap();
        assert_eq!(sol.reference_celestial, [84.2, -3.24]);
        // FITS CRPIX would be [2449.5, 1615.5]: image coordinates + 0.5.
        assert_eq!(sol.reference_image, [2449.0, 1615.0]);
        assert_eq!(sol.projection_system, "Gnomonic");
        assert_eq!(sol.reference_system, "ICRS");
        assert_eq!(sol.reference_native, [0.0, 90.0]);
        assert_eq!(sol.celestial_pole_native, [180.0, 90.0]);
        assert_eq!(sol.version.as_deref(), Some("1.0"));
        assert_eq!(sol.block, PiBlock::Standard);
        assert!(sol.projective.is_none() && sol.distortion.is_none());
        assert!(sol.notes.is_empty());
        assert!(has_standard_block(&base()));
    }

    #[test]
    fn celestial_reference_system_defaults_to_icrs_and_overrides_observation() {
        let mut p = base();
        p.retain(|x| x.id != "AstrometricSolution:CelestialReferenceSystem");
        p.push(prop(
            "Observation:CelestialReferenceSystem",
            "String",
            PropertyValue::Str("GCRS".into()),
        ));
        assert_eq!(parse(&p).unwrap().reference_system, "ICRS");
        p.push(prop(
            "AstrometricSolution:CelestialReferenceSystem",
            "String",
            PropertyValue::Str("GCRS".into()),
        ));
        assert_eq!(parse(&p).unwrap().reference_system, "GCRS");
    }

    #[test]
    fn unsupported_major_version_and_missing_required_properties_fail() {
        let mut p = base();
        p[0] = prop(
            "AstrometricSolution:Version",
            "String",
            PropertyValue::Str("2.0".into()),
        );
        assert!(parse(&p).unwrap_err().contains("major"));
        let mut p = base();
        p[0] = prop(
            "AstrometricSolution:Version",
            "String",
            PropertyValue::Str("1.3".into()),
        );
        assert!(parse(&p).is_ok(), "minor revisions are additive");
        let mut p = base();
        p.retain(|x| x.id != "AstrometricSolution:LinearTransformationMatrix");
        assert!(
            parse(&p)
                .unwrap_err()
                .contains("LinearTransformationMatrix")
        );
        let mut p = base();
        p[1] = prop(
            "AstrometricSolution:ProjectionSystem",
            "String",
            PropertyValue::Str("Bonne".into()),
        );
        assert!(
            parse(&p).is_err(),
            "unknown projection makes the whole solution unavailable"
        );
        // A known projection other than Gnomonic: refused, since
        // pixel_to_sky implements only the gnomonic deprojection.
        p[1] = prop(
            "AstrometricSolution:ProjectionSystem",
            "String",
            PropertyValue::Str("Stereographic".into()),
        );
        assert!(parse(&p).unwrap_err().contains("not supported"));
    }

    #[test]
    fn non_standard_native_frame_is_refused() {
        let mut p = base();
        p.push(prop(
            "AstrometricSolution:ReferenceNativeCoordinates",
            "F64Vector",
            PropertyValue::F64Vec(vec![0.0, 45.0]),
        ));
        assert!(parse(&p).unwrap_err().contains("native frame"));
    }

    #[test]
    fn layer2_requires_both_matrices_and_applies_a_homography() {
        let mut p = base();
        layer2(
            &mut p,
            affine_h([[-S, 0.0], [0.0, S]], [2449.0 * S, -1615.0 * S]),
            affine_h([[-1.0 / S, 0.0], [0.0, 1.0 / S]], [2449.0, 1615.0]),
        );
        let sol = parse(&p).unwrap();
        let d = sol.projective.as_ref().unwrap();
        let (xi, eta) = d.image_to_projection.apply(2449.0, 1615.0);
        assert!(xi.abs() < 1e-12 && eta.abs() < 1e-12);
        let inv = d.projection_to_image.apply(xi, eta);
        assert!((inv.0 - 2449.0).abs() < 1e-9 && (inv.1 - 1615.0).abs() < 1e-9);
        // Only one matrix ⇒ layer unavailable, solution still valid (layer 1), with a note.
        let mut p = base();
        exact_layer2(&mut p);
        p.retain(|x| x.id != "AstrometricSolution:ProjectiveTransformation:ProjectionToImage");
        let sol = parse(&p).unwrap();
        assert!(sol.projective.is_none() && !sol.notes.is_empty());
    }

    #[test]
    fn homography_divides_by_w() {
        let h = Homography([[2.0, 0.0, 1.0], [0.0, 3.0, 0.0], [0.5, 0.0, 1.0]]);
        let (x, y) = h.apply(2.0, 1.0);
        assert!((x - 5.0 / 2.0).abs() < 1e-12 && (y - 3.0 / 2.0).abs() < 1e-12);
    }

    #[test]
    fn layer3_global_round_trips_through_properties() {
        let nodes = lattice(6, 4000.0, 3000.0);
        let v = vector_from(&nodes, |x, y| {
            (
                1e-4 * (x - 2000.0) + 2e-9 * (x - 2000.0) * (y - 1500.0),
                1e-4 * (y - 1500.0),
            )
        });
        let mut p = base();
        exact_layer2(&mut p);
        layer3_global(&mut p, "ImageToProjection", &v);
        layer3_global(&mut p, "ProjectionToImage", &v);
        assert!(!p.iter().any(|x| x.id.ends_with("Global:Y:Nodes")));
        let sol = parse(&p).unwrap();
        assert!(sol.notes.is_empty(), "{:?}", sol.notes);
        let (m, _) = rbf(&sol);
        for (x, y) in [(0.0, 0.0), (1234.5, 678.9), (4000.0, 3000.0)] {
            let a = m.residual(x, y);
            let b = v.eval(x, y);
            assert!((a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9);
        }
        // Y with its own nodes (different lattice) also round-trips.
        let nodes_y = lattice(5, 4000.0, 3000.0);
        let vy = VectorSpline {
            x: v.x.clone(),
            y: vector_from(&nodes_y, |x, y| (0.0, 3e-4 * y + 1e-9 * x * y)).y,
        };
        let mut p = base();
        exact_layer2(&mut p);
        layer3_global(&mut p, "ImageToProjection", &vy);
        layer3_global(&mut p, "ProjectionToImage", &vy);
        assert!(p.iter().any(|x| x.id.ends_with("Global:Y:Nodes")));
        let sol = parse(&p).unwrap();
        let (m, _) = rbf(&sol);
        let a = m.residual(777.0, 999.0);
        let b = vy.eval(777.0, 999.0);
        assert!((a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9);
    }

    #[test]
    fn layer3_local_and_fallback_round_trip_with_packed_offsets() {
        let n1 = lattice(4, 300.0, 300.0);
        let n2: Vec<[f64; 2]> = lattice(5, 300.0, 300.0)
            .iter()
            .map(|p| [p[0] + 200.0, p[1] + 100.0])
            .collect();
        let t1 = LocalTerm {
            center: [150.0, 150.0],
            radius: 260.0,
            spline: vector_from(&n1, |x, y| (1e-3 * x, -2e-3 * y)),
        };
        let t2 = LocalTerm {
            center: [350.0, 250.0],
            radius: 260.0,
            spline: vector_from(&n2, |x, y| (5e-4 * y, 7e-4 * x)),
        };
        let fb = vector_from(&lattice(4, 600.0, 400.0), |x, y| (1e-5 * x, 1e-5 * y));
        let mut p = base();
        exact_layer2(&mut p);
        layer3_local(
            &mut p,
            "ImageToProjection",
            &[t1.clone(), t2.clone()],
            Some((0.15, &fb)),
            None,
        );
        layer3_local(
            &mut p,
            "ProjectionToImage",
            &[t1.clone(), t2.clone()],
            Some((0.15, &fb)),
            None,
        );
        assert!(
            p.iter()
                .any(|x| x.id.ends_with("Terms") && x.value.as_str() == Some("Local\nFallback"))
        );
        let sol = parse(&p).unwrap();
        assert!(sol.notes.is_empty(), "{:?}", sol.notes);
        let (m, _) = rbf(&sol);
        let expect = DistortionModel::new(None, vec![t1, t2], Some((0.15, fb))).unwrap();
        for (x, y) in [
            (150.0, 150.0),
            (350.0, 250.0),
            (250.0, 200.0),
            (900.0, 900.0),
        ] {
            let a = m.residual(x, y);
            let b = expect.residual(x, y);
            assert!(
                (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9,
                "at ({x},{y})"
            );
        }
    }

    #[test]
    fn unknown_identifiers_and_inconsistent_dimensions_drop_layer3_only() {
        let nodes = lattice(4, 400.0, 300.0);
        let v = vector_from(&nodes, |x, y| (1e-4 * x, 1e-4 * y));
        let build = || {
            let mut p = base();
            exact_layer2(&mut p);
            layer3_global(&mut p, "ImageToProjection", &v);
            layer3_global(&mut p, "ProjectionToImage", &v);
            p
        };
        let mut p = build();
        let i = p
            .iter()
            .position(|x| x.id.ends_with("ImageToProjection:BasisFunction"))
            .unwrap();
        p[i] = prop(
            &p[i].id.clone(),
            "String",
            PropertyValue::Str("Wendland".into()),
        );
        let sol = parse(&p).unwrap();
        assert!(sol.distortion.is_none() && sol.projective.is_some());
        assert!(sol.notes.iter().any(|n| n.contains("Wendland")));
        let mut p = build();
        let i = p
            .iter()
            .position(|x| x.id.ends_with("ProjectionToImage:Terms"))
            .unwrap();
        p[i] = prop(
            &p[i].id.clone(),
            "String",
            PropertyValue::Str("Global\nQuadtree".into()),
        );
        assert!(parse(&p).unwrap().distortion.is_none());
        let mut p = build();
        let i = p
            .iter()
            .position(|x| x.id.ends_with("ImageToProjection:Global:X:Coefficients"))
            .unwrap();
        if let PropertyValue::F64Vec(c) = &mut p[i].value {
            c.pop();
        }
        let sol = parse(&p).unwrap();
        assert!(sol.distortion.is_none(), "both directions drop together");
    }

    #[test]
    fn layer3_without_layer2_is_unavailable() {
        let nodes = lattice(4, 400.0, 300.0);
        let v = vector_from(&nodes, |x, y| (1e-4 * x, 1e-4 * y));
        let mut p = base();
        layer3_global(&mut p, "ImageToProjection", &v);
        layer3_global(&mut p, "ProjectionToImage", &v);
        let sol = parse(&p).unwrap();
        assert!(sol.projective.is_none() && sol.distortion.is_none());
        assert!(!sol.notes.is_empty());
    }

    /// A smooth distortion on top of the exact linear map, with an inverse
    /// residual so that P2I(I2P(p)) ≈ p to first order (mmm's sampling
    /// fixture, evaluated directly here instead of through sampled grids).
    fn distorted(rx: f64, ry: f64) -> Vec<XisfProperty> {
        let nodes = lattice(6, 640.0, 480.0);
        let dist = |x: f64, y: f64| (2e-8 * (x - rx) * (y - ry), -1.5e-10 * (x - rx) * (x - rx));
        let v = vector_from(&nodes, dist);
        let nodes_p: Vec<[f64; 2]> = nodes
            .iter()
            .map(|p| {
                let (dx, dy) = dist(p[0], p[1]);
                [-S * (p[0] - rx) + dx, S * (p[1] - ry) + dy]
            })
            .collect();
        let inv = vector_from(&nodes_p, |xi, eta| {
            let x = rx - xi / S;
            let y = ry + eta / S;
            let (dx, dy) = dist(x, y);
            (dx / S, -dy / S)
        });
        let mut p = layer1([84.2, -3.24], [rx, ry], [[-S, 0.0], [0.0, S]]);
        layer2(
            &mut p,
            affine_h([[-S, 0.0], [0.0, S]], [rx * S, -ry * S]),
            affine_h([[-1.0 / S, 0.0], [0.0, 1.0 / S]], [rx, ry]),
        );
        layer3_global(&mut p, "ImageToProjection", &v);
        layer3_global(&mut p, "ProjectionToImage", &inv);
        p
    }

    #[test]
    fn the_model_adds_the_residual_to_layer2_and_passes_validation() {
        let (rx, ry) = (320.0, 240.0);
        let p = distorted(rx, ry);
        let sol = decode(&p, Some((640, 480))).unwrap();
        assert!(sol.notes.is_empty(), "{:?}", sol.notes);
        let (m, _) = rbf(&sol);
        let h = &sol.projective.as_ref().unwrap().image_to_projection;
        for (x, y) in [(3.0, 5.0), (321.7, 239.1), (600.0, 470.0)] {
            let (px, py) = h.apply(x, y);
            let (dx, dy) = m.residual(x, y);
            let got = sol.image_to_projection(x, y);
            assert!((got.0 - (px + dx)).abs() < 1e-15 && (got.1 - (py + dy)).abs() < 1e-15);
            // The residual is the fixture's distortion, to the accuracy of a
            // 6×6-node fit of a field the spline cannot represent exactly
            // (within 3% in the corner cells).
            let want = (2e-8 * (x - rx) * (y - ry), -1.5e-10 * (x - rx) * (x - rx));
            let tol = |w: f64| 0.05 * w.abs() + 1e-6;
            assert!(
                (dx - want.0).abs() < tol(want.0) && (dy - want.1).abs() < tol(want.1),
                "({x},{y}): residual ({dx},{dy}) vs {want:?}"
            );
        }
        // The reference pixel maps to the reference point, to within the
        // fitted residual there (4e-8 deg).
        let (ra, dec) = sol.pixel_to_sky(rx, ry);
        assert!((ra - 84.2).abs() < 1e-6 && (dec - -3.24).abs() < 1e-6);
    }

    #[test]
    fn layer1_only_validates_trivially() {
        assert!(decode(&base(), Some((100, 100))).is_ok());
    }

    #[test]
    fn validation_refuses_a_projective_layer_inconsistent_with_the_linear_layer() {
        // Projective layer rotated 90° relative to the linear matrix.
        let mut p = base();
        layer2(
            &mut p,
            affine_h([[0.0, S], [-S, 0.0]], [0.0, 0.0]),
            affine_h([[0.0, -1.0 / S], [1.0 / S, 0.0]], [0.0, 0.0]),
        );
        assert!(decode(&p, Some((4000, 3000))).is_err());
    }

    #[test]
    fn control_points_and_provenance_are_read() {
        let mut p = base();
        let m = |id: &str, data: Vec<f64>| {
            prop(
                id,
                "F64Matrix",
                PropertyValue::F64Mat {
                    rows: (data.len() / 2) as u32,
                    cols: 2,
                    data,
                },
            )
        };
        p.push(m(
            "AstrometricSolution:ControlPoints:Image",
            vec![1.0, 2.0, 3.0, 4.0],
        ));
        p.push(m(
            "AstrometricSolution:ControlPoints:Celestial",
            vec![84.0, -3.0, 84.1, -3.1],
        ));
        for (id, v) in [
            ("Catalog", "Gaia DR3"),
            ("CreatorApplication", "PixInsight 1.9.5"),
            ("CreatorModule", "ImageSolver 6.5.0"),
            ("CreatorOS", "Linux"),
            ("CreationTime", "2026-10-07T12:01:25.527Z"),
        ] {
            p.push(prop(
                &format!("AstrometricSolution:{id}"),
                "String",
                PropertyValue::Str(v.into()),
            ));
        }
        let sol = parse(&p).unwrap();
        let cp = sol.control_points.unwrap();
        assert_eq!(cp.image, vec![[1.0, 2.0], [3.0, 4.0]]);
        assert_eq!(cp.celestial, vec![[84.0, -3.0], [84.1, -3.1]]);
        assert_eq!(sol.catalog.as_deref(), Some("Gaia DR3"));
        assert_eq!(sol.creator_module.as_deref(), Some("ImageSolver 6.5.0"));
        assert_eq!(
            sol.creation_time.as_deref(),
            Some("2026-10-07T12:01:25.527Z")
        );
        // Mismatched lengths drop the control points with a note.
        p.retain(|x| x.id != "AstrometricSolution:ControlPoints:Celestial");
        p.push(m(
            "AstrometricSolution:ControlPoints:Celestial",
            vec![84.0, -3.0],
        ));
        let sol = parse(&p).unwrap();
        assert!(sol.control_points.is_none());
        assert!(sol.notes.iter().any(|n| n.contains("control points")));
    }
}
