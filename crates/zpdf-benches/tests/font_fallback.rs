use zpdf::{ContentInterpreter, PdfDocument, RenderBackend};
use zpdf_benches::{font_fallback_workloads as fixtures, font_workloads};

#[test]
fn mixed_faces_preserve_original_widths_text_and_visible_glyphs() {
    let doc = fixtures::document(false, 0, false, 16);
    let setup = fixtures::assert_mixed(&doc, 16);
    let svg = zpdf_svg_export::display_list_to_svg(
        &setup.dl,
        &setup.font_cache,
        &setup.image_cache,
        &Default::default(),
    );
    let contours: usize = svg
        .split("<path d=\"")
        .skip(1)
        .map(|path| path.split('"').next().unwrap().matches('M').count())
        .sum();
    // Five geometric outlines plus four contours in the real Noto glyph 文.
    assert_eq!(contours, 9 * 16);
}

#[test]
fn forms_and_vertical_origins_keep_original_pdf_placement() {
    for form in [false, true] {
        let doc = fixtures::document(true, 0, form, 1);
        let setup = font_workloads::setup(&doc, 144.0);
        let expected = [
            (-2.0, -7.0, -9.0),
            (-4.0, -17.0, -11.0),
            (-3.0, -28.5, -8.0),
            (-2.0, -35.5, -10.0),
            (-1.5, -46.8, -3.0),
            (-5.5, -49.8, -12.0),
            (-5.0, -61.0, -9.0),
            (-2.5, -70.5, -7.0),
        ];
        let mut count = 0;
        for command in &setup.dl.commands {
            if let zpdf::display_list::RenderCommand::DrawGlyphRun(run) = command {
                let font = setup.font_cache.get(run.font_id).unwrap();
                for glyph in &run.glyphs {
                    let (x, y, advance) = expected[count];
                    assert!((glyph.x - x).abs() < 0.001, "glyph {count} x");
                    assert!((glyph.y - y).abs() < 0.001, "glyph {count} y");
                    assert!((glyph.advance - advance).abs() < 0.001);
                    if matches!(count, 1..=3 | 6) {
                        assert!(font.is_substitute);
                    }
                    count += 1;
                }
            }
        }
        assert_eq!(count, 8);
    }
}

#[test]
fn text_clipping_uses_the_union_of_all_drawing_faces() {
    let doc = fixtures::document(false, 7, false, 1);
    let setup = font_workloads::setup(&doc, 144.0);
    assert!(!setup
        .dl
        .commands
        .iter()
        .any(|command| matches!(command, zpdf::display_list::RenderCommand::DrawGlyphRun(_))));
    let image = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&setup.font_cache)
        .render_display_list(&setup.dl, setup.scale)
        .unwrap();
    let ordinary = fixtures::document(false, 0, false, 1);
    let expected = font_workloads::setup(&ordinary, 144.0);
    let reference = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&expected.font_cache)
        .render_display_list(&expected.dl, expected.scale)
        .unwrap();
    assert_eq!(image.data, reference.data);
}

#[test]
fn late_registration_recovers_an_existing_page_cache_without_reusing_ids() {
    let provider = std::sync::Arc::new(zpdf::FontProvider::memory_only());
    // A registration unrelated to the miss enables and populates resolution.
    provider.register(font_workloads::PRIMARY, 0).unwrap();
    let doc = PdfDocument::open(fixtures::pdf(false, 0, false, 1))
        .unwrap()
        .with_font_provider(std::sync::Arc::clone(&provider));
    let page = doc.page(0).unwrap();
    let bytes = doc.page_content_bytes(&page).unwrap();
    let mut fonts = doc.load_page_fonts(&page);
    let before = ContentInterpreter::new(page.effective_box())
        .with_fonts(&mut fonts)
        .interpret(&bytes);
    provider.register(font_workloads::FALLBACK, 0).unwrap();
    let after = ContentInterpreter::new(page.effective_box())
        .with_fonts(&mut fonts)
        .interpret(&bytes);
    let runs = |dl: &zpdf::display_list::DisplayList| {
        dl.commands
            .iter()
            .filter(|cmd| matches!(cmd, zpdf::display_list::RenderCommand::DrawGlyphRun(_)))
            .count()
    };
    assert_eq!(runs(&before), 1);
    assert!(runs(&after) > 1);
    let zpdf::display_list::RenderCommand::DrawGlyphRun(old) = &before.commands[0] else {
        panic!("old run")
    };
    assert!(!fonts.get(old.font_id).unwrap().is_substitute);
}

