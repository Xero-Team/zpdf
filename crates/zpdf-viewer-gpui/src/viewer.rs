use std::collections::HashMap;
use std::fs::File;
use std::sync::Arc;

use gpui::{
    div, img, prelude::*, px, rgb, App, Context, FocusHandle, Focusable, FontWeight,
    InteractiveElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement,
    RenderImage, StatefulInteractiveElement, Styled, StyledImage, Window,
};
use zpdf::gpu::GpuContext;
use zpdf::{ObjectId, PdfObject, Rect};
use zpdf_document::{InkAnnotDict, InkAnnotationBuilder};
use zpdf_writer::IncrementalWriter;

use crate::actions::{
    ActualSize, AddConfidentialStamp, AddDraftStamp, AddWatermark, CancelAnnotation,
    DeleteSelected, FirstPage, FitWidth, LastPage, NextPage, PreviousPage, SaveAnnotation,
    SaveEdits, ToggleInkMode, ToggleSelectMode, ZoomIn, ZoomOut,
};
use crate::document::{LoadedDocument, PagePreview, PageSummary};

const PREVIEW_DPI: f32 = 144.0;
const MIN_ZOOM: f32 = 0.35;
const MAX_ZOOM: f32 = 3.5;
const ZOOM_STEP: f32 = 1.2;
const SIDEBAR_WIDTH: f32 = 264.0;
const PAGE_VIEWPORT_CHROME: f32 = 220.0;
type ButtonPalette = (u32, u32, u32);

#[derive(Debug, Clone, Copy, PartialEq)]
enum AnnotationMode {
    None,
    Ink,
    Select,
}

#[derive(Debug, Clone)]
struct EditableAnnotation {
    object_id: ObjectId,
    #[allow(dead_code)]
    page_index: usize,
    #[allow(dead_code)]
    subtype: String,
    #[allow(dead_code)]
    original_rect: Rect,
    current_rect: Rect,
}

#[derive(Debug, Clone)]
enum EditOperation {
    Move {
        object_id: ObjectId,
        #[allow(dead_code)]
        page_index: usize,
        new_rect: Rect,
    },
    Delete {
        object_id: ObjectId,
        page_index: usize,
    },
}

#[derive(Debug, Default)]
struct EditBuffer {
    operations: HashMap<ObjectId, EditOperation>,
}

impl EditBuffer {
    fn add(&mut self, op: EditOperation) {
        let id = match &op {
            EditOperation::Move { object_id, .. } => *object_id,
            EditOperation::Delete { object_id, .. } => *object_id,
        };
        self.operations.insert(id, op);
    }

    fn clear(&mut self) {
        self.operations.clear();
    }

    fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }
}

