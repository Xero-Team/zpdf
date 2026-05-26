use std::collections::HashMap;

use zpdf_core::{Error, ObjectId, PdfName, PdfObject, Rect, Result};
use zpdf_parser::PdfFile;

#[derive(Debug)]
pub struct PdfPage {
    pub id: ObjectId,
    pub media_box: Rect,
    pub crop_box: Rect,
    pub rotate: i32,
    pub resources: ResourceDict,
    pub contents: Vec<ObjectId>,
}

#[derive(Debug, Default)]
pub struct ResourceDict {
    pub fonts: HashMap<String, ObjectId>,
    pub xobjects: HashMap<String, ObjectId>,
    pub ext_g_state: HashMap<String, ObjectId>,
    pub color_spaces: HashMap<String, ObjectId>,
    pub patterns: HashMap<String, ObjectId>,
}

impl PdfPage {
    pub fn from_object(file: &PdfFile, page_id: ObjectId) -> Result<Self> {
        let obj = file.resolve(page_id)?;
        let dict = obj.as_dict()?;

        let media_box = Self::inherit_rect(file, dict, "MediaBox")?
            .ok_or_else(|| zpdf_core::Error::MissingKey("MediaBox".into()))?;
        let crop_box = Self::inherit_rect(file, dict, "CropBox")?
            .unwrap_or(media_box);

        let rotate = dict
            .get_i64("Rotate")
            .unwrap_or(0) as i32;

        let contents = match dict.get("Contents") {
            Some(PdfObject::Ref(r)) => vec![*r],
            Some(PdfObject::Array(arr)) => arr
                .iter()
                .filter_map(|o| match o {
                    PdfObject::Ref(r) => Some(*r),
                    _ => None,
                })
                .collect(),
            _ => vec![],
        };

        let resources = match dict.get("Resources") {
            Some(PdfObject::Dict(d)) => parse_resource_dict(d)?,
            Some(PdfObject::Ref(r)) => {
                let res_obj = file.resolve(*r)?;
                let res_dict = res_obj.as_dict()?;
                parse_resource_dict(res_dict)?
            }
            _ => ResourceDict::default(),
        };

        Ok(Self {
            id: page_id,
            media_box,
            crop_box,
            rotate,
            resources,
            contents,
        })
    }

    fn inherit_rect(
        file: &PdfFile,
        dict: &zpdf_core::PdfDict,
        key: &str,
    ) -> Result<Option<Rect>> {
        if let Ok(r) = dict.get_rect(key) {
            return Ok(Some(r));
        }
        // Walk up the Parent chain
        if let Ok(parent_ref) = dict.get_ref("Parent") {
            let parent_obj = file.resolve(parent_ref)?;
            if let Ok(parent_dict) = parent_obj.as_dict() {
                return Self::inherit_rect(file, parent_dict, key);
            }
        }
        Ok(None)
    }

    pub fn width(&self) -> f64 {
        self.media_box.width()
    }

    pub fn height(&self) -> f64 {
        self.media_box.height()
    }
}

fn parse_resource_dict(dict: &zpdf_core::PdfDict) -> Result<ResourceDict> {
    let mut res = ResourceDict::default();

    if let Ok(fonts) = dict.get_dict("Font") {
        for (name, obj) in &fonts.0 {
            if let PdfObject::Ref(r) = obj {
                res.fonts.insert(name.0.clone(), *r);
            }
        }
    }

    if let Ok(xobjects) = dict.get_dict("XObject") {
        for (name, obj) in &xobjects.0 {
            if let PdfObject::Ref(r) = obj {
                res.xobjects.insert(name.0.clone(), *r);
            }
        }
    }

    if let Ok(gs) = dict.get_dict("ExtGState") {
        for (name, obj) in &gs.0 {
            if let PdfObject::Ref(r) = obj {
                res.ext_g_state.insert(name.0.clone(), *r);
            }
        }
    }

    if let Ok(cs) = dict.get_dict("ColorSpace") {
        for (name, obj) in &cs.0 {
            if let PdfObject::Ref(r) = obj {
                res.color_spaces.insert(name.0.clone(), *r);
            }
        }
    }

    if let Ok(pat) = dict.get_dict("Pattern") {
        for (name, obj) in &pat.0 {
            if let PdfObject::Ref(r) = obj {
                res.patterns.insert(name.0.clone(), *r);
            }
        }
    }

    Ok(res)
}
