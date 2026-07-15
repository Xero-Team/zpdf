//! Optional-content (layer) configuration: the catalog's `/OCProperties`
//! default configuration determines which optional-content groups render.
//! Membership evaluation for `/OC` entries (OCG refs, OCMDs and visibility
//! expressions) lives in zpdf-content, which has the object graph in hand;
//! this module only answers "is group X on?".

use std::collections::HashSet;

use zpdf_core::{ObjectId, PdfObject};
use zpdf_parser::PdfFile;

const MAX_OC_GROUPS_PER_LIST: usize = 65_536;

/// The document's default optional-content configuration (`/OCProperties /D`).
#[derive(Debug, Clone, Default)]
pub struct OcConfig {
    /// Groups explicitly turned off.
    off: HashSet<ObjectId>,
    /// Groups explicitly turned on (overrides /BaseState /OFF).
    on: HashSet<ObjectId>,
    /// /BaseState /OFF: groups default to hidden unless listed in /ON.
    base_state_off: bool,
}

impl OcConfig {
    /// Visibility of a single optional-content group. Per 8.11.4.3 the
    /// config applies in order BaseState → /ON → /OFF, so OFF wins when a
    /// group is listed in both arrays.
    pub fn group_visible(&self, id: ObjectId) -> bool {
        if self.off.contains(&id) {
            return false;
        }
        if self.on.contains(&id) {
            return true;
        }
        !self.base_state_off
    }

    /// True when every group renders (no config means everything visible).
    pub fn all_visible(&self) -> bool {
        self.off.is_empty() && !self.base_state_off
    }
}

/// Parse `/OCProperties` from the document catalog. Returns `None` when the
/// document declares no optional content.
pub fn parse_oc_config(file: &PdfFile) -> Option<OcConfig> {
    let root_ref = file.trailer.get_ref("Root").ok()?;
    let root = file.resolve(root_ref).ok()?;
    let root_dict = root.as_dict().ok()?;

    let ocp = resolve_dict(file, root_dict.get("OCProperties")?)?;
    let d = resolve_dict(file, ocp.get("D")?).unwrap_or_default();

    let mut config = OcConfig {
        base_state_off: matches!(d.get_name("BaseState"), Ok("OFF")),
        ..Default::default()
    };
    for id in ref_array(file, d.get("OFF")) {
        config.off.insert(id);
    }
    for id in ref_array(file, d.get("ON")) {
        config.on.insert(id);
    }
    Some(config)
}

fn resolve_dict(file: &PdfFile, obj: &PdfObject) -> Option<zpdf_core::PdfDict> {
    match obj {
        PdfObject::Dict(d) => Some(d.clone()),
        PdfObject::Ref(r) => match file.resolve(*r).ok()? {
            PdfObject::Dict(d) => Some(d),
            _ => None,
        },
        _ => None,
    }
}

fn ref_array(file: &PdfFile, obj: Option<&PdfObject>) -> Vec<ObjectId> {
    let arr: std::borrow::Cow<'_, [PdfObject]> = match obj {
        Some(PdfObject::Array(a)) => std::borrow::Cow::Borrowed(a),
        Some(PdfObject::Ref(r)) => match file.resolve(*r) {
            Ok(PdfObject::Array(a)) => std::borrow::Cow::Owned(a),
            _ => return Vec::new(),
        },
        _ => return Vec::new(),
    };
    arr.iter()
        .filter_map(|o| match o {
            PdfObject::Ref(r) => Some(*r),
            _ => None,
        })
        .take(MAX_OC_GROUPS_PER_LIST)
        .collect()
}
