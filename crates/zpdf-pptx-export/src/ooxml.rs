//! OOXML PowerPoint (.pptx) file generation.
//!
//! A .pptx file is a ZIP archive containing XML files following the Office Open XML standard.

use std::io::{Cursor, Write};
use zip::write::{FileOptions, ZipWriter};
use zip::CompressionMethod;

use crate::{PptxPresentation, PptxSlide, SlideElement};

/// Write a PowerPoint presentation to bytes.
pub struct PptxWriter {
    presentation: PptxPresentation,
}

impl PptxWriter {
    pub fn new(presentation: PptxPresentation) -> Self {
        Self { presentation }
    }

    /// Generate the .pptx file as bytes.
    pub fn write_to_bytes(&self) -> zpdf_core::Result<Vec<u8>> {
        let mut buffer = Cursor::new(Vec::new());
        let mut zip = ZipWriter::new(&mut buffer);

        let options: FileOptions<()> = FileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .unix_permissions(0o644);

        // Write [Content_Types].xml
        zip.start_file("[Content_Types].xml", options)
            .map_err(|e| zpdf_core::Error::Io(std::io::Error::other(e)))?;
        zip.write_all(self.generate_content_types().as_bytes())
            .map_err(zpdf_core::Error::Io)?;

        // Write _rels/.rels
        zip.start_file("_rels/.rels", options)
            .map_err(|e| zpdf_core::Error::Io(std::io::Error::other(e)))?;
        zip.write_all(self.generate_root_rels().as_bytes())
            .map_err(zpdf_core::Error::Io)?;

        // Write ppt/presentation.xml
        zip.start_file("ppt/presentation.xml", options)
            .map_err(|e| zpdf_core::Error::Io(std::io::Error::other(e)))?;
        zip.write_all(self.generate_presentation_xml().as_bytes())
            .map_err(zpdf_core::Error::Io)?;

        // Write ppt/_rels/presentation.xml.rels
        zip.start_file("ppt/_rels/presentation.xml.rels", options)
            .map_err(|e| zpdf_core::Error::Io(std::io::Error::other(e)))?;
        zip.write_all(self.generate_presentation_rels().as_bytes())
            .map_err(zpdf_core::Error::Io)?;

        // Write each slide
        for (idx, slide) in self.presentation.slides.iter().enumerate() {
            let slide_num = idx + 1;

            // Write ppt/slides/slide{N}.xml
            zip.start_file(format!("ppt/slides/slide{}.xml", slide_num), options)
                .map_err(|e| zpdf_core::Error::Io(std::io::Error::other(e)))?;
            zip.write_all(self.generate_slide_xml(slide, slide_num).as_bytes())
                .map_err(zpdf_core::Error::Io)?;

            // Write ppt/slides/_rels/slide{N}.xml.rels
            zip.start_file(
                format!("ppt/slides/_rels/slide{}.xml.rels", slide_num),
                options,
            )
            .map_err(|e| zpdf_core::Error::Io(std::io::Error::other(e)))?;
            zip.write_all(self.generate_slide_rels(slide).as_bytes())
                .map_err(zpdf_core::Error::Io)?;

            // Write embedded images
            for elem in &slide.elements {
                if let SlideElement::Image {
                    image_data,
                    image_id,
                    ..
                } = elem
                {
                    zip.start_file(format!("ppt/media/{}.png", image_id), options)
                        .map_err(|e| zpdf_core::Error::Io(std::io::Error::other(e)))?;
                    zip.write_all(image_data).map_err(zpdf_core::Error::Io)?;
                }
            }
        }

        zip.finish()
            .map_err(|e| zpdf_core::Error::Io(std::io::Error::other(e)))?;

        Ok(buffer.into_inner())
    }

    fn generate_content_types(&self) -> String {
        let mut xml = String::from(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Default Extension="png" ContentType="image/png"/>
  <Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
"#,
        );

        for i in 1..=self.presentation.slides.len() {
            xml.push_str(&format!(
                r#"  <Override PartName="/ppt/slides/slide{}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
"#,
                i
            ));
        }

        xml.push_str("</Types>");
        xml
    }

    fn generate_root_rels(&self) -> String {
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
</Relationships>"#.to_string()
    }

    fn generate_presentation_xml(&self) -> String {
        let mut xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:sldMasterIdLst/>
  <p:sldIdLst>
"#.to_string();

        for i in 1..=self.presentation.slides.len() {
            xml.push_str(&format!(
                r#"    <p:sldId id="{}" r:id="rId{}"/>
"#,
                255 + i,
                i
            ));
        }

        xml.push_str(&format!(
            r#"  </p:sldIdLst>
  <p:sldSz cx="{}" cy="{}"/>
  <p:notesSz cx="6858000" cy="9144000"/>
</p:presentation>"#,
            self.presentation.width_emu, self.presentation.height_emu
        ));
        xml
    }

