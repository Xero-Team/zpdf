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
        FitWidth,
        ToggleInkMode,
        SaveAnnotation,
        CancelAnnotation,
        AddWatermark,
        AddConfidentialStamp,
        AddDraftStamp,
        ToggleSelectMode,
        SaveEdits,
        DeleteSelected
    ]
);
