use std::collections::HashMap;
use zpdf_core::{ObjectId, PdfObject, Result};

/// Caching object resolver. Wraps PdfFile to add resolved object cache.
pub struct Resolver {
    cache: HashMap<ObjectId, PdfObject>,
}

impl Resolver {
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
        }
    }
}

impl Default for Resolver {
    fn default() -> Self {
        Self::new()
    }
}