#[derive(Debug, Clone)]
enum DragState {
    MovingAnnotation {
        annotation_id: ObjectId,
        start_screen_pos: (f32, f32),
        start_pdf_rect: Rect,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
enum ResizeHandle {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    Top,
    Bottom,
    Left,
    Right,
}

enum HitTestResult {
    None,
    AnnotationBody { annotation_id: ObjectId },
}

pub struct Viewer {
    document: LoadedDocument,
    focus_handle: FocusHandle,
    current_page: usize,
    zoom: f32,
    fit_width: bool,
    page_cache: HashMap<usize, PagePreview>,
    gpu_context: Option<GpuContext>,
    last_error: Option<String>,

    // Ink annotation state
    annotation_mode: AnnotationMode,
    current_stroke: Vec<(f32, f32)>,
    ink_strokes: Vec<Vec<(f32, f32)>>,
    ink_color: (f64, f64, f64),
    ink_width: f64,
    is_drawing: bool,

    // Selection mode state
    page_annotations: Vec<EditableAnnotation>,
    selected_annotation: Option<ObjectId>,
    edit_buffer: EditBuffer,
    drag_state: Option<DragState>,

    // Current rendered image bounds (for coordinate conversion)
    current_image_width: f32,
    current_image_height: f32,
}

impl Viewer {
    pub fn new(document: LoadedDocument, cx: &mut Context<Self>) -> Self {
        Self {
            document,
            focus_handle: cx.focus_handle(),
            current_page: 0,
            zoom: 1.0,
            fit_width: true,
            page_cache: HashMap::new(),
            gpu_context: None,
            last_error: None,
            annotation_mode: AnnotationMode::None,
            current_stroke: Vec::new(),
            ink_strokes: Vec::new(),
            ink_color: (0.0, 0.0, 0.0), // black
            ink_width: 2.0,
            is_drawing: false,
            page_annotations: Vec::new(),
            selected_annotation: None,
            edit_buffer: EditBuffer::default(),
            drag_state: None,
            current_image_width: 0.0,
            current_image_height: 0.0,
        }
    }

    pub fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn document_name(&self) -> String {
        self.document
            .summary
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Untitled PDF")
            .to_string()
    }

    fn document_path_label(&self) -> String {
        self.document.summary.path.display().to_string()
    }

    fn version_label(&self) -> String {
        format!(
            "PDF-{}.{}",
            self.document.summary.version.0, self.document.summary.version.1
        )
    }

    fn set_page(&mut self, page: usize, cx: &mut Context<Self>) {
        let clamped = page.min(self.document.summary.page_count.saturating_sub(1));
        if clamped != self.current_page {
            self.current_page = clamped;
            self.last_error = None;
            cx.notify();
        }
    }

    fn zoom_to(&mut self, zoom: f32, cx: &mut Context<Self>) {
        self.zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        self.fit_width = false;
        cx.notify();
    }

    fn next_page(&mut self, _: &NextPage, _: &mut Window, cx: &mut Context<Self>) {
        self.set_page(self.current_page + 1, cx);
    }

    fn previous_page(&mut self, _: &PreviousPage, _: &mut Window, cx: &mut Context<Self>) {
        self.set_page(self.current_page.saturating_sub(1), cx);
    }

    fn first_page(&mut self, _: &FirstPage, _: &mut Window, cx: &mut Context<Self>) {
        self.set_page(0, cx);
    }

    fn last_page(&mut self, _: &LastPage, _: &mut Window, cx: &mut Context<Self>) {
        self.set_page(self.document.summary.page_count.saturating_sub(1), cx);
    }

    fn zoom_in(&mut self, _: &ZoomIn, window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_to(self.effective_zoom(window) * ZOOM_STEP, cx);
    }

    fn zoom_out(&mut self, _: &ZoomOut, window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_to(self.effective_zoom(window) / ZOOM_STEP, cx);
    }

    fn actual_size(&mut self, _: &ActualSize, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom = 1.0;
        self.fit_width = false;
        cx.notify();
    }

    fn fit_width(&mut self, _: &FitWidth, _: &mut Window, cx: &mut Context<Self>) {
        self.fit_width = true;
        cx.notify();
    }

    fn toggle_ink_mode(&mut self, _: &ToggleInkMode, _: &mut Window, cx: &mut Context<Self>) {
        match self.annotation_mode {
            AnnotationMode::None => {
                self.annotation_mode = AnnotationMode::Ink;
                self.current_stroke.clear();
                self.ink_strokes.clear();
            }
            AnnotationMode::Ink => {
                self.annotation_mode = AnnotationMode::None;
                self.current_stroke.clear();
                self.ink_strokes.clear();
            }
            AnnotationMode::Select => {
                self.annotation_mode = AnnotationMode::Ink;
                self.selected_annotation = None;
                self.page_annotations.clear();
                self.current_stroke.clear();
                self.ink_strokes.clear();
            }
        }
        cx.notify();
    }

    fn cancel_annotation(&mut self, _: &CancelAnnotation, _: &mut Window, cx: &mut Context<Self>) {
        self.annotation_mode = AnnotationMode::None;
        self.current_stroke.clear();
        self.ink_strokes.clear();
        cx.notify();
    }

    fn save_annotation(&mut self, _: &SaveAnnotation, window: &mut Window, cx: &mut Context<Self>) {
        tracing::warn!("=== SAVE ANNOTATION CALLED ===");
        tracing::warn!("Number of strokes: {}", self.ink_strokes.len());

        if self.ink_strokes.is_empty() {
            tracing::warn!("No strokes to save!");
            self.last_error = Some("No strokes to save".to_string());
            cx.notify();
            return;
        }

        // Get the current page info
        let page = &self.document.summary.pages[self.current_page];
        let zoom = self.effective_zoom(window);

        tracing::warn!("Page dimensions: {}x{}", page.width, page.height);
        tracing::warn!("Zoom: {}", zoom);

        // Convert screen coordinates to PDF coordinates
        let mut builder = InkAnnotationBuilder::new();
        builder.set_color(self.ink_color.0, self.ink_color.1, self.ink_color.2);
        builder.set_width(self.ink_width);

        for (stroke_idx, stroke) in self.ink_strokes.iter().enumerate() {
            tracing::warn!("Stroke {}: {} points", stroke_idx, stroke.len());
            if let Some(first) = stroke.first() {
                tracing::warn!("  Screen coords - first: ({}, {})", first.0, first.1);
            }

            let pdf_stroke: Vec<(f64, f64)> = stroke
                .iter()
                .map(|&(screen_x, screen_y)| self.screen_to_pdf(screen_x, screen_y, page, zoom))
                .collect();

            if let Some(first) = pdf_stroke.first() {
                tracing::warn!("  PDF coords - first: ({}, {})", first.0, first.1);
            }

            if !pdf_stroke.is_empty() {
                builder.add_stroke(pdf_stroke);
            }
        }

        match builder.build() {
            Some((annot_dict, appearance)) => {
                tracing::warn!(
                    "Built annotation, appearance size: {} bytes",
                    appearance.len()
                );
                // Save to file
                if let Err(e) = self.save_to_file(&annot_dict, &appearance) {
                    tracing::error!("Save failed: {}", e);
                    self.last_error = Some(format!("Failed to save: {}", e));
                } else {
                    tracing::warn!("Save successful!");
                    // Success - exit ink mode and reload
                    self.annotation_mode = AnnotationMode::None;
                    self.current_stroke.clear();
                    self.ink_strokes.clear();
                    self.page_cache.clear();
                    self.last_error = None;
                }
            }
            None => {
                tracing::error!("Failed to build annotation");
                self.last_error = Some("Failed to build annotation".to_string());
            }
        }
        cx.notify();
    }

    fn screen_to_pdf(
        &self,
        screen_x: f32,
        screen_y: f32,
        page: &crate::document::PageSummary,
        zoom: f32,
    ) -> (f64, f64) {
        // Screen coordinates should be relative to the page_container div
        // The image fills the container, so coords should map directly
        // Scale factor: DPI / 72.0 * zoom
        let scale = PREVIEW_DPI / 72.0 * zoom;

        tracing::warn!(
            "  screen_to_pdf: screen=({}, {}), scale={}, page_size=({}, {})",
            screen_x,
            screen_y,
            scale,
            page.width,
            page.height
        );
        tracing::warn!(
            "  image_size=({}, {})",
            self.current_image_width,
            self.current_image_height
        );

        // Check if click is within image bounds
        if screen_x < 0.0
            || screen_x > self.current_image_width
            || screen_y < 0.0
            || screen_y > self.current_image_height
        {
            tracing::warn!("  WARNING: Click outside image bounds!");
        }

        // Convert to PDF coordinates
        // PDF origin is bottom-left, screen origin is top-left
        let pdf_x = (screen_x / scale) as f64;
        let pdf_y = ((page.height as f32) - (screen_y / scale)) as f64;

        tracing::warn!("  -> pdf=({}, {})", pdf_x, pdf_y);

        (pdf_x, pdf_y)
    }

    fn save_to_file(
        &self,
        annot_dict: &InkAnnotDict,
        appearance: &[u8],
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Read the original PDF
        let original_bytes = std::fs::read(&self.document.summary.path)?;

        // Create incremental writer
        let mut writer = IncrementalWriter::new(original_bytes)?;

        // Add the annotation
        writer.add_ink_annotation_to_page(self.current_page, annot_dict, appearance)?;

        // Write to temporary file
        let temp_path = self.document.summary.path.with_extension("pdf.tmp");
        let mut temp_file = File::create(&temp_path)?;
        writer.write(&mut temp_file)?;
        temp_file.sync_all()?;
        drop(temp_file);

        // Atomic rename
        std::fs::rename(&temp_path, &self.document.summary.path)?;

        Ok(())
    }

    fn add_watermark(&mut self, _: &AddWatermark, _: &mut Window, cx: &mut Context<Self>) {
        self.add_stamp("WATERMARK", 48.0, (0.7, 0.7, 0.7), cx);
    }

    fn add_draft_stamp(&mut self, _: &AddDraftStamp, _: &mut Window, cx: &mut Context<Self>) {
        self.add_stamp("DRAFT", 72.0, (1.0, 0.0, 0.0), cx);
    }

    fn add_confidential_stamp(
        &mut self,
        _: &AddConfidentialStamp,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.add_stamp("CONFIDENTIAL", 48.0, (1.0, 0.0, 0.0), cx);
    }

    fn add_stamp(&mut self, text: &str, size: f64, color: (f64, f64, f64), cx: &mut Context<Self>) {
        tracing::info!("Adding stamp '{}' to page {}", text, self.current_page);

        // Read the original PDF
        let original_bytes = match std::fs::read(&self.document.summary.path) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.last_error = Some(format!("Failed to read PDF: {}", e));
                cx.notify();
                return;
            }
        };

        // Create incremental writer
        let mut writer = match IncrementalWriter::new(original_bytes) {
            Ok(w) => w,
            Err(e) => {
                self.last_error = Some(format!("Failed to create writer: {}", e));
                cx.notify();
                return;
            }
        };

        // Get page dimensions
        let page = &self.document.summary.pages[self.current_page];

        // Position stamp in center of page
        let x = (page.width / 2.0) - 100.0; // Rough centering
        let y = page.height / 2.0;

        // Create stamp item
        let stamp = zpdf::StampItem::Text {
            text: text.to_string(),
            x,
            y,
            font: "Helvetica-Bold".to_string(),
            size,
            color,
        };

        // Apply stamp
        if let Err(e) = writer.stamp_page(self.current_page, &[stamp]) {
            self.last_error = Some(format!("Failed to stamp: {}", e));
            cx.notify();
            return;
        }

        // Write to temporary file
        let temp_path = self.document.summary.path.with_extension("pdf.tmp");
        let mut temp_file = match File::create(&temp_path) {
            Ok(f) => f,
            Err(e) => {
                self.last_error = Some(format!("Failed to create temp file: {}", e));
                cx.notify();
                return;
            }
        };

        if let Err(e) = writer.write(&mut temp_file) {
            self.last_error = Some(format!("Failed to write: {}", e));
            cx.notify();
            return;
        }

        if let Err(e) = temp_file.sync_all() {
            self.last_error = Some(format!("Failed to sync: {}", e));
            cx.notify();
            return;
        }
        drop(temp_file);

        // Atomic rename
        if let Err(e) = std::fs::rename(&temp_path, &self.document.summary.path) {
            self.last_error = Some(format!("Failed to rename: {}", e));
            cx.notify();
            return;
        }

        tracing::info!("Stamp applied successfully!");

        // Clear cache to force reload
        self.page_cache.clear();
        self.last_error = Some(format!(
            "✓ Added '{}' stamp to page {}",
            text,
            self.current_page + 1
        ));
        cx.notify();
    }