#[test]
fn transforms_spacing_tj_and_invisible_text_keep_original_layout() {
    let provider = fixtures::provider();
    let doc = PdfDocument::open(fixtures::pdf(false, 0, false, 1)).unwrap();
    let page = doc.page(0).unwrap();
    let mut original_fonts = doc.load_page_fonts(&page);
    let mut fallback_fonts = doc.load_page_fonts(&page).with_provider(provider);
    let content = b"q 1 .2 .3 1 5 9 cm BT /F1 10 Tf 80 Tz 2 Tc 4 Tw 3 Ts 0 1 -1 0 80 90 Tm [<000100020003> 125 <00040005000600070008>] TJ ET Q";
    let mut original_spans = Vec::new();
    let mut fallback_spans = Vec::new();
    let original = ContentInterpreter::new(page.effective_box())
        .with_fonts(&mut original_fonts)
        .with_text_sink(&mut original_spans)
        .interpret(content);
    let fallback = ContentInterpreter::new(page.effective_box())
        .with_fonts(&mut fallback_fonts)
        .with_text_sink(&mut fallback_spans)
        .interpret(content);
    let positions = |dl: &zpdf::display_list::DisplayList| {
        dl.commands
            .iter()
            .filter_map(|command| {
                if let zpdf::display_list::RenderCommand::DrawGlyphRun(run) = command {
                    Some(run)
                } else {
                    None
                }
            })
            .flat_map(|run| {
                run.glyphs.iter().map(|glyph| {
                    (
                        glyph.x,
                        glyph.y,
                        glyph.advance,
                        run.transform.a,
                        run.transform.b,
                        run.transform.c,
                        run.transform.d,
                        run.transform.e,
                        run.transform.f,
                        run.h_scale,
                    )
                })
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(positions(&original), positions(&fallback));
    assert_eq!(
        original_spans
            .iter()
            .map(|s| (&s.text, s.x, s.y, s.advance))
            .collect::<Vec<_>>(),
        fallback_spans
            .iter()
            .map(|s| (&s.text, s.x, s.y, s.advance))
            .collect::<Vec<_>>()
    );
    let mut fonts = doc
        .load_page_fonts(&page)
        .with_provider(fixtures::provider());
    let before = fonts.len();
    let mut spans = Vec::new();
    let invisible = ContentInterpreter::new(page.effective_box())
        .with_fonts(&mut fonts)
        .with_text_sink(&mut spans)
        .interpret(b"BT /F1 10 Tf 3 Tr <00010002000300040005000600070008> Tj ET");
    assert!(invisible.commands.is_empty());
    assert_eq!(
        fonts.len(),
        before,
        "invisible text should not load drawing faces"
    );
    assert_eq!(spans[0].text, fixtures::TEXT);
}

#[test]
fn translucent_pattern_is_painted_once_through_the_mixed_face_union() {
    // The overlapping glyph origins make painting each face separately visible
    // as darker alpha. Compare patterned text with one explicit union clip.
    let pattern = b"0 0 1 rg 0 0 100 100 re f";
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 280] /Resources << /Pattern << /P 4 0 R >> /ExtGState << /GS << /ca .5 >> >> >> >>".to_vec(),
        [format!("<< /Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 100 100] /XStep 100 /YStep 100 /Resources << >> /Length {} >>\nstream\n", pattern.len()).into_bytes(), pattern.to_vec(), b"\nendstream".to_vec()].concat(),
    ];
    let mut bytes = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(bytes.len());
        bytes.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        bytes.extend_from_slice(object);
        bytes.extend_from_slice(b"\nendobj\n");
    }
    let start = bytes.len();
    bytes.extend_from_slice(b"xref\n0 5\n0000000000 65535 f \n");
    for offset in offsets {
        bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    bytes.extend_from_slice(
        format!("trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{start}\n%%EOF\n").as_bytes(),
    );
    let pattern_doc = PdfDocument::open(bytes).unwrap();
    let pattern_page = pattern_doc.page(0).unwrap();
    let source = fixtures::document(false, 0, false, 1);
    let source_page = source.page(0).unwrap();
    let render = |content: &[u8]| {
        let mut fonts = source.load_page_fonts(&source_page);
        let dl = ContentInterpreter::new(source_page.effective_box())
            .with_fonts(&mut fonts)
            .with_document(pattern_doc.file(), &pattern_page.resources)
            .interpret(content);
        zpdf::cpu::CpuRenderer::new()
            .with_fonts(&fonts)
            .render_display_list(&dl, 2.0)
            .unwrap()
    };
    let painted = render(b"q /GS gs /Pattern cs /P scn BT /F1 20 Tf -8 Tc 20 200 Td <0001000200030004000500060007> Tj ET Q");
    let clipped = render(b"q BT /F1 20 Tf -8 Tc 7 Tr 20 200 Td <0001000200030004000500060007> Tj ET /GS gs /Pattern cs /P scn 0 0 240 280 re f Q");
    assert_eq!(painted.data, clipped.data);
    assert!(painted
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .any(|p| p[2] > 200 && (90..200).contains(&p[0])));
}

#[test]
fn space_only_text_clip_blocks_later_paint_and_restores_on_q() {
    let doc = fixtures::document(false, 0, false, 1);
    let page = doc.page(0).unwrap();
    let mut fonts = doc.load_page_fonts(&page);
    let dl = ContentInterpreter::new(page.effective_box()).with_fonts(&mut fonts)
        .interpret(b"q BT /F1 20 Tf 7 Tr 20 200 Td <0005> Tj ET 0 0 240 280 re f Q 1 0 0 rg 200 10 10 10 re f");
    let image = zpdf::cpu::CpuRenderer::new()
        .with_fonts(&fonts)
        .render_display_list(&dl, 1.0)
        .unwrap();
    assert_eq!(&image.data[(80 * 240 + 20) * 4..][..3], &[255, 255, 255]);
    assert_eq!(&image.data[(265 * 240 + 205) * 4..][..3], &[255, 0, 0]);
    let svg = zpdf_svg_export::display_list_to_svg(
        &dl,
        &fonts,
        &zpdf::ImageCache::new(),
        &Default::default(),
    );
    assert!(
        svg.contains("<path d=\"\""),
        "SVG must retain the empty clip"
    );
    #[cfg(feature = "gpu-render")]
    {
        let gpu = zpdf::gpu::WgpuRenderer::new()
            .with_fonts(&fonts)
            .render_display_list(&dl, 1.0)
            .unwrap();
        assert_eq!(gpu.data, image.data);
    }
}

#[cfg(feature = "gpu-render")]
#[test]
fn gpu_places_every_fallback_glyph_in_its_cpu_cell() {
    for vertical in [false, true] {
        let doc = fixtures::document(vertical, 0, false, 1);
        let setup = font_workloads::setup(&doc, 144.0);
        let cpu = zpdf::cpu::CpuRenderer::new()
            .with_fonts(&setup.font_cache)
            .render_display_list(&setup.dl, setup.scale)
            .unwrap();
        let mut renderer = zpdf::gpu::WgpuRenderer::new().with_fonts(&setup.font_cache);
        eprintln!(
            "fallback GPU: {}",
            renderer
                .adapter_identity()
                .expect("GPU required for this feature-gated acceptance test")
                .describe()
        );
        let gpu = renderer
            .render_display_list(&setup.dl, setup.scale)
            .unwrap();
        let mut count = 0;
        for command in &setup.dl.commands {
            if let zpdf::display_list::RenderCommand::DrawGlyphRun(run) = command {
                for glyph in &run.glyphs {
                    if !matches!(count, 4 | 7) {
                        let origin = zpdf::Point::new(glyph.x as f64, glyph.y as f64)
                            .transform(&run.transform);
                        let x0 = (origin.x.max(0.0) * setup.scale as f64) as usize;
                        let x1 = ((origin.x + 10.0) * setup.scale as f64) as usize;
                        let y0 = ((280.0 - origin.y - 10.0).max(0.0) * setup.scale as f64) as usize;
                        let y1 = ((280.0 - origin.y + 2.0) * setup.scale as f64) as usize;
                        let ink = |data: &[u8]| {
                            (y0..y1)
                                .flat_map(|y| {
                                    (x0..x1).map(move |x| (y * cpu.width as usize + x) * 4)
                                })
                                .filter(|&i| data[i] < 128)
                                .count()
                        };
                        let (expected, actual) = (ink(&cpu.data), ink(&gpu.data));
                        assert!(expected > 2);
                        assert!(
                            actual * 10 >= expected * 7 && actual * 10 <= expected * 13 + 100,
                            "glyph {count} vertical={vertical}: CPU {expected} GPU {actual}"
                        );
                    }
                    count += 1;
                }
            }
        }
        assert_eq!(count, 8);
    }
}
