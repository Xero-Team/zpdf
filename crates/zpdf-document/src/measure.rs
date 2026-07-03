//! Geospatial measure dictionaries (ISO 32000-1 §13.2, Table 261-265).
//!
//! A measure dictionary describes a coordinate system, measurement units, and
//! geographic bounds for annotations that represent real-world locations —
//! chiefly used with PDF 2.0 Projection annotations for mapping and GIS
//! applications. This module parses these dictionaries into a read-only data
//! model; it does not perform coordinate transformations or rendering.
//!
//! The measure info is exposed through [`Annotation::measure`] and can be
//! displayed via `zpdf info` / `zpdf links` CLI commands.

use std::borrow::Cow;
use zpdf_core::{PdfDict, PdfObject};
use zpdf_parser::PdfFile;

/// A measure dictionary describing geospatial coordinate systems and units
/// for an annotation (PDF §13.2, Table 261).
#[derive(Debug, Clone)]
pub struct Measure {
    /// `/Subtype` - the measurement type (e.g., `GEO` for geographic).
    pub subtype: String,
    /// `/Bounds` - a rectangle in default user space defining the measurement
    /// region (optional, defaults to annotation `/Rect`).
    pub bounds: Option<[f32; 4]>,
    /// `/GPTS` - geospatial points array defining the mapping between PDF
    /// coordinates and real-world coordinates (lat/lon pairs).
    pub gpts: Option<Vec<f32>>,
    /// `/GCS` - the geographic coordinate system dictionary (Table 262).
    pub gcs: Option<GeographicCoordinateSystem>,
    /// `/PDU` - point distance units (e.g., `KM`, `MI`).
    pub pdu: Option<String>,
    /// `/DU` - display units for measurements (e.g., `M`, `FT`).
    pub du: Option<String>,
    /// `/A` - area units (e.g., `SQKM`, `HA`).
    pub a: Option<String>,
}

/// Geographic coordinate system info (Table 262).
#[derive(Debug, Clone)]
pub struct GeographicCoordinateSystem {
    /// `/Type` - should be `GEOGCS`.
    pub type_: String,
    /// `/EPSG` - EPSG code (e.g., `4326` for WGS 84).
    pub epsg: Option<i64>,
    /// `/WKT` - Well-Known Text coordinate system definition.
    pub wkt: Option<String>,
}

/// Maximum array sizes to prevent adversarial input from consuming unbounded
/// memory (consistent with existing parse limits).
const MAX_GPTS_VALUES: usize = 1024;
const MAX_WKT_BYTES: usize = 32 * 1024; // 32 KiB

/// Parse a `/Measure` dictionary from an annotation, returning `None` if the
/// dictionary is absent, malformed, or exceeds safety limits.
pub fn parse_measure(file: &PdfFile, annot_dict: &PdfDict) -> Option<Measure> {
    let measure_dict: Cow<'_, PdfDict> = match annot_dict.get("Measure")? {
        PdfObject::Dict(d) => Cow::Borrowed(d),
        PdfObject::Ref(r) => match file.resolve(*r).ok()? {
            PdfObject::Dict(d) => Cow::Owned(d),
            _ => return None,
        },
        _ => return None,
    };

    let subtype = measure_dict
        .get_name("Subtype")
        .ok()
        .unwrap_or("Unknown")
        .to_string();

    // Bounds: [x1 y1 x2 y2] rectangle in default user space.
    let bounds = measure_dict
        .get("Bounds")
        .and_then(|b| resolve_number_array(file, b, 4, 4))
        .and_then(|v| {
            if v.len() == 4 {
                Some([v[0], v[1], v[2], v[3]])
            } else {
                None
            }
        });

    // GPTS: array of geospatial points (latitude, longitude pairs).
    let gpts = measure_dict
        .get("GPTS")
        .and_then(|g| resolve_number_array(file, g, 4, MAX_GPTS_VALUES));

    // GCS: geographic coordinate system.
    let gcs = measure_dict.get("GCS").and_then(|g| parse_gcs(file, g));

    // Units: PDU (point distance), DU (display), A (area).
    let pdu = measure_dict.get_name("PDU").ok().map(|s| s.to_string());
    let du = measure_dict.get_name("DU").ok().map(|s| s.to_string());
    let a = measure_dict.get_name("A").ok().map(|s| s.to_string());

    Some(Measure {
        subtype,
        bounds,
        gpts,
        gcs,
        pdu,
        du,
        a,
    })
}

fn parse_gcs(file: &PdfFile, obj: &PdfObject) -> Option<GeographicCoordinateSystem> {
    let dict: Cow<'_, PdfDict> = match obj {
        PdfObject::Dict(d) => Cow::Borrowed(d),
        PdfObject::Ref(r) => match file.resolve(*r).ok()? {
            PdfObject::Dict(d) => Cow::Owned(d),
            _ => return None,
        },
        _ => return None,
    };

    let type_ = dict.get_name("Type").ok().unwrap_or("Unknown").to_string();

    // EPSG code (integer).
    let epsg = dict.get("EPSG").and_then(|e| match e {
        PdfObject::Integer(n) => Some(*n),
        PdfObject::Ref(r) => match file.resolve(*r).ok()? {
            PdfObject::Integer(n) => Some(n),
            _ => None,
        },
        _ => None,
    });

    // WKT string (can be large, apply limit).
    let wkt = dict.get("WKT").and_then(|w| {
        let bytes: Vec<u8> = match w {
            PdfObject::String(s) => s.as_bytes().to_vec(),
            PdfObject::Ref(r) => match file.resolve(*r).ok()? {
                PdfObject::String(s) => s.as_bytes().to_vec(),
                _ => return None,
            },
            _ => return None,
        };
        if bytes.len() > MAX_WKT_BYTES {
            return None;
        }
        String::from_utf8(bytes).ok()
    });

    Some(GeographicCoordinateSystem { type_, epsg, wkt })
}