    fn handle_ink_mouse_down(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        if self.annotation_mode != AnnotationMode::Ink {
            return;
        }
        tracing::warn!(
            "=== Mouse DOWN at position: ({}, {}) ===",
            event.position.x,
            event.position.y
        );
        tracing::warn!(
            "    Current image size: {} x {}",
            self.current_image_width,
            self.current_image_height
        );

        self.is_drawing = true;
        self.current_stroke.clear();
        self.current_stroke
            .push((event.position.x.into(), event.position.y.into()));
        cx.notify();
    }

    fn handle_ink_mouse_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if self.annotation_mode != AnnotationMode::Ink || !self.is_drawing {
            return;
        }
        // Only log every 10th point to avoid spam
        if self.current_stroke.len().is_multiple_of(10) {
            tracing::warn!("    Move to: ({}, {})", event.position.x, event.position.y);
        }
        self.current_stroke
            .push((event.position.x.into(), event.position.y.into()));
        cx.notify();
    }

    fn handle_ink_mouse_up(&mut self, _event: &MouseUpEvent, cx: &mut Context<Self>) {
        if self.annotation_mode != AnnotationMode::Ink || !self.is_drawing {
            return;
        }
        tracing::warn!("Mouse UP - stroke has {} points", self.current_stroke.len());
        self.is_drawing = false;
        if !self.current_stroke.is_empty() {
            self.ink_strokes.push(self.current_stroke.clone());
            tracing::warn!("Total strokes: {}", self.ink_strokes.len());
            self.current_stroke.clear();
        }
        cx.notify();
    }

    // ========== Selection Mode Methods ==========

    fn toggle_select_mode(&mut self, cx: &mut Context<Self>) {
        match self.annotation_mode {
            AnnotationMode::Select => {
                self.annotation_mode = AnnotationMode::None;
                self.selected_annotation = None;
                self.page_annotations.clear();
            }
            _ => {
                self.annotation_mode = AnnotationMode::Select;
                self.load_page_annotations(cx);
            }
        }
        cx.notify();
    }

    fn load_page_annotations(&mut self, _cx: &mut Context<Self>) {
        self.page_annotations.clear();

        // Parse annotations from current page
        let page_result = self.document.document().page(self.current_page);
        if let Ok(page) = page_result {
            let file = self.document.document().file();

            // Get annotations for this page
            for &annot_id in &page.annots {
                if let Ok(annot_obj) = file.resolve(annot_id) {
                    if let Ok(annot_dict) = annot_obj.as_dict() {
                        // Parse subtype
                        let subtype = annot_dict
                            .get_name("Subtype")
                            .unwrap_or("Unknown")
                            .to_string();

                        // Parse rect
                        if let Some(PdfObject::Array(rect_arr)) = annot_dict.get("Rect") {
                            if rect_arr.len() == 4 {
                                let x0 = match &rect_arr[0] {
                                    PdfObject::Real(x) => *x,
                                    PdfObject::Integer(x) => *x as f64,
                                    _ => continue,
                                };
                                let y0 = match &rect_arr[1] {
                                    PdfObject::Real(y) => *y,
                                    PdfObject::Integer(y) => *y as f64,
                                    _ => continue,
                                };
                                let x1 = match &rect_arr[2] {
                                    PdfObject::Real(x) => *x,
                                    PdfObject::Integer(x) => *x as f64,
                                    _ => continue,
                                };
                                let y1 = match &rect_arr[3] {
                                    PdfObject::Real(y) => *y,
                                    PdfObject::Integer(y) => *y as f64,
                                    _ => continue,
                                };

                                let rect = Rect { x0, y0, x1, y1 };

                                self.page_annotations.push(EditableAnnotation {
                                    object_id: annot_id,
                                    page_index: self.current_page,
                                    subtype,
                                    original_rect: rect,
                                    current_rect: rect,
                                });
                            }
                        }
                    }
                }
            }
        }

        tracing::info!(
            "Loaded {} annotations for page {}",
            self.page_annotations.len(),
            self.current_page
        );
    }

    fn hit_test(
        &self,
        screen_x: f32,
        screen_y: f32,
        page: &PageSummary,
        zoom: f32,
    ) -> HitTestResult {
        // Convert screen to PDF coordinates
        let (pdf_x, pdf_y) = self.screen_to_pdf(screen_x, screen_y, page, zoom);

        // Test annotation bodies (rect contains point)
        for annot in &self.page_annotations {
            let rect = &annot.current_rect;
            if pdf_x >= rect.x0 && pdf_x <= rect.x1 && pdf_y >= rect.y0 && pdf_y <= rect.y1 {
                return HitTestResult::AnnotationBody {
                    annotation_id: annot.object_id,
                };
            }
        }

        HitTestResult::None
    }

    fn handle_select_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.annotation_mode != AnnotationMode::Select {
            return;
        }

        let page = &self.document.summary.pages[self.current_page];
        let zoom = self.effective_zoom(window);

        match self.hit_test(event.position.x.into(), event.position.y.into(), page, zoom) {
            HitTestResult::AnnotationBody { annotation_id } => {
                self.selected_annotation = Some(annotation_id);

                // Find the annotation to get its rect
                if let Some(annot) = self
                    .page_annotations
                    .iter()
                    .find(|a| a.object_id == annotation_id)
                {
                    self.drag_state = Some(DragState::MovingAnnotation {
                        annotation_id,
                        start_screen_pos: (event.position.x.into(), event.position.y.into()),
                        start_pdf_rect: annot.current_rect,
                    });
                }
            }
            HitTestResult::None => {
                self.selected_annotation = None;
                self.drag_state = None;
            }
        }

        cx.notify();
    }

    fn handle_select_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.annotation_mode != AnnotationMode::Select {
            return;
        }

        if let Some(DragState::MovingAnnotation {
            annotation_id,
            start_screen_pos,
            start_pdf_rect,
        }) = &self.drag_state
        {
            let _page = &self.document.summary.pages[self.current_page];
            let zoom = self.effective_zoom(window);
            let scale = (PREVIEW_DPI / 72.0) * zoom;

            // Calculate screen delta (Pixels to f32)
            let screen_dx: f32 = (event.position.x - px(start_screen_pos.0)).into();
            let screen_dy: f32 = (event.position.y - px(start_screen_pos.1)).into();

            // Convert to PDF delta
            let pdf_dx = (screen_dx / scale) as f64;
            let pdf_dy = -((screen_dy / scale) as f64); // Negative because PDF Y increases upward

            // Update annotation's current rect
            if let Some(annot) = self
                .page_annotations
                .iter_mut()
                .find(|a| a.object_id == *annotation_id)
            {
                annot.current_rect = Rect {
                    x0: start_pdf_rect.x0 + pdf_dx,
                    y0: start_pdf_rect.y0 + pdf_dy,
                    x1: start_pdf_rect.x1 + pdf_dx,
                    y1: start_pdf_rect.y1 + pdf_dy,
                };
            }

            cx.notify();
        }
    }

    fn handle_select_mouse_up(&mut self, _event: &MouseUpEvent, cx: &mut Context<Self>) {
        if self.annotation_mode != AnnotationMode::Select {
            return;
        }

        if let Some(DragState::MovingAnnotation { annotation_id, .. }) = self.drag_state.take() {
            // Find the annotation's final rect
            if let Some(annot) = self
                .page_annotations
                .iter()
                .find(|a| a.object_id == annotation_id)
            {
                // Add to edit buffer
                self.edit_buffer.add(EditOperation::Move {
                    object_id: annotation_id,
                    page_index: self.current_page,
                    new_rect: annot.current_rect,
                });

                self.last_error = Some("Moved annotation (press Ctrl+S to save)".to_string());
            }
        }

        cx.notify();
    }

    fn delete_selected_annotation(&mut self, cx: &mut Context<Self>) {
        if let Some(selected_id) = self.selected_annotation {
            // Add delete operation to buffer
            self.edit_buffer.add(EditOperation::Delete {
                object_id: selected_id,
                page_index: self.current_page,
            });

            // Remove from display
            self.page_annotations.retain(|a| a.object_id != selected_id);
            self.selected_annotation = None;

            self.last_error = Some("Deleted annotation (press Ctrl+S to save)".to_string());
            cx.notify();
        }
    }

    fn save_edits(&mut self, cx: &mut Context<Self>) {
        if self.edit_buffer.is_empty() {
            self.last_error = Some("No edits to save".to_string());
            cx.notify();
            return;
        }

        // Read original PDF bytes
        let original_bytes = match std::fs::read(&self.document.summary.path) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.last_error = Some(format!("Failed to read PDF: {}", e));
                cx.notify();
                return;
            }
        };

        // Create writer
        let mut writer = match IncrementalWriter::new(original_bytes) {
            Ok(w) => w,
            Err(e) => {
                self.last_error = Some(format!("Failed to create writer: {}", e));
                cx.notify();
                return;
            }
        };

        // Apply all buffered operations
        for op in self.edit_buffer.operations.values() {
            let result = match op {
                EditOperation::Move {
                    object_id,
                    new_rect,
                    ..
                } => writer.update_annotation_rect(*object_id, *new_rect),
                EditOperation::Delete {
                    object_id,
                    page_index,
                } => writer.delete_annotation(*page_index, *object_id),
            };

            if let Err(e) = result {
                self.last_error = Some(format!("Failed to apply edit: {}", e));
                cx.notify();
                return;
            }
        }

        // Write to temp file
        let temp_path = self.document.summary.path.with_extension("pdf.tmp");
        let mut temp_file = match File::create(&temp_path) {
            Ok(f) => f,
            Err(e) => {
                self.last_error = Some(format!("Failed to create temp file: {}", e));
                cx.notify();
                return;
            }
        };

        if let Err(e) = writer.write(&mut temp_file) {
            self.last_error = Some(format!("Failed to write: {}", e));
            cx.notify();
            return;
        }

        if let Err(e) = temp_file.sync_all() {
            self.last_error = Some(format!("Failed to sync: {}", e));
            cx.notify();
            return;
        }
        drop(temp_file);

        // Atomic rename
        if let Err(e) = std::fs::rename(&temp_path, &self.document.summary.path) {
            self.last_error = Some(format!("Failed to rename: {}", e));
            cx.notify();
            return;
        }

        let num_edits = self.edit_buffer.operations.len();
        self.edit_buffer.clear();
        self.page_cache.clear();

        self.last_error = Some(format!("✓ Saved {} edit(s)", num_edits));

        // Reload annotations
        self.load_page_annotations(cx);
        cx.notify();
    }

    fn effective_zoom(&self, window: &Window) -> f32 {
        if self.fit_width {
            self.fit_width_zoom(window)
        } else {
            self.zoom
        }
    }

    fn fit_width_zoom(&self, window: &Window) -> f32 {
        let Some(preview) = self.page_cache.get(&self.current_page) else {
            return self.zoom;
        };

        let viewport_width = f32::from(window.viewport_size().width);
        let usable_width = (viewport_width - SIDEBAR_WIDTH - PAGE_VIEWPORT_CHROME).max(320.0);
        (usable_width / preview.pixel_width).clamp(MIN_ZOOM, MAX_ZOOM)
    }

    fn ensure_page_rendered(&mut self, index: usize) {
        if self.page_cache.contains_key(&index) {
            return;
        }

        match self
            .document
            .render_page_preview(index, PREVIEW_DPI, &mut self.gpu_context)
        {
            Ok(preview) => {
                self.page_cache.insert(index, preview);
                self.last_error = None;
            }
            Err(err) => {
                self.last_error = Some(err.to_string());
            }
        }
    }

    fn prefetch_nearby_pages(&mut self) {
        let page_count = self.document.summary.page_count;
        if page_count == 0 {
            return;
        }

        let previous = self.current_page.saturating_sub(1);
        let next = (self.current_page + 1).min(page_count - 1);

        for index in [self.current_page, previous, next] {
            self.ensure_page_rendered(index);
        }
    }

    fn button_colors(active: bool) -> ButtonPalette {
        if active {
            (0xcaa757, 0xe2c486, 0x221910)
        } else {
            (0x222a26, 0x39453f, 0xece6db)
        }
    }

    fn tool_button(
        &self,
        id: impl Into<String>,
        label: impl Into<String>,
        active: bool,
    ) -> gpui::Stateful<gpui::Div> {
        let (bg, border, text) = Self::button_colors(active);

        div()
            .id(id.into())
            .px_3()
            .py_2()
            .rounded_lg()
            .bg(rgb(bg))
            .border_1()
            .border_color(rgb(border))
            .text_sm()
            .font_weight(FontWeight::MEDIUM)
            .text_color(rgb(text))
            .cursor_pointer()
            .hover(move |style| style.bg(rgb(border)))
            .child(label.into())
    }

    fn stat_chip(
        &self,
        id: impl Into<String>,
        label: impl Into<String>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id.into())
            .px_3()
            .py_1()
            .rounded_full()
            .bg(rgb(0x1f2823))
            .border_1()
            .border_color(rgb(0x364239))
            .text_xs()
            .text_color(rgb(0xcabfae))
            .child(label.into())
    }

    fn page_list_item(&self, page: &PageSummary) -> gpui::Stateful<gpui::Div> {
        let active = page.index == self.current_page;
        let bg = if active { rgb(0xcaa757) } else { rgb(0x161d1a) };
        let border = if active { rgb(0xe5c989) } else { rgb(0x2e3932) };
        let title = if active { rgb(0x221910) } else { rgb(0xf4eee2) };
        let detail = if active { rgb(0x523717) } else { rgb(0xa29d90) };

        div()
            .id(format!("page-{}", page.index))
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .rounded_xl()
            .bg(bg)
            .border_1()
            .border_color(border)
            .cursor_pointer()
            .hover(move |style| style.border_color(rgb(0xcaa757)))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .text_color(title)
                            .child(format!("Page {}", page.index + 1)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(detail)
                            .child(format!("{:.0} pt", page.height)),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(detail)
                    .child(format!("{:.0} x {:.0} pt", page.width, page.height)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(detail)
                    .child(format!("rotation {}", page.rotate)),
            )
    }

    fn current_render_image(&self) -> Option<Arc<RenderImage>> {
        self.page_cache
            .get(&self.current_page)
            .map(|preview| preview.image.clone())
    }
}

