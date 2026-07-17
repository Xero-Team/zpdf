use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use zpdf::{ConversionMode, ConversionOptions, ConvertedDocument};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Text,
    Markdown,
    Html,
}

#[derive(Debug, Clone)]
enum PageSelection {
    All,
    Selected(Vec<usize>),
}

#[derive(Debug, Clone)]
struct ConvertArgs {
    input: PathBuf,
    output: Option<PathBuf>,
    images_dir: Option<PathBuf>,
    mode: ConversionMode,
    format: Option<OutputFormat>,
    pages: PageSelection,
    use_structure: bool,
}

const USAGE: &str = "Usage: zpdf convert <file.pdf> [-o <output.txt|output.md|output.html>] \
    [--mode text|rich] [--format txt|md|html] [-p <page>|--pages <list>|--all] \
    [--struct] [--images-dir <dir>] [--password <pw>]";

pub(crate) fn run(args: &[String]) -> zpdf::Result<()> {
    let (args, password) = super::extract_password(args);
    let parsed = parse_args(&args).unwrap_or_else(|error| {
        eprintln!("{error}\n{USAGE}");
        std::process::exit(2);
    });

    let format = parsed.format.unwrap_or_else(|| {
        infer_format(parsed.output.as_deref()).unwrap_or(match parsed.mode {
            ConversionMode::TextOnly => OutputFormat::Text,
            ConversionMode::Rich => OutputFormat::Markdown,
        })
    });
    if parsed.mode == ConversionMode::Rich && format == OutputFormat::Text {
        eprintln!(
            "rich conversion requires --format md or html because TXT cannot reference images"
        );
        std::process::exit(2);
    }
    if parsed.mode == ConversionMode::TextOnly && parsed.images_dir.is_some() {
        eprintln!("--images-dir is only valid with --mode rich");
        std::process::exit(2);
    }

    let output = parsed.output.clone().unwrap_or_else(|| {
        parsed.input.with_extension(match format {
            OutputFormat::Text => "txt",
            OutputFormat::Markdown => "md",
            OutputFormat::Html => "html",
        })
    });
    if parsed.input == output {
        eprintln!("output path must differ from the input PDF");
        std::process::exit(2);
    }

    let input_string = parsed.input.to_string_lossy();
    let doc = super::open_document(&input_string, password.as_deref())?;
    let pages = selected_pages(&parsed.pages, doc.page_count()).unwrap_or_else(|error| {
        eprintln!("{error}");
        std::process::exit(2);
    });
    let converted = zpdf::convert_pdf(
        &doc,
        &pages,
        ConversionOptions {
            mode: parsed.mode,
            use_structure: parsed.use_structure,
        },
    )?;
    if parsed.use_structure && !converted.structure_order_used {
        eprintln!("no usable structure tree; used geometric reading order");
    }

    match format {
        OutputFormat::Text => write_text(&output, &converted)?,
        OutputFormat::Markdown => write_markdown(
            &output,
            parsed.images_dir.as_deref(),
            &parsed.input,
            converted,
            parsed.mode,
        )?,
        OutputFormat::Html => write_html(
            &output,
            parsed.images_dir.as_deref(),
            &parsed.input,
            converted,
            parsed.mode,
        )?,
    }

    println!("Converted {} page(s) to {}", pages.len(), output.display());
    Ok(())
}

