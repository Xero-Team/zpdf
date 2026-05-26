use std::collections::HashMap;

pub type ImageId = u32;

#[derive(Debug)]
pub struct ImageCache {
    images: HashMap<ImageId, DecodedImage>,
    next_id: ImageId,
}

#[derive(Debug, Clone)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub has_alpha: bool,
    pub premultiplied: bool,
}

impl ImageCache {
    pub fn new() -> Self {
        Self {
            images: HashMap::new(),
            next_id: 0,
        }
    }

    pub fn get(&self, id: ImageId) -> Option<&DecodedImage> {
        self.images.get(&id)
    }

    pub fn insert(&mut self, image: DecodedImage) -> ImageId {
        let id = self.next_id;
        self.next_id += 1;
        self.images.insert(id, image);
        id
    }
}

impl Default for ImageCache {
    fn default() -> Self {
        Self::new()
    }
}