impl Focusable for Viewer {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Viewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.prefetch_nearby_pages();

        let page = &self.document.summary.pages[self.current_page];
        let zoom = self.effective_zoom(window);
        let zoom_pct = zoom * 100.0;
        let document_name = self.document_name();
        let document_path = self.document_path_label();
        let version = self.version_label();
        let page_items = self
            .document
            .summary
            .pages
            .iter()
            .map(|page| {
                let page_index = page.index;
                self.page_list_item(page)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_page(page_index, cx);
                    }))
            })
            .collect::<Vec<_>>();

        window.set_window_title(&format!(
            "{document_name} - page {}/{} - {:.0}%",
            self.current_page + 1,
            self.document.summary.page_count,
            zoom_pct
        ));

        let page_surface = match self.page_cache.get(&self.current_page) {
            Some(preview) => {
                let width = preview.pixel_width * zoom;
                let height = preview.pixel_height * zoom;

                // Track current image dimensions for coordinate conversion
                self.current_image_width = width;
                self.current_image_height = height;

                let page_container = div()
                    .bg(rgb(0xffffff))
                    .border_1()
                    .border_color(rgb(0xd8d0c2))
                    .shadow_lg()
                    .relative() // Make container positioned for absolute children
                    .child(
                        img(self.current_render_image().expect("preview image"))
                            .w(px(width))
                            .h(px(height))
                            .object_fit(gpui::ObjectFit::Fill),
                    );

                // Add stroke overlay when in ink mode
                let page_container = if self.annotation_mode == AnnotationMode::Ink {
                    let mut container = page_container;

                    // Render completed strokes
                    for stroke in &self.ink_strokes {
                        for point in stroke {
                            container = container.child(
                                div()
                                    .absolute()
                                    .left(px(point.0 - 2.0))
                                    .top(px(point.1 - 2.0))
                                    .w(px(4.0))
                                    .h(px(4.0))
                                    .rounded(px(2.0))
                                    .bg(rgb(0xff0000)), // Red
                            );
                        }
                    }

                    // Render current stroke being drawn
                    for point in &self.current_stroke {
                        container = container.child(
                            div()
                                .absolute()
                                .left(px(point.0 - 2.0))
                                .top(px(point.1 - 2.0))
                                .w(px(4.0))
                                .h(px(4.0))
                                .rounded(px(2.0))
                                .bg(rgb(0xff0000)), // Red
                        );
                    }

                    // Add a crosshair at the last point for debugging
                    if let Some(last_point) = self.current_stroke.last() {
                        container = container
                            .child(
                                div()
                                    .absolute()
                                    .left(px(last_point.0 - 10.0))
                                    .top(px(last_point.1 - 1.0))
                                    .w(px(20.0))
                                    .h(px(2.0))
                                    .bg(rgb(0x00ff00)), // Green horizontal line
                            )
                            .child(
                                div()
                                    .absolute()
                                    .left(px(last_point.0 - 1.0))
                                    .top(px(last_point.1 - 10.0))
                                    .w(px(2.0))
                                    .h(px(20.0))
                                    .bg(rgb(0x00ff00)), // Green vertical line
                            );
                    }

                    container
                } else if self.annotation_mode == AnnotationMode::Select {
                    let mut container = page_container;

                    // Render selection highlight for selected annotation
                    if let Some(selected_id) = self.selected_annotation {
                        if let Some(annot) = self
                            .page_annotations
                            .iter()
                            .find(|a| a.object_id == selected_id)
                        {
                            let scale = (PREVIEW_DPI / 72.0) * zoom;
                            let screen_x0 = (annot.current_rect.x0 * scale as f64) as f32;
                            let screen_y0 =
                                ((page.height - annot.current_rect.y1) * scale as f64) as f32;
                            let screen_width = ((annot.current_rect.x1 - annot.current_rect.x0)
                                * scale as f64)
                                as f32;
                            let screen_height = ((annot.current_rect.y1 - annot.current_rect.y0)
                                * scale as f64)
                                as f32;

                            // Blue selection border
                            container = container.child(
                                div()
                                    .absolute()
                                    .left(px(screen_x0))
                                    .top(px(screen_y0))
                                    .w(px(screen_width))
                                    .h(px(screen_height))
                                    .border_2()
                                    .border_color(rgb(0x4A90E2)),
                            );
                        }
                    }

                    container
                } else {
                    page_container
                };

                // Add mouse handlers when in ink mode
                let page_container = if self.annotation_mode == AnnotationMode::Ink {
                    page_container
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, event, _, cx| {
                                this.handle_ink_mouse_down(event, cx);
                            }),
                        )
                        .on_mouse_move(cx.listener(|this, event, _, cx| {
                            this.handle_ink_mouse_move(event, cx);
                        }))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, event, _, cx| {
                                this.handle_ink_mouse_up(event, cx);
                            }),
                        )
                } else if self.annotation_mode == AnnotationMode::Select {
                    page_container
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, event, window, cx| {
                                this.handle_select_mouse_down(event, window, cx);
                            }),
                        )
                        .on_mouse_move(cx.listener(|this, event, window, cx| {
                            this.handle_select_mouse_move(event, window, cx);
                        }))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, event, _, cx| {
                                this.handle_select_mouse_up(event, cx);
                            }),
                        )
                } else {
                    page_container
                };

                div()
                    .flex()
                    .justify_center()
                    .w_full()
                    .child(
                        div()
                            .p_4()
                            .rounded_2xl()
                            .bg(rgb(0xd7cfbe))
                            .border_1()
                            .border_color(rgb(0xbeb29d))
                            .shadow_lg()
                            .child(page_container),
                    )
                    .into_any_element()
            }
            None => {
                let message = self
                    .last_error
                    .clone()
                    .unwrap_or_else(|| "page preview is not available yet".to_string());

                div()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .items_center()
                    .gap_3()
                    .size_full()
                    .rounded_2xl()
                    .bg(rgb(0xe5dccd))
                    .border_1()
                    .border_color(rgb(0xd0c5b4))
                    .text_color(rgb(0x5d5549))
                    .child(
                        div()
                            .text_xl()
                            .font_weight(FontWeight::BOLD)
                            .child("Preview unavailable"),
                    )
                    .child(div().text_sm().child(message))
                    .into_any_element()
            }
        };

        div()
            .id("pdf-viewer")
            .key_context("PdfViewer")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::next_page))
            .on_action(cx.listener(Self::previous_page))
            .on_action(cx.listener(Self::first_page))
            .on_action(cx.listener(Self::last_page))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::actual_size))
            .on_action(cx.listener(Self::fit_width))
            .on_action(cx.listener(Self::toggle_ink_mode))
            .on_action(cx.listener(Self::save_annotation))
            .on_action(cx.listener(Self::cancel_annotation))
            .on_action(cx.listener(Self::add_watermark))
            .on_action(cx.listener(Self::add_draft_stamp))
            .on_action(cx.listener(Self::add_confidential_stamp))
            .on_action(cx.listener(|this, _: &ToggleSelectMode, _, cx| {
                this.toggle_select_mode(cx);
            }))
            .on_action(cx.listener(|this, _: &SaveEdits, _, cx| {
                this.save_edits(cx);
            }))
            .on_action(cx.listener(|this, _: &DeleteSelected, _, cx| {
                this.delete_selected_annotation(cx);
            }))
            .size_full()
            .flex()
            .bg(rgb(0x0d1210))
            .text_color(rgb(0xf2ecdf))
            .child(
                div()
                    .w(px(SIDEBAR_WIDTH))
                    .h_full()
                    .flex()
                    .flex_col()
                    .bg(rgb(0x111815))
                    .border_r_1()
                    .border_color(rgb(0x27322c))
                    .child(
                        div()
                            .p_5()
                            .flex()
                            .flex_col()
                            .gap_4()
                            .border_b_1()
                            .border_color(rgb(0x27322c))
                            .child(
                                div()
                                    .flex()
                                    .justify_between()
                                    .items_center()
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap_1()
                                            .child(
                                                div()
                                                    .text_xl()
                                                    .font_weight(FontWeight::BOLD)
                                                    .text_color(rgb(0xf7f1e6))
                                                    .child("zpdf"),
                                            )
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .text_color(rgb(0x9b9b8f))
                                                    .child("Desktop reader"),
                                            ),
                                    )
                                    .child(self.stat_chip("doc-version", version.clone())),
                            )
                            .child(
                                div()
                                    .p_4()
                                    .rounded_2xl()
                                    .bg(rgb(0x171f1b))
                                    .border_1()
                                    .border_color(rgb(0x2b3730))
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(
                                        div().text_sm().text_color(rgb(0x8d897d)).child("Document"),
                                    )
                                    .child(
                                        div()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(rgb(0xf5efe4))
                                            .child(document_name.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(rgb(0xa9a292))
                                            .child(document_path),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .gap_2()
                                            .flex_wrap()
                                            .child(self.stat_chip(
                                                "page-count",
                                                format!(
                                                    "{} pages",
                                                    self.document.summary.page_count
                                                ),
                                            ))
                                            .child(self.stat_chip(
                                                "active-page",
                                                format!("Page {}", self.current_page + 1),
                                            )),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .id("page-sidebar-scroll")
                            .flex_1()
                            .overflow_y_scroll()
                            .p_4()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(
                                        div()
                                            .px_1()
                                            .pb_2()
                                            .text_sm()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(rgb(0xbab09f))
                                            .child("Pages"),
                                    )
                                    .children(page_items),
                            ),
                    )
                    .child(
                        div().p_4().border_t_1().border_color(rgb(0x27322c)).child(
                            div()
                                .p_4()
                                .rounded_2xl()
                                .bg(rgb(0x151d19))
                                .border_1()
                                .border_color(rgb(0x2b3730))
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(rgb(0xefe7da))
                                        .child("Shortcuts"),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(rgb(0xa9a292))
                                        .child("j / k switch pages"),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(rgb(0xa9a292))
                                        .child("+ / - adjust zoom"),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(rgb(0xa9a292))
                                        .child("f fit width, 0 actual size"),
                                ),
                        ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .bg(rgb(0x111613))
                    .child(
                        div()
                            .h(px(56.0))
                            .px_4()
                            .flex()
                            .items_center()
                            .justify_between()
                            .bg(rgb(0x101513))
                            .border_b_1()
                            .border_color(rgb(0x27322c))
                            .child(
                                div()
                                    .flex_1()
                                    .h_full()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .on_mouse_down(MouseButton::Left, |_event, window, _cx| {
                                        window.start_window_move();
                                    })
                                    .on_mouse_down(MouseButton::Right, |event, window, _cx| {
                                        window.show_window_menu(event.position);
                                    })
                                    .child(div().size(px(12.0)).rounded_full().bg(rgb(0xcaa757)))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap_0p5()
                                            .child(
                                                div()
                                                    .font_weight(FontWeight::BOLD)
                                                    .text_color(rgb(0xf7f1e6))
                                                    .child(document_name),
                                            )
                                            .child(
                                                div().text_sm().text_color(rgb(0x9f9a8e)).child(
                                                    format!(
                                                        "{} • page {}/{}",
                                                        version,
                                                        self.current_page + 1,
                                                        self.document.summary.page_count
                                                    ),
                                                ),
                                            ),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(self.stat_chip(
                                        "titlebar-page-size",
                                        format!("{:.0} x {:.0} pt", page.width, page.height),
                                    ))
                                    .child(self.stat_chip(
                                        "titlebar-zoom-mode",
                                        if self.fit_width {
                                            "Fit width".to_string()
                                        } else {
                                            "Actual zoom".to_string()
                                        },
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .px_4()
                            .py_3()
                            .flex()
                            .justify_between()
                            .items_center()
                            .gap_3()
                            .bg(rgb(0x171e1b))
                            .border_b_1()
                            .border_color(rgb(0x27322c))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(self.tool_button("first-page", "|<", false).on_click(
                                        cx.listener(|this, _, window, cx| {
                                            this.first_page(&FirstPage, window, cx)
                                        }),
                                    ))
                                    .child(self.tool_button("prev-page", "Prev", false).on_click(
                                        cx.listener(|this, _, window, cx| {
                                            this.previous_page(&PreviousPage, window, cx)
                                        }),
                                    ))
                                    .child(div().px_3().text_sm().text_color(rgb(0xbfb5a5)).child(
                                        format!(
                                            "Page {} / {}",
                                            self.current_page + 1,
                                            self.document.summary.page_count
                                        ),
                                    ))
                                    .child(self.tool_button("next-page", "Next", false).on_click(
                                        cx.listener(|this, _, window, cx| {
                                            this.next_page(&NextPage, window, cx)
                                        }),
                                    ))
                                    .child(self.tool_button("last-page", ">|", false).on_click(
                                        cx.listener(|this, _, window, cx| {
                                            this.last_page(&LastPage, window, cx)
                                        }),
                                    )),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(self.tool_button("zoom-out", "-", false).on_click(
                                        cx.listener(|this, _, window, cx| {
                                            this.zoom_out(&ZoomOut, window, cx)
                                        }),
                                    ))
                                    .child(
                                        div()
                                            .px_3()
                                            .text_sm()
                                            .text_color(rgb(0xbfb5a5))
                                            .child(format!("{zoom_pct:.0}%")),
                                    )
                                    .child(self.tool_button("zoom-in", "+", false).on_click(
                                        cx.listener(|this, _, window, cx| {
                                            this.zoom_in(&ZoomIn, window, cx)
                                        }),
                                    ))
                                    .child(
                                        self.tool_button("actual-size", "100%", !self.fit_width)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.actual_size(&ActualSize, window, cx)
                                            })),
                                    )
                                    .child(
                                        self.tool_button("fit-width", "Fit width", self.fit_width)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.fit_width(&FitWidth, window, cx)
                                            })),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        self.tool_button(
                                            "ink-mode",
                                            "✏ Ink",
                                            self.annotation_mode == AnnotationMode::Ink,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, window, cx| {
                                                this.toggle_ink_mode(&ToggleInkMode, window, cx)
                                            }),
                                        ),
                                    )
                                    .child(
                                        self.tool_button(
                                            "select-mode",
                                            "🖱 Select",
                                            self.annotation_mode == AnnotationMode::Select,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.toggle_select_mode(cx)
                                            }),
                                        ),
                                    )
                                    .when(
                                        self.annotation_mode == AnnotationMode::Ink,
                                        |container| {
                                            container
                                                .child(
                                                    div()
                                                        .px_3()
                                                        .text_sm()
                                                        .text_color(rgb(0xbfb5a5))
                                                        .child(format!(
                                                            "{} stroke(s)",
                                                            self.ink_strokes.len()
                                                        )),
                                                )
                                                .child(
                                                    self.tool_button(
                                                        "save-annotation",
                                                        "Save",
                                                        false,
                                                    )
                                                    .on_click(cx.listener(|this, _, window, cx| {
                                                        this.save_annotation(
                                                            &SaveAnnotation,
                                                            window,
                                                            cx,
                                                        )
                                                    })),
                                                )
                                                .child(
                                                    self.tool_button(
                                                        "cancel-annotation",
                                                        "Cancel",
                                                        false,
                                                    )
                                                    .on_click(cx.listener(|this, _, window, cx| {
                                                        this.cancel_annotation(
                                                            &CancelAnnotation,
                                                            window,
                                                            cx,
                                                        )
                                                    })),
                                                )
                                        },
                                    )
                                    .when(
                                        self.annotation_mode == AnnotationMode::Select,
                                        |container| {
                                            container
                                                .child(
                                                    div()
                                                        .px_3()
                                                        .text_sm()
                                                        .text_color(rgb(0xbfb5a5))
                                                        .child(format!(
                                                            "{} annotation(s)",
                                                            self.page_annotations.len()
                                                        )),
                                                )
                                                .when(!self.edit_buffer.is_empty(), |c| {
                                                    c.child(
                                                        div()
                                                            .px_3()
                                                            .text_sm()
                                                            .text_color(rgb(0xff6b35))
                                                            .child(format!(
                                                                "{} edit(s) pending",
                                                                self.edit_buffer.operations.len()
                                                            )),
                                                    )
                                                })
                                        },
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .px_3()
                                            .text_sm()
                                            .text_color(rgb(0xbfb5a5))
                                            .child("Stamps:"),
                                    )
                                    .child(
                                        self.tool_button("watermark", "💧 Water", false).on_click(
                                            cx.listener(|this, _, window, cx| {
                                                this.add_watermark(&AddWatermark, window, cx)
                                            }),
                                        ),
                                    )
                                    .child(self.tool_button("draft", "📝 Draft", false).on_click(
                                        cx.listener(|this, _, window, cx| {
                                            this.add_draft_stamp(&AddDraftStamp, window, cx)
                                        }),
                                    ))
                                    .child(
                                        self.tool_button("confidential", "🔒 Conf", false)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.add_confidential_stamp(
                                                    &AddConfidentialStamp,
                                                    window,
                                                    cx,
                                                )
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .id("page-scroll")
                            .flex_1()
                            .overflow_scroll()
                            .p_6()
                            .bg(rgb(0xc6c0b3))
                            .child(page_surface),
                    )
                    .child(
                        div()
                            .px_4()
                            .py_3()
                            .flex()
                            .justify_between()
                            .items_center()
                            .gap_3()
                            .bg(rgb(0xf1eadf))
                            .border_t_1()
                            .border_color(rgb(0xd1c5b4))
                            .child(div().text_sm().text_color(rgb(0x594f43)).child(format!(
                                "Page {} • {:.0} x {:.0} pt • rotate {}",
                                page.index + 1,
                                page.width,
                                page.height,
                                page.rotate
                            )))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(0x594f43))
                                    .child("Arrows or j/k to navigate • +/- to zoom"),
                            ),
                    ),
            )
    }
}