fn parse_args(args: &[String]) -> Result<ConvertArgs, String> {
    if args.is_empty() {
        return Err("missing input PDF".to_string());
    }

    let mut input = None;
    let mut output = None;
    let mut images_dir = None;
    let mut mode = ConversionMode::TextOnly;
    let mut format = None;
    let mut pages = None;
    let mut use_structure = false;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "-o" | "--output" => {
                output = Some(PathBuf::from(required_value(args, i, &args[i])?));
                i += 2;
            }
            "--images-dir" => {
                images_dir = Some(PathBuf::from(required_value(args, i, "--images-dir")?));
                i += 2;
            }
            "--mode" => {
                mode = match required_value(args, i, "--mode")? {
                    "text" | "text-only" => ConversionMode::TextOnly,
                    "rich" => ConversionMode::Rich,
                    value => return Err(format!("unknown conversion mode: {value}")),
                };
                i += 2;
            }
            "--format" => {
                format = Some(match required_value(args, i, "--format")? {
                    "txt" | "text" => OutputFormat::Text,
                    "md" | "markdown" => OutputFormat::Markdown,
                    "html" | "htm" => OutputFormat::Html,
                    value => return Err(format!("unknown output format: {value}")),
                });
                i += 2;
            }
            "-p" | "--page" => {
                ensure_no_page_selection(&pages)?;
                let value = required_value(args, i, &args[i])?;
                let page = value
                    .parse::<usize>()
                    .ok()
                    .filter(|page| *page > 0)
                    .ok_or_else(|| format!("{} requires a positive page number", args[i]))?;
                pages = Some(PageSelection::Selected(vec![page - 1]));
                i += 2;
            }
            "--pages" => {
                ensure_no_page_selection(&pages)?;
                let value = required_value(args, i, "--pages")?;
                let selected = super::parse_page_list(value)
                    .map_err(|error| format!("invalid page list: {error}"))?;
                pages = Some(PageSelection::Selected(selected));
                i += 2;
            }
            "--all" => {
                ensure_no_page_selection(&pages)?;
                pages = Some(PageSelection::All);
                i += 1;
            }
            "--struct" => {
                use_structure = true;
                i += 1;
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown convert option: {value}"));
            }
            value => {
                if input.is_some() {
                    return Err(format!("unexpected positional argument: {value}"));
                }
                input = Some(PathBuf::from(value));
                i += 1;
            }
        }
    }

    Ok(ConvertArgs {
        input: input.ok_or_else(|| "missing input PDF".to_string())?,
        output,
        images_dir,
        mode,
        format,
        pages: pages.unwrap_or(PageSelection::All),
        use_structure,
    })
}

fn required_value<'a>(args: &'a [String], index: usize, flag: &str) -> Result<&'a str, String> {
    args.get(index + 1)
        .filter(|value| !value.starts_with('-'))
        .map(String::as_str)
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn ensure_no_page_selection(selection: &Option<PageSelection>) -> Result<(), String> {
    if selection.is_some() {
        Err("-p, --pages, and --all are mutually exclusive".to_string())
    } else {
        Ok(())
    }
}

fn infer_format(output: Option<&Path>) -> Option<OutputFormat> {
    match output
        .and_then(Path::extension)
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("txt") => Some(OutputFormat::Text),
        Some("md" | "markdown") => Some(OutputFormat::Markdown),
        Some("html" | "htm") => Some(OutputFormat::Html),
        _ => None,
    }
}

fn selected_pages(selection: &PageSelection, page_count: usize) -> Result<Vec<usize>, String> {
    let candidates: Vec<usize> = match selection {
        PageSelection::All => (0..page_count).collect(),
        PageSelection::Selected(pages) => pages.clone(),
    };
    let mut seen = HashSet::new();
    let mut result = Vec::with_capacity(candidates.len());
    for page in candidates {
        if page >= page_count {
            return Err(format!(
                "page {} is out of range; document has {page_count} page(s)",
                page + 1
            ));
        }
        if seen.insert(page) {
            result.push(page);
        }
    }
    Ok(result)
}