    fn generate_presentation_rels(&self) -> String {
        let mut xml = String::from(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
"#,
        );

        for i in 1..=self.presentation.slides.len() {
            xml.push_str(&format!(
                r#"  <Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide{}.xml"/>
"#,
                i, i
            ));
        }

        xml.push_str("</Relationships>");
        xml
    }

    fn generate_slide_xml(&self, slide: &PptxSlide, _slide_num: usize) -> String {
        let mut xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr>
        <p:cNvPr id="1" name=""/>
        <p:cNvGrpSpPr/>
        <p:nvPr/>
      </p:nvGrpSpPr>
      <p:grpSpPr>
        <a:xfrm>
          <a:off x="0" y="0"/>
          <a:ext cx="0" cy="0"/>
          <a:chOff x="0" y="0"/>
          <a:chExt cx="0" cy="0"/>
        </a:xfrm>
      </p:grpSpPr>
"#.to_string();

        for (shape_id, elem) in (2..).zip(slide.elements.iter()) {
            xml.push_str(&self.generate_shape_xml(elem, shape_id));
        }

        xml.push_str(
            r#"    </p:spTree>
  </p:cSld>
  <p:clrMapOvr>
    <a:masterClrMapping/>
  </p:clrMapOvr>
</p:sld>"#,
        );
        xml
    }

