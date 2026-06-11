use gpui::actions;

actions!(
    zpdf_viewer_gpui,
    [
        Quit,
        NextPage,
        PreviousPage,
        FirstPage,
        LastPage,
        ZoomIn,
        ZoomOut,
        ActualSize,
        FitWidth
    ]
);