fn write_text(output: &Path, converted: &ConvertedDocument) -> zpdf::Result<()> {
    let mut text = String::new();
    for (index, page) in converted.pages.iter().enumerate() {
        if index > 0 {
            text.push_str("\n\n");
        }
        text.push_str(&page.text);
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    std::fs::write(output, text).map_err(zpdf::Error::Io)
}

fn write_markdown(
    output: &Path,
    images_dir: Option<&Path>,
    input: &Path,
    mut converted: ConvertedDocument,
    mode: ConversionMode,
) -> zpdf::Result<()> {
    let rich = mode == ConversionMode::Rich;
    let fallback_title = input
        .file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("PDF document");
    let title = converted
        .info
        .as_ref()
        .and_then(|info| info.title.as_deref())
        .or_else(|| converted.xmp.as_ref().and_then(|xmp| xmp.title.as_deref()))
        .unwrap_or(fallback_title);

    let output_parent = output.parent().filter(|path| !path.as_os_str().is_empty());
    let default_assets = format!(
        "{}_assets",
        output
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("zpdf")
    );
    let link_dir = images_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(&default_assets));
    let disk_dir = if link_dir.is_absolute() {
        link_dir.clone()
    } else {
        output_parent
            .unwrap_or_else(|| Path::new("."))
            .join(&link_dir)
    };
    let has_images = rich && converted.pages.iter().any(|page| !page.images.is_empty());
    let can_write_images = if has_images {
        match std::fs::create_dir_all(&disk_dir) {
            Ok(()) => true,
            Err(error) => {
                eprintln!(
                    "could not create image directory {}: {error}; continuing with text",
                    disk_dir.display()
                );
                false
            }
        }
    } else {
        false
    };

    let mut markdown = String::new();
    if rich {
        writeln!(markdown, "# {}\n", escape_markdown(title)).unwrap();
        append_markdown_metadata(&mut markdown, &converted);
    }

    for page in &mut converted.pages {
        if rich {
            write!(markdown, "## Page {}", page.index + 1).unwrap();
            if let Some(label) = &page.label {
                write!(markdown, " ({})", escape_markdown(label)).unwrap();
            }
            writeln!(markdown, "\n").unwrap();
            writeln!(
                markdown,
                "_Size: {:.2} x {:.2} pt; rotation: {} degrees._\n",
                page.width_points, page.height_points, page.rotation
            )
            .unwrap();
        } else {
            writeln!(markdown, "## Page {}\n", page.index + 1).unwrap();
        }

        if !page.text.is_empty() {
            markdown.push_str(&escape_markdown(&page.text));
            markdown.push_str("\n\n");
        }

        if rich && can_write_images && !page.images.is_empty() {
            let section_start = markdown.len();
            markdown.push_str("### Images\n\n");
            let mut saved = 0usize;
            for (image_index, converted_image) in page.images.iter_mut().enumerate() {
                let filename = format!(
                    "page-{:04}-image-{:03}.png",
                    page.index + 1,
                    image_index + 1
                );
                let path = disk_dir.join(&filename);
                if let Err(error) = save_image(&path, &mut converted_image.image) {
                    eprintln!(
                        "could not export image on page {}: {error}; continuing with text",
                        page.index + 1
                    );
                    continue;
                }
                let destination = asset_destination(&link_dir.join(&filename));
                writeln!(
                    markdown,
                    "![Page {} image {}]({destination})\n",
                    page.index + 1,
                    image_index + 1
                )
                .unwrap();
                writeln!(
                    markdown,
                    "_{} x {} px; {} placement(s)._\n",
                    converted_image.image.width,
                    converted_image.image.height,
                    converted_image.placements.len()
                )
                .unwrap();
                saved += 1;
            }
            if saved == 0 {
                markdown.truncate(section_start);
            }
        }
    }

    std::fs::write(output, markdown).map_err(zpdf::Error::Io)
}