/// Resolve a numeric array (direct or indirect), returning `None` if the array
/// is malformed, contains non-numeric values, or exceeds `max_len`.
fn resolve_number_array(
    file: &PdfFile,
    obj: &PdfObject,
    min_len: usize,
    max_len: usize,
) -> Option<Vec<f32>> {
    let arr: Cow<'_, [PdfObject]> = match obj {
        PdfObject::Array(a) => Cow::Borrowed(a.as_slice()),
        PdfObject::Ref(r) => match file.resolve(*r).ok()? {
            PdfObject::Array(a) => Cow::Owned(a),
            _ => return None,
        },
        _ => return None,
    };

    if arr.len() < min_len || arr.len() > max_len {
        return None;
    }

    let mut nums = Vec::with_capacity(arr.len());
    for elem in arr.iter() {
        let n = match elem {
            PdfObject::Integer(i) => *i as f32,
            PdfObject::Real(f) => *f as f32,
            PdfObject::Ref(r) => match file.resolve(*r).ok()? {
                PdfObject::Integer(i) => i as f32,
                PdfObject::Real(f) => f as f32,
                _ => return None,
            },
            _ => return None,
        };
        if !n.is_finite() {
            return None;
        }
        nums.push(n);
    }

    Some(nums)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zpdf_core::ObjectId;
    use zpdf_parser::PdfFile;

    fn measure_of(measure_str: &str) -> Option<Measure> {
        let pdf = format!(
            "%PDF-1.7\n1 0 obj\n<< /Type /Annot /Subtype /Projection \
             /Rect [0 0 100 100] /Measure {} >>\nendobj\n\
             xref\n0 2\n0000000000 65535 f\n0000000009 00000 n\ntrailer\n\
             << /Size 2 /Root << >> >>\nstartxref\n0\n%%EOF",
            measure_str
        );
        let file = PdfFile::parse(pdf.as_bytes()).ok()?;
        let obj = file.resolve(ObjectId(1, 0)).ok()?;
        let annot_dict = obj.as_dict().ok()?;
        parse_measure(&file, annot_dict)
    }

    #[test]
    fn parses_geo_measure_with_epsg() {
        let m = measure_of(
            "<< /Subtype /GEO /GPTS [0.0 0.0 100.0 0.0 100.0 100.0 0.0 100.0] \
             /GCS << /Type /GEOGCS /EPSG 4326 >> /PDU /KM /DU /M >>",
        )
        .expect("measure");

        assert_eq!(m.subtype, "GEO");
        assert_eq!(m.gpts.as_ref().unwrap().len(), 8);
        assert_eq!(m.pdu.as_deref(), Some("KM"));
        assert_eq!(m.du.as_deref(), Some("M"));

        let gcs = m.gcs.as_ref().expect("GCS");
        assert_eq!(gcs.type_, "GEOGCS");
        assert_eq!(gcs.epsg, Some(4326));
    }

    #[test]
    fn parses_bounds() {
        let m = measure_of(
            "<< /Subtype /GEO /Bounds [10.0 20.0 90.0 80.0] \
             /GPTS [0.0 0.0 100.0 100.0] >>",
        )
        .expect("measure");

        assert_eq!(m.bounds, Some([10.0, 20.0, 90.0, 80.0]));
    }

    #[test]
    fn rejects_oversized_gpts() {
        // MAX_GPTS_VALUES is 1024; test that we reject arrays beyond that limit.
        // Use a smaller test case that the PDF parser can handle.
        let large_gpts = (0..1025)
            .map(|i| format!("{}.0", i))
            .collect::<Vec<_>>()
            .join(" ");
        let m = measure_of(&format!("<< /Subtype /GEO /GPTS [{}] >>", large_gpts));
        // If the parser accepts it, check that our measure parser rejects it.
        match m {
            None => {} // Good - rejected
            Some(measure) => {
                // If it parsed, GPTS should be None due to oversized array rejection.
                assert!(
                    measure.gpts.is_none(),
                    "GPTS should be None when array exceeds MAX_GPTS_VALUES, got: {:?}",
                    measure.gpts.as_ref().map(|v| v.len())
                );
            }
        }
    }

    #[test]
    fn handles_missing_measure() {
        let pdf = "%PDF-1.7\n1 0 obj\n<< /Type /Annot /Subtype /Square /Rect [0 0 100 100] >>\nendobj\n\
                   xref\n0 2\n0000000000 65535 f\n0000000009 00000 n\ntrailer\n<< /Size 2 /Root << >> >>\n\
                   startxref\n0\n%%EOF";
        let file = PdfFile::parse(pdf.as_bytes()).expect("parse");
        let obj = file.resolve(ObjectId(1, 0)).ok().unwrap();
        let annot_dict = obj.as_dict().ok().unwrap();
        let m = parse_measure(&file, annot_dict);
        assert!(m.is_none(), "no measure dict should return None");
    }
}
