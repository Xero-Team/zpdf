use std::path::PathBuf;

use gpui::{
    point, px, size, App, AppContext, Focusable, KeyBinding, TitlebarOptions, WindowBounds,
    WindowOptions,
};
use gpui_platform::application;
use thiserror::Error;

use crate::actions::{
    ActualSize, FirstPage, FitWidth, LastPage, NextPage, PreviousPage, Quit, ZoomIn, ZoomOut,
};
use crate::document::{load_document, DocumentError};
use crate::viewer::Viewer;

#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    Document(#[from] DocumentError),
}

pub fn run(path: PathBuf) -> Result<(), AppError> {
    let document = load_document(path)?;

    application().run(move |cx: &mut App| {
        let window_size = size(px(1220.), px(860.));
        let window_min_size = size(px(960.), px(640.));
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        let window_decorations = Some(gpui::WindowDecorations::Client);
        #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
        let window_decorations = None;

        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("ctrl-q", Quit, None),
            KeyBinding::new("j", NextPage, Some("PdfViewer")),
            KeyBinding::new("down", NextPage, Some("PdfViewer")),
            KeyBinding::new("right", NextPage, Some("PdfViewer")),
            KeyBinding::new("pagedown", NextPage, Some("PdfViewer")),
            KeyBinding::new("space", NextPage, Some("PdfViewer")),
            KeyBinding::new("k", PreviousPage, Some("PdfViewer")),
            KeyBinding::new("up", PreviousPage, Some("PdfViewer")),
            KeyBinding::new("left", PreviousPage, Some("PdfViewer")),
            KeyBinding::new("pageup", PreviousPage, Some("PdfViewer")),
            KeyBinding::new("home", FirstPage, Some("PdfViewer")),
            KeyBinding::new("g", FirstPage, Some("PdfViewer")),
            KeyBinding::new("end", LastPage, Some("PdfViewer")),
            KeyBinding::new("shift-g", LastPage, Some("PdfViewer")),
            KeyBinding::new("=", ZoomIn, Some("PdfViewer")),
            KeyBinding::new("+", ZoomIn, Some("PdfViewer")),
            KeyBinding::new("-", ZoomOut, Some("PdfViewer")),
            KeyBinding::new("0", ActualSize, Some("PdfViewer")),
            KeyBinding::new("f", FitWidth, Some("PdfViewer")),
        ]);

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::centered(window_size, cx)),
                window_min_size: Some(window_min_size),
                window_decorations,
                titlebar: Some(TitlebarOptions {
                    title: Some("zpdf GPUI viewer".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(12.0), px(12.0))),
                }),
                ..Default::default()
            },
            |window, cx| {
                let viewer = cx.new(|cx| Viewer::new(document, cx));
                viewer.focus_handle(cx).focus(window, cx);
                viewer
            },
        )
        .expect("failed to open GPUI window");

        cx.activate(true);
    });

    Ok(())
}