fn write_html(
    output: &Path,
    images_dir: Option<&Path>,
    input: &Path,
    mut converted: ConvertedDocument,
    mode: ConversionMode,
) -> zpdf::Result<()> {
    let rich = mode == ConversionMode::Rich;
    let fallback_title = input
        .file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("PDF document");
    let title = converted
        .info
        .as_ref()
        .and_then(|info| info.title.as_deref())
        .or_else(|| converted.xmp.as_ref().and_then(|xmp| xmp.title.as_deref()))
        .unwrap_or(fallback_title);

    let output_parent = output.parent().filter(|path| !path.as_os_str().is_empty());
    let default_assets = format!(
        "{}_assets",
        output
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("zpdf")
    );
    let link_dir = images_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(&default_assets));
    let disk_dir = if link_dir.is_absolute() {
        link_dir.clone()
    } else {
        output_parent
            .unwrap_or_else(|| Path::new("."))
            .join(&link_dir)
    };
    let has_images = rich && converted.pages.iter().any(|page| !page.images.is_empty());
    let can_write_images = if has_images {
        match std::fs::create_dir_all(&disk_dir) {
            Ok(()) => true,
            Err(error) => {
                eprintln!(
                    "could not create image directory {}: {error}; continuing with text",
                    disk_dir.display()
                );
                false
            }
        }
    } else {
        false
    };

    let mut html = String::new();
    html.push_str("<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n");
    html.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    writeln!(html, "<title>{}</title>", escape_html(title)).unwrap();
    html.push_str(
        "<style>\n\
        :root{color-scheme:light dark;font-family:system-ui,sans-serif}\n\
        body{margin:0;background:#eee;color:#222}\n\
        main{max-width:960px;margin:auto;padding:2rem}\n\
        header,.page{background:#fff;margin:0 0 1.5rem;padding:1.5rem;box-shadow:0 1px 4px #0002}\n\
        h1,h2,h3{margin-top:0}.page-meta,figcaption{color:#666}\n\
        table{border-collapse:collapse;width:100%}th,td{border:1px solid #bbb;padding:.4rem;text-align:left;vertical-align:top}\n\
        pre.text{white-space:pre-wrap;overflow-wrap:anywhere;font:inherit;line-height:1.45;margin-bottom:0}\n\
        figure{margin:1rem 0}img{display:block;max-width:100%;height:auto}\n\
        @media(prefers-color-scheme:dark){body{background:#181818;color:#eee}header,.page{background:#242424}th,td{border-color:#555}.page-meta,figcaption{color:#bbb}}\n\
        </style>\n</head>\n<body>\n<main>\n",
    );

    if rich {
        writeln!(html, "<header>\n<h1>{}</h1>", escape_html(title)).unwrap();
        append_html_metadata(&mut html, &converted);
        html.push_str("</header>\n");
    }

    for page in &mut converted.pages {
        writeln!(
            html,
            "<section class=\"page\" data-page=\"{}\">",
            page.index + 1
        )
        .unwrap();
        write!(html, "<h2>Page {}", page.index + 1).unwrap();
        if rich {
            if let Some(label) = &page.label {
                write!(html, " ({})", escape_html(label)).unwrap();
            }
        }
        html.push_str("</h2>\n");
        if rich {
            writeln!(
                html,
                "<p class=\"page-meta\">Size: {:.2} × {:.2} pt; rotation: {} degrees.</p>",
                page.width_points, page.height_points, page.rotation
            )
            .unwrap();
        }
        if !page.text.is_empty() {
            writeln!(
                html,
                "<pre class=\"text\">{}</pre>",
                escape_html(&page.text)
            )
            .unwrap();
        }

        if rich && can_write_images && !page.images.is_empty() {
            let section_start = html.len();
            html.push_str("<section class=\"images\">\n<h3>Images</h3>\n");
            let mut saved = 0usize;
            for (image_index, converted_image) in page.images.iter_mut().enumerate() {
                let filename = format!(
                    "page-{:04}-image-{:03}.png",
                    page.index + 1,
                    image_index + 1
                );
                let path = disk_dir.join(&filename);
                if let Err(error) = save_image(&path, &mut converted_image.image) {
                    eprintln!(
                        "could not export image on page {}: {error}; continuing with text",
                        page.index + 1
                    );
                    continue;
                }
                let destination = escape_html(&asset_destination(&link_dir.join(&filename)));
                writeln!(
                    html,
                    "<figure><img src=\"{destination}\" alt=\"Page {} image {}\" loading=\"lazy\">",
                    page.index + 1,
                    image_index + 1
                )
                .unwrap();
                writeln!(
                    html,
                    "<figcaption>{} × {} px; {} placement(s).</figcaption></figure>",
                    converted_image.image.width,
                    converted_image.image.height,
                    converted_image.placements.len()
                )
                .unwrap();
                saved += 1;
            }
            if saved == 0 {
                html.truncate(section_start);
            } else {
                html.push_str("</section>\n");
            }
        }
        html.push_str("</section>\n");
    }

    html.push_str("</main>\n</body>\n</html>\n");
    std::fs::write(output, html).map_err(zpdf::Error::Io)
}