    fn generate_shape_xml(&self, elem: &SlideElement, id: usize) -> String {
        match elem {
            SlideElement::TextBox {
                text,
                x_emu,
                y_emu,
                width_emu,
                height_emu,
                font_family,
                font_size_pt,
                color_rgb,
                bold,
                italic,
            } => {
                let color_hex =
                    format!("{:02X}{:02X}{:02X}", color_rgb.0, color_rgb.1, color_rgb.2);
                let font_size_hpt = (font_size_pt * 100.0) as i64; // hundredths of a point
                let bold_attr = if *bold { r#" b="1""# } else { "" };
                let italic_attr = if *italic { r#" i="1""# } else { "" };

                format!(
                    r#"      <p:sp>
        <p:nvSpPr>
          <p:cNvPr id="{}" name="TextBox {}"/>
          <p:cNvSpPr txBox="1"/>
          <p:nvPr/>
        </p:nvSpPr>
        <p:spPr>
          <a:xfrm>
            <a:off x="{}" y="{}"/>
            <a:ext cx="{}" cy="{}"/>
          </a:xfrm>
          <a:prstGeom prst="rect">
            <a:avLst/>
          </a:prstGeom>
          <a:noFill/>
        </p:spPr>
        <p:txBody>
          <a:bodyPr wrap="square" rtlCol="0">
            <a:spAutoFit/>
          </a:bodyPr>
          <a:lstStyle/>
          <a:p>
            <a:r>
              <a:rPr lang="en-US" sz="{}"{}{}/>
              <a:solidFill>
                <a:srgbClr val="{}"/>
              </a:solidFill>
              <a:latin typeface="{}"/>
              <a:t>{}</a:t>
            </a:r>
          </a:p>
        </p:txBody>
      </p:sp>
"#,
                    id,
                    id,
                    x_emu,
                    y_emu,
                    width_emu,
                    height_emu,
                    font_size_hpt,
                    bold_attr,
                    italic_attr,
                    color_hex,
                    escape_xml(font_family),
                    escape_xml(text)
                )
            }
            SlideElement::Rectangle {
                x_emu,
                y_emu,
                width_emu,
                height_emu,
                fill_rgb,
                stroke_rgb,
                stroke_width_pt,
            } => {
                let fill_xml = if let Some(rgb) = fill_rgb {
                    format!(
                        r#"          <a:solidFill>
            <a:srgbClr val="{:02X}{:02X}{:02X}"/>
          </a:solidFill>
"#,
                        rgb.0, rgb.1, rgb.2
                    )
                } else {
                    String::from("          <a:noFill/>\n")
                };

                let stroke_xml = if let Some(rgb) = stroke_rgb {
                    let width_emu = (stroke_width_pt * 12700.0) as i64;
                    format!(
                        r#"          <a:ln w="{}">
            <a:solidFill>
              <a:srgbClr val="{:02X}{:02X}{:02X}"/>
            </a:solidFill>
          </a:ln>
"#,
                        width_emu, rgb.0, rgb.1, rgb.2
                    )
                } else {
                    String::new()
                };

                format!(
                    r#"      <p:sp>
        <p:nvSpPr>
          <p:cNvPr id="{}" name="Rectangle {}"/>
          <p:cNvSpPr/>
          <p:nvPr/>
        </p:nvSpPr>
        <p:spPr>
          <a:xfrm>
            <a:off x="{}" y="{}"/>
            <a:ext cx="{}" cy="{}"/>
          </a:xfrm>
          <a:prstGeom prst="rect">
            <a:avLst/>
          </a:prstGeom>
{}{}        </p:spPr>
        <p:txBody>
          <a:bodyPr/>
          <a:lstStyle/>
          <a:p/>
        </p:txBody>
      </p:sp>
"#,
                    id, id, x_emu, y_emu, width_emu, height_emu, fill_xml, stroke_xml
                )
            }
            SlideElement::Ellipse {
                x_emu,
                y_emu,
                width_emu,
                height_emu,
                fill_rgb,
                stroke_rgb,
                stroke_width_pt,
            } => {
                let fill_xml = if let Some(rgb) = fill_rgb {
                    format!(
                        r#"          <a:solidFill>
            <a:srgbClr val="{:02X}{:02X}{:02X}"/>
          </a:solidFill>
"#,
                        rgb.0, rgb.1, rgb.2
                    )
                } else {
                    String::from("          <a:noFill/>\n")
                };

                let stroke_xml = if let Some(rgb) = stroke_rgb {
                    let width_emu = (stroke_width_pt * 12700.0) as i64;
                    format!(
                        r#"          <a:ln w="{}">
            <a:solidFill>
              <a:srgbClr val="{:02X}{:02X}{:02X}"/>
            </a:solidFill>
          </a:ln>
"#,
                        width_emu, rgb.0, rgb.1, rgb.2
                    )
                } else {
                    String::new()
                };

                format!(
                    r#"      <p:sp>
        <p:nvSpPr>
          <p:cNvPr id="{}" name="Ellipse {}"/>
          <p:cNvSpPr/>
          <p:nvPr/>
        </p:nvSpPr>
        <p:spPr>
          <a:xfrm>
            <a:off x="{}" y="{}"/>
            <a:ext cx="{}" cy="{}"/>
          </a:xfrm>
          <a:prstGeom prst="ellipse">
            <a:avLst/>
          </a:prstGeom>
{}{}        </p:spPr>
        <p:txBody>
          <a:bodyPr/>
          <a:lstStyle/>
          <a:p/>
        </p:txBody>
      </p:sp>
"#,
                    id, id, x_emu, y_emu, width_emu, height_emu, fill_xml, stroke_xml
                )
            }
            SlideElement::Line {
                x1_emu,
                y1_emu,
                x2_emu,
                y2_emu,
                stroke_rgb,
                stroke_width_pt,
            } => {
                let width_emu = (stroke_width_pt * 12700.0) as i64;
                format!(
                    r#"      <p:cxnSp>
        <p:nvCxnSpPr>
          <p:cNvPr id="{}" name="Line {}"/>
          <p:cNvCxnSpPr/>
          <p:nvPr/>
        </p:nvCxnSpPr>
        <p:spPr>
          <a:xfrm>
            <a:off x="{}" y="{}"/>
            <a:ext cx="{}" cy="{}"/>
          </a:xfrm>
          <a:prstGeom prst="line">
            <a:avLst/>
          </a:prstGeom>
          <a:ln w="{}">
            <a:solidFill>
              <a:srgbClr val="{:02X}{:02X}{:02X}"/>
            </a:solidFill>
          </a:ln>
        </p:spPr>
      </p:cxnSp>
"#,
                    id,
                    id,
                    x1_emu.min(x2_emu),
                    y1_emu.min(y2_emu),
                    (x2_emu - x1_emu).abs(),
                    (y2_emu - y1_emu).abs(),
                    width_emu,
                    stroke_rgb.0,
                    stroke_rgb.1,
                    stroke_rgb.2
                )
            }
            SlideElement::Image {
                x_emu,
                y_emu,
                width_emu,
                height_emu,
                image_id,
                ..
            } => {
                format!(
                    r#"      <p:pic>
        <p:nvPicPr>
          <p:cNvPr id="{}" name="Picture {}"/>
          <p:cNvPicPr>
            <a:picLocks noChangeAspect="1"/>
          </p:cNvPicPr>
          <p:nvPr/>
        </p:nvPicPr>
        <p:blipFill>
          <a:blip r:embed="rId_{}"/>
          <a:stretch>
            <a:fillRect/>
          </a:stretch>
        </p:blipFill>
        <p:spPr>
          <a:xfrm>
            <a:off x="{}" y="{}"/>
            <a:ext cx="{}" cy="{}"/>
          </a:xfrm>
          <a:prstGeom prst="rect">
            <a:avLst/>
          </a:prstGeom>
        </p:spPr>
      </p:pic>
"#,
                    id, id, image_id, x_emu, y_emu, width_emu, height_emu
                )
            }
            SlideElement::FreeformPath { .. } => {
                // Complex paths not yet implemented
                String::new()
            }
        }
    }

    fn generate_slide_rels(&self, slide: &PptxSlide) -> String {
        let mut xml = String::from(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
"#,
        );

        for elem in &slide.elements {
            if let SlideElement::Image { image_id, .. } = elem {
                xml.push_str(&format!(
                    r#"  <Relationship Id="rId_{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/{}.png"/>
"#,
                    image_id, image_id
                ));
            }
        }

        xml.push_str("</Relationships>");
        xml
    }
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_xml_entities() {
        assert_eq!(escape_xml("a<b&c>d"), "a&lt;b&amp;c&gt;d");
    }
}
