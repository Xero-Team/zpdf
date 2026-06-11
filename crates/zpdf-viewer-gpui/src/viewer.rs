use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    div, img, prelude::*, px, rgb, App, Context, FocusHandle, Focusable, FontWeight,
    InteractiveElement, MouseButton, ParentElement, RenderImage, StatefulInteractiveElement,
    Styled, StyledImage, Window,
};
use zpdf::gpu::GpuContext;

use crate::actions::{
    ActualSize, FirstPage, FitWidth, LastPage, NextPage, PreviousPage, ZoomIn, ZoomOut,
};
use crate::document::{LoadedDocument, PagePreview, PageSummary};

const PREVIEW_DPI: f32 = 144.0;
const MIN_ZOOM: f32 = 0.35;
const MAX_ZOOM: f32 = 3.5;
const ZOOM_STEP: f32 = 1.2;
const SIDEBAR_WIDTH: f32 = 264.0;
const PAGE_VIEWPORT_CHROME: f32 = 220.0;
type ButtonPalette = (u32, u32, u32);

pub struct Viewer {
    document: LoadedDocument,
    focus_handle: FocusHandle,
    current_page: usize,
    zoom: f32,
    fit_width: bool,
    page_cache: HashMap<usize, PagePreview>,
    gpu_context: Option<GpuContext>,
    last_error: Option<String>,
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
                            .child(
                                div()
                                    .bg(rgb(0xffffff))
                                    .border_1()
                                    .border_color(rgb(0xd8d0c2))
                                    .shadow_lg()
                                    .child(
                                        img(self.current_render_image().expect("preview image"))
                                            .w(px(width))
                                            .h(px(height))
                                            .object_fit(gpui::ObjectFit::Fill),
                                    ),
                            ),
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