fn append_markdown_metadata(markdown: &mut String, converted: &ConvertedDocument) {
    markdown.push_str("| Field | Value |\n| --- | --- |\n");
    for (field, value) in metadata_entries(converted) {
        markdown_metadata_row(markdown, field, &value);
    }
    markdown.push('\n');
}

fn append_html_metadata(html: &mut String, converted: &ConvertedDocument) {
    html.push_str(
        "<table class=\"metadata\"><thead><tr><th>Field</th><th>Value</th></tr></thead><tbody>\n",
    );
    for (field, value) in metadata_entries(converted) {
        writeln!(
            html,
            "<tr><th>{}</th><td>{}</td></tr>",
            escape_html(field),
            escape_html(&value).replace('\n', "<br>")
        )
        .unwrap();
    }
    html.push_str("</tbody></table>\n");
}

fn metadata_entries(converted: &ConvertedDocument) -> Vec<(&'static str, String)> {
    let (major, minor) = converted.pdf_version;
    let mut entries = vec![
        ("PDF version", format!("{major}.{minor}")),
        ("Pages", converted.total_pages.to_string()),
        (
            "Reading order",
            if converted.structure_order_used {
                "Tagged PDF structure".to_string()
            } else {
                "Geometric".to_string()
            },
        ),
    ];

    let info = converted.info.as_ref();
    let xmp = converted.xmp.as_ref();
    let author = info.and_then(|value| value.author.clone()).or_else(|| {
        xmp.filter(|value| !value.creators.is_empty())
            .map(|value| value.creators.join(", "))
    });
    let subject = info
        .and_then(|value| value.subject.clone())
        .or_else(|| xmp.and_then(|value| value.description.clone()));
    let keywords = info
        .and_then(|value| value.keywords.clone())
        .or_else(|| xmp.and_then(|value| value.keywords.clone()))
        .or_else(|| {
            xmp.filter(|value| !value.subjects.is_empty())
                .map(|value| value.subjects.join(", "))
        });
    let creator = info
        .and_then(|value| value.creator.clone())
        .or_else(|| xmp.and_then(|value| value.creator_tool.clone()));
    let producer = info
        .and_then(|value| value.producer.clone())
        .or_else(|| xmp.and_then(|value| value.producer.clone()));
    let created = info
        .and_then(|value| value.creation_date.clone())
        .or_else(|| xmp.and_then(|value| value.create_date.clone()));
    let modified = info
        .and_then(|value| value.mod_date.clone())
        .or_else(|| xmp.and_then(|value| value.modify_date.clone()));

    for (field, value) in [
        ("Author", author),
        ("Subject", subject),
        ("Keywords", keywords),
        ("Creator", creator),
        ("Producer", producer),
        ("Created", created),
        ("Modified", modified),
    ] {
        if let Some(value) = value {
            entries.push((field, value));
        }
    }
    entries
}

fn markdown_metadata_row(markdown: &mut String, field: &str, value: &str) {
    let value = value
        .replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace(['\r', '\n'], "<br>");
    writeln!(markdown, "| {field} | {value} |").unwrap();
}

fn escape_markdown(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    let mut line_prefix = LinePrefix::Start;
    for character in value.chars() {
        if character == '\n' {
            escaped.push(character);
            line_prefix = LinePrefix::Start;
            continue;
        }

        let structural = match line_prefix {
            LinePrefix::Start => matches!(character, '#' | '>' | '+' | '-'),
            LinePrefix::Digits => matches!(character, '.' | ')'),
            LinePrefix::Body => false,
        };
        let inline = matches!(
            character,
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '|'
        );
        if structural || inline {
            escaped.push('\\');
        }
        escaped.push(character);

        line_prefix = match (line_prefix, character) {
            (LinePrefix::Start, value) if value.is_whitespace() => LinePrefix::Start,
            (LinePrefix::Start, value) if value.is_ascii_digit() => LinePrefix::Digits,
            (LinePrefix::Digits, value) if value.is_ascii_digit() => LinePrefix::Digits,
            _ => LinePrefix::Body,
        };
    }
    escaped
}

#[derive(Clone, Copy)]
enum LinePrefix {
    Start,
    Digits,
    Body,
}

fn asset_destination(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .replace('%', "%25")
        .replace(' ', "%20")
        .replace('#', "%23")
        .replace('?', "%3F")
        .replace('(', "%28")
        .replace(')', "%29")
        .replace('<', "%3C")
        .replace('>', "%3E")
}

fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

fn save_image(path: &Path, image: &mut zpdf::DecodedImage) -> Result<(), String> {
    let expected = (image.width as usize)
        .checked_mul(image.height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "image dimensions overflow".to_string())?;
    if image.data.len() != expected {
        return Err("decoded RGBA buffer size mismatch".to_string());
    }
    if image.premultiplied {
        unpremultiply_rgba(&mut image.data);
        image.premultiplied = false;
    }
    image::save_buffer(
        path,
        &image.data,
        image.width,
        image.height,
        image::ColorType::Rgba8,
    )
    .map_err(|error| format!("save {}: {error}", path.display()))
}

fn unpremultiply_rgba(data: &mut [u8]) {
    for pixel in data.chunks_exact_mut(4) {
        let alpha = u16::from(pixel[3]);
        if alpha == 0 {
            pixel[0] = 0;
            pixel[1] = 0;
            pixel[2] = 0;
            continue;
        }
        if alpha < 255 {
            for channel in &mut pixel[..3] {
                let straight = (u16::from(*channel) * 255 + alpha / 2) / alpha;
                *channel = straight.min(255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rich_markdown_and_page_ranges() {
        let args = [
            "input.pdf",
            "--mode",
            "rich",
            "--format",
            "md",
            "--pages",
            "1,3-4",
            "--struct",
            "-o",
            "out.md",
        ]
        .map(str::to_string);
        let parsed = parse_args(&args).expect("parse");
        assert_eq!(parsed.mode, ConversionMode::Rich);
        assert_eq!(parsed.format, Some(OutputFormat::Markdown));
        assert!(parsed.use_structure);
        assert!(matches!(
            parsed.pages,
            PageSelection::Selected(ref pages) if pages == &[0, 2, 3]
        ));
    }

    #[test]
    fn rejects_conflicting_page_selectors_and_unknown_flags() {
        let conflict = ["in.pdf", "-p", "1", "--all"].map(str::to_string);
        assert!(parse_args(&conflict).is_err());
        let unknown = ["in.pdf", "--typo"].map(str::to_string);
        assert!(parse_args(&unknown).is_err());
    }

    #[test]
    fn selected_pages_are_checked_and_deduplicated() {
        let pages = PageSelection::Selected(vec![0, 1, 0]);
        assert_eq!(selected_pages(&pages, 2).unwrap(), vec![0, 1]);
        assert!(selected_pages(&PageSelection::Selected(vec![2]), 2).is_err());
    }

    #[test]
    fn unpremultiplies_rgba_without_touching_alpha() {
        let mut rgba = vec![64, 32, 0, 128, 9, 8, 7, 0, 1, 2, 3, 255];
        unpremultiply_rgba(&mut rgba);
        assert_eq!(rgba, [128, 64, 0, 128, 0, 0, 0, 0, 1, 2, 3, 255]);
    }

    #[test]
    fn markdown_text_and_destinations_are_escaped() {
        assert_eq!(escape_markdown("# a_b"), "\\# a\\_b");
        assert_eq!(
            asset_destination(Path::new("assets dir/a(1).png")),
            "assets%20dir/a%281%29.png"
        );
    }

    #[test]
    fn parses_and_infers_html_output() {
        let args = ["input.pdf", "--mode", "rich", "--format", "html"].map(str::to_string);
        let parsed = parse_args(&args).expect("parse");
        assert_eq!(parsed.mode, ConversionMode::Rich);
        assert_eq!(parsed.format, Some(OutputFormat::Html));
        assert_eq!(
            infer_format(Some(Path::new("result.htm"))),
            Some(OutputFormat::Html)
        );
    }

    #[test]
    fn html_text_is_escaped() {
        assert_eq!(
            escape_html("<tag a=\"x\">Tom & 'Ada'</tag>"),
            "&lt;tag a=&quot;x&quot;&gt;Tom &amp; &#39;Ada&#39;&lt;/tag&gt;"
        );
    }
}
