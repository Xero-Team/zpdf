use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process;

fn main() {
    // RUST_LOG-controlled diagnostics (e.g. RUST_LOG=zpdf_font=warn). Defaults to
    // silent so normal CLI output stays clean.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: zpdf <command> [args...]");
        eprintln!(
            "Commands: info, dump, render, text, tables, forms, outline, links, struct, signatures, attachments, compare, debug-stream, fill, pages, set-meta, stamp"
        );
        process::exit(1);
    }

    let result = match args[1].as_str() {
        "info" => cmd_info(&args[2..]),
        "dump" => cmd_dump(&args[2..]),
        "render" => cmd_render(&args[2..]),
        "text" => cmd_text(&args[2..]),
        "tables" => cmd_tables(&args[2..]),
        "forms" => cmd_forms(&args[2..]),
        "outline" => cmd_outline(&args[2..]),
        "links" => cmd_links(&args[2..]),
        "struct" => cmd_struct(&args[2..]),
        "signatures" => cmd_signatures(&args[2..]),
        "attachments" => cmd_attachments(&args[2..]),
        "compare" => cmd_compare(&args[2..]),
        "debug-stream" => cmd_debug_stream(&args[2..]),
        "fill" => cmd_fill(&args[2..]),
        "pages" => cmd_pages(&args[2..]),
        "set-meta" => cmd_set_meta(&args[2..]),
        "stamp" => cmd_stamp(&args[2..]),
        other => {
            eprintln!("Unknown command: {other}");
            process::exit(1);
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {e}");
        process::exit(1);
    }
}

/// Pull an optional `--password <pw>` out of an argument list, returning the
/// remaining args and the password. Lets every document command accept it
/// uniformly without each flag loop having to know about it.
fn extract_password(args: &[String]) -> (Vec<String>, Option<String>) {
    let mut rest = Vec::new();
    let mut password = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--password" {
            // Require a value that is not itself a flag, so a forgotten value
            // (`--password -o out.png`) does not silently swallow the next flag.
            match args.get(i + 1) {
                Some(v) if !v.starts_with('-') => {
                    password = Some(v.clone());
                    i += 2;
                }
                _ => {
                    eprintln!("--password requires a value (a password not starting with '-')");
                    process::exit(2);
                }
            }
        } else {
            rest.push(args[i].clone());
            i += 1;
        }
    }
    (rest, password)
}

/// Read and open a PDF, optionally with a decryption password.
fn open_document(path: &str, password: Option<&str>) -> zpdf::Result<zpdf::PdfDocument> {
    let data = fs::read(path).map_err(zpdf::Error::Io)?;
    match password {
        Some(pw) => zpdf::PdfDocument::open_with_password(data, pw.as_bytes()),
        None => zpdf::PdfDocument::open(data),
    }
}

fn cmd_info(args: &[String]) -> zpdf::Result<()> {
    let (args, password) = extract_password(args);
    if args.is_empty() {
        eprintln!("Usage: zpdf info <file.pdf> [--password <pw>]");
        process::exit(1);
    }

    let doc = open_document(&args[0], password.as_deref())?;
    let (major, minor) = doc.version();

    println!("File: {}", args[0]);
    println!("Version: PDF-{major}.{minor}");
    println!("Pages: {}", doc.page_count());

    // Cap the per-page listing: a fuzzed/huge document can carry hundreds of
    // thousands of pages, and resolving + printing each (a fresh inheritance
    // walk per page) would make `info` appear to hang. The first N is enough to
    // characterize the file.
    const MAX_LISTED_PAGES: usize = 1000;
    let listed = doc.page_count().min(MAX_LISTED_PAGES);
    // Parse the page-label number tree once, then annotate each listed page with
    // its printed label (e.g. "iv", "A-2") when the document defines labels.
    let page_labels = doc.page_labels();
    for i in 0..listed {
        if let Ok(page) = doc.page(i) {
            let label = page_labels
                .as_ref()
                .and_then(|pl| pl.label(i))
                .filter(|l| !l.is_empty());
            let label_suffix = match label {
                Some(l) => format!(", label: {l}"),
                None => String::new(),
            };
            println!(
                "  Page {}: {:.0} x {:.0} pt (rotate: {}{})",
                i + 1,
                page.width(),
                page.height(),
                page.rotate,
                label_suffix,
            );
        }
    }
    if doc.page_count() > listed {
        println!("  ... and {} more pages", doc.page_count() - listed);
    }

    if let Some(meta) = doc.info() {
        println!("Metadata:");
        let field = |label: &str, value: &Option<String>| {
            if let Some(v) = value {
                println!("  {label}: {v}");
            }
        };
        field("Title", &meta.title);
        field("Author", &meta.author);
        field("Subject", &meta.subject);
        field("Keywords", &meta.keywords);
        field("Creator", &meta.creator);
        field("Producer", &meta.producer);
        field("Created", &meta.creation_date);
        field("Modified", &meta.mod_date);
        field("Trapped", &meta.trapped);
    }

    if let Some(xmp) = doc.xmp_metadata() {
        println!("XMP Metadata:");
        let field = |label: &str, value: &Option<String>| {
            if let Some(v) = value {
                println!("  {label}: {v}");
            }
        };
        let list = |label: &str, values: &[String]| {
            if !values.is_empty() {
                println!("  {label}: {}", values.join(", "));
            }
        };
        field("Title", &xmp.title);
        list("Creators", &xmp.creators);
        field("Description", &xmp.description);
        list("Subjects", &xmp.subjects);
        field("Keywords", &xmp.keywords);
        field("Creator Tool", &xmp.creator_tool);
        field("Producer", &xmp.producer);
        field("Created", &xmp.create_date);
        field("Modified", &xmp.modify_date);
    }

    let outline = doc.outline();
    if !outline.is_empty() {
        let total = count_outline(&outline);
        println!(
            "Outline: {} top-level bookmark(s), {total} total",
            outline.len()
        );
    }

    // Logical structure / Tagged PDF. Report tagged-ness, and the structure tree
    // summary when present (use `zpdf struct` for the full tree).
    if doc.is_tagged() {
        println!("Tagged PDF: yes (/MarkInfo /Marked)");
    }
    if let Some(tree) = doc.struct_tree() {
        println!(
            "Structure tree: {} element(s), {} top-level",
            tree.element_count(),
            tree.children.len()
        );
    }

    let intents = doc.output_intents();
    if !intents.is_empty() {
        println!("Output Intents: {}", intents.len());
        for (i, oi) in intents.iter().enumerate() {
            let subtype = if oi.subtype.is_empty() {
                "(none)"
            } else {
                &oi.subtype
            };
            let condition = oi
                .output_condition_identifier
                .as_deref()
                .unwrap_or("(none)");
            let profile = match oi.dest_output_profile {
                Some(id) => {
                    let n = oi
                        .dest_profile_components
                        .map_or_else(|| "?".to_string(), |n| n.to_string());
                    // `has_cmyk_profile` mirrors the render-time gate: an /N-absent
                    // embedded profile may still resolve to CMYK and be managed.
                    let managed = if oi.has_cmyk_profile() {
                        " (DeviceCMYK colour-managed)"
                    } else {
                        ""
                    };
                    format!("DestOutputProfile {id} N={n}{managed}")
                }
                None => "external profile".to_string(),
            };
            println!(
                "  [{}] /S {subtype} | OutputConditionIdentifier: {condition} | {profile}",
                i + 1
            );
        }
    }

    let embedded = doc.embedded_files();
    let embedded_streams: HashSet<zpdf::ObjectId> =
        embedded.iter().filter_map(|e| e.stream).collect();
    if !embedded.is_empty() {
        println!("Embedded files: {}", embedded.len());
        for ef in &embedded {
            println!("  {}", describe_embedded_file(ef));
        }
    }
    let associated = doc.associated_files();
    if !associated.is_empty() {
        println!("Associated files (PDF 2.0): {}", associated.len());
        for af in &associated {
            // A PDF 2.0 associated file is normally also in the name tree above;
            // flag the overlap so the two counts are not misread as distinct files.
            let also = match af.stream {
                Some(id) if embedded_streams.contains(&id) => "  (also listed above)",
                _ => "",
            };
            println!("  {}{also}", describe_embedded_file(af));
        }
    }

    Ok(())
}

/// One-line description of an embedded/associated file for listings: name, then
/// the metadata that is actually present (relationship, MIME subtype, size).
fn describe_embedded_file(ef: &zpdf::EmbeddedFile) -> String {
    let name = if ef.name.is_empty() {
        "(unnamed)"
    } else {
        &ef.name
    };
    let mut parts = Vec::new();
    if let Some(rel) = &ef.relationship {
        parts.push(format!("rel={rel}"));
    }
    if let Some(st) = &ef.subtype {
        parts.push(st.clone());
    }
    if let Some(sz) = ef.size {
        parts.push(format!("{sz} bytes"));
    }
    if !ef.is_embedded() {
        parts.push("external (no /EF)".to_string());
    }
    if parts.is_empty() {
        name.to_string()
    } else {
        format!("{name}  [{}]", parts.join(", "))
    }
}

/// List the document's interactive-form (AcroForm) fields, types, and values.
fn cmd_forms(args: &[String]) -> zpdf::Result<()> {
    let (args, password) = extract_password(args);
    if args.is_empty() {
        eprintln!("Usage: zpdf forms <file.pdf> [--password <pw>]");
        process::exit(1);
    }

    let doc = open_document(&args[0], password.as_deref())?;

    let Some(form) = doc.acro_form() else {
        println!("No AcroForm (no interactive form fields).");
        return Ok(());
    };

    println!(
        "AcroForm: {} field(s), NeedAppearances: {}",
        form.fields.len(),
        form.need_appearances
    );
    for f in &form.fields {
        let value = match &f.value {
            Some(zpdf::FieldValue::Text(s)) => format!(" = {s:?}"),
            Some(zpdf::FieldValue::Name(n)) => format!(" = /{n}"),
            Some(zpdf::FieldValue::List(v)) => format!(" = {v:?}"),
            None => String::new(),
        };
        let flags = if f.flags != 0 {
            format!(" (Ff {:#x})", f.flags)
        } else {
            String::new()
        };
        println!("  [{}] {}{}{}", f.kind.as_str(), f.name, value, flags);
    }

    Ok(())
}

/// Print the document outline (bookmarks) as an indented tree, each line ending
/// in its resolved target (`p.<N>` for an in-document page, `uri:<…>` for a
/// link). Bookmarks with no resolvable target print just their title.
fn cmd_outline(args: &[String]) -> zpdf::Result<()> {
    let (args, password) = extract_password(args);
    if args.is_empty() {
        eprintln!("Usage: zpdf outline <file.pdf> [--password <pw>]");
        process::exit(1);
    }

    let doc = open_document(&args[0], password.as_deref())?;
    let outline = doc.outline();
    if outline.is_empty() {
        println!("No document outline (bookmarks).");
        return Ok(());
    }
    print_outline(&outline, 0);
    Ok(())
}

/// Recursively print outline items with two-space indentation per level.
fn print_outline(items: &[zpdf::OutlineItem], depth: usize) {
    for item in items {
        let indent = "  ".repeat(depth);
        let title = if item.title.is_empty() {
            "(untitled)"
        } else {
            &item.title
        };
        let target = match (&item.dest, &item.uri) {
            (Some(d), _) => match d.page {
                Some(p) => format!("  -> p.{}", p + 1),
                None => "  -> (external page)".to_string(),
            },
            (None, Some(uri)) => format!("  -> uri:{uri}"),
            (None, None) => String::new(),
        };
        println!("{indent}{title}{target}");
        print_outline(&item.children, depth + 1);
    }
}

/// Total number of bookmarks in an outline tree (for the `info` summary).
fn count_outline(items: &[zpdf::OutlineItem]) -> usize {
    items.iter().map(|i| 1 + count_outline(&i.children)).sum()
}

/// List link annotations and their resolved targets, page by page. Each line
/// gives the link rectangle and where it points — an in-document page
/// (`-> p.<N>`), an external page reference (`-> (external page)`), or an
/// external URI / remote file (`-> uri:<…>`).
fn cmd_links(args: &[String]) -> zpdf::Result<()> {
    let (args, password) = extract_password(args);
    if args.is_empty() {
        eprintln!("Usage: zpdf links <file.pdf> [--password <pw>]");
        process::exit(1);
    }

    let doc = open_document(&args[0], password.as_deref())?;

    // Cap the page scan so a fuzzed/huge document can't make `links` hang
    // (each page re-parses its annotations); the first N characterizes the file.
    const MAX_SCANNED_PAGES: usize = 5000;
    let scanned = doc.page_count().min(MAX_SCANNED_PAGES);
    let mut found = 0usize;
    for i in 0..scanned {
        let Ok(page) = doc.page(i) else { continue };
        for a in doc.page_annotations(&page) {
            let target = match (&a.dest, &a.uri) {
                (Some(d), _) => match d.page {
                    Some(p) => format!("-> p.{}", p + 1),
                    None => "-> (external page)".to_string(),
                },
                (None, Some(uri)) => format!("-> uri:{uri}"),
                (None, None) => {
                    // Not a link/navigational annotation - but if it has measure info, show it
                    if let Some(measure) = &a.measure {
                        let mut info = format!("[Measure: {}]", measure.subtype);
                        if let Some(epsg) = measure.gcs.as_ref().and_then(|g| g.epsg) {
                            info.push_str(&format!(" EPSG:{}", epsg));
                        }
                        found += 1;
                        println!(
                            "Page {}: [{:.0} {:.0} {:.0} {:.0}] {}",
                            i + 1,
                            a.rect.x0,
                            a.rect.y0,
                            a.rect.x1,
                            a.rect.y1,
                            info
                        );
                    }
                    continue;
                }
            };
            found += 1;
            println!(
                "Page {}: [{:.0} {:.0} {:.0} {:.0}] {target}",
                i + 1,
                a.rect.x0,
                a.rect.y0,
                a.rect.x1,
                a.rect.y1,
            );
        }
    }
    if found == 0 {
        println!("No link annotations.");
    }
    if doc.page_count() > scanned {
        println!(
            "(scanned the first {scanned} of {} pages)",
            doc.page_count()
        );
    }
    Ok(())
}

/// Print the document's logical structure tree (Tagged PDF, ISO 32000-1
/// §14.7–14.8): the nested structure elements with their roles, page
/// associations, titles, and accessibility text. Marked-content (`mcid`) and
/// object (`obj`) leaves are shown under their owning element.
fn cmd_struct(args: &[String]) -> zpdf::Result<()> {
    let (args, password) = extract_password(args);
    if args.is_empty() {
        eprintln!("Usage: zpdf struct <file.pdf> [--password <pw>]");
        process::exit(1);
    }

    let doc = open_document(&args[0], password.as_deref())?;
    println!(
        "Tagged (/MarkInfo /Marked): {}",
        if doc.is_tagged() { "yes" } else { "no" }
    );
    match doc.struct_tree() {
        None => println!("No logical structure tree (/StructTreeRoot)."),
        Some(tree) => {
            println!(
                "Structure tree: {} element(s), {} top-level",
                tree.element_count(),
                tree.children.len()
            );
            for elem in &tree.children {
                print_struct_elem(elem, 0);
            }
        }
    }
    Ok(())
}

/// Recursively print a structure element and its kids, two-space indented per
/// level. An element line shows its role, optional title / accessibility text,
/// and page; marked-content and object kids are shown as `·` leaves.
fn print_struct_elem(elem: &zpdf::StructElem, depth: usize) {
    let indent = "  ".repeat(depth);
    let mut line = format!("{indent}{}", elem.role.as_str());
    if let Some(t) = &elem.title {
        line.push_str(&format!("  \"{}\"", truncate_display(t)));
    }
    if let Some(alt) = &elem.alt {
        line.push_str(&format!("  alt:\"{}\"", truncate_display(alt)));
    }
    if let Some(at) = &elem.actual_text {
        line.push_str(&format!("  text:\"{}\"", truncate_display(at)));
    }
    if let Some(p) = elem.page {
        line.push_str(&format!("  (p.{})", p + 1));
    }
    println!("{line}");

    let child_indent = "  ".repeat(depth + 1);
    for kid in &elem.kids {
        match kid {
            zpdf::StructKid::Element(e) => print_struct_elem(e, depth + 1),
            zpdf::StructKid::MarkedContent { page, mcid } => {
                let pg = page.map_or_else(String::new, |p| format!(" p.{}", p + 1));
                println!("{child_indent}· mcid {mcid}{pg}");
            }
            zpdf::StructKid::Object { page, obj } => {
                let pg = page.map_or_else(String::new, |p| format!(" p.{}", p + 1));
                println!("{child_indent}· obj {} {} R{pg}", obj.0, obj.1);
            }
        }
    }
}

/// List the document's digital signatures, with each signature's declared
/// metadata, its `/ByteRange` coverage, and the byte-range integrity verdict.
///
/// The verdict reports whether the *signed bytes are intact* (their digest
/// matches the one embedded in the CMS blob); it does NOT verify the signer's
/// cryptographic signature or certificate trust. The output labels this
/// explicitly so it is never read as full validation.
fn cmd_signatures(args: &[String]) -> zpdf::Result<()> {
    let (args, password) = extract_password(args);
    if args.is_empty() {
        eprintln!("Usage: zpdf signatures <file.pdf> [--password <pw>]");
        process::exit(1);
    }

    let doc = open_document(&args[0], password.as_deref())?;
    let sigs = doc.signatures();
    if sigs.is_empty() {
        println!("No digital signatures (/Sig fields).");
        return Ok(());
    }

    println!("Digital signatures: {}", sigs.len());
    println!(
        "(integrity = signed bytes match the CMS digest; signature = signer's public-key \
         signature verifies; neither validates certificate trust or revocation)"
    );
    for (i, s) in sigs.iter().enumerate() {
        println!("\n[{}] field: {}", i + 1, s.field_name);
        let field = |label: &str, value: &Option<String>| {
            if let Some(v) = value {
                println!("    {label}: {}", truncate_display(v));
            }
        };
        field("Signer (/Name)", &s.name);
        field("Signer CN (cert)", &s.signer_common_name);
        field("Reason", &s.reason);
        field("Location", &s.location);
        field("Contact", &s.contact_info);
        field("Signing time", &s.signing_time);
        field("Filter", &s.filter);
        field("SubFilter", &s.sub_filter);
        if let Some(alg) = &s.digest_algorithm {
            println!("    Digest algorithm: {alg}");
        }
        if let Some(alg) = &s.signature_algorithm {
            println!("    Signature algorithm: {alg}");
        }

        let cov = &s.coverage;
        println!(
            "    Coverage: {} span(s), whole document: {}{}",
            cov.ranges.len(),
            if cov.covers_whole_document {
                "yes"
            } else {
                "no"
            },
            if cov.bytes_after_signature > 0 {
                format!(
                    " ({} byte(s) added after signing — later incremental update)",
                    cov.bytes_after_signature
                )
            } else {
                String::new()
            }
        );

        let integrity = match s.digest {
            zpdf::DigestStatus::Verified => "VERIFIED — signed bytes are intact",
            zpdf::DigestStatus::Mismatch => "MISMATCH — signed bytes were altered",
            zpdf::DigestStatus::Unsupported => "unsupported — no comparable digest",
        };
        println!("    Integrity: {integrity}");

        let signature = match s.crypto {
            zpdf::CryptoStatus::Valid => {
                "VALID — signature verifies (certificate trust NOT checked)"
            }
            zpdf::CryptoStatus::Invalid => "INVALID — signature does not verify",
            zpdf::CryptoStatus::Unsupported => "unsupported — algorithm/key not verifiable",
        };
        println!("    Signature: {signature}");

        if s.is_cryptographically_valid() {
            println!("    => Cryptographically sound (bytes intact + signature valid); trust anchor NOT validated");
        }
    }

    Ok(())
}

/// Truncate a string for single-line display, appending `…` when shortened.
fn truncate_display(s: &str) -> String {
    const MAX: usize = 60;
    if s.chars().count() > MAX {
        let mut out: String = s.chars().take(MAX).collect();
        out.push('…');
        out
    } else {
        s.to_string()
    }
}

/// List the document's embedded & associated files, and optionally extract them
/// to disk. Extraction sanitizes file names (a `/UF` like `../../etc/passwd` is
/// reduced to its basename) so a malicious attachment cannot escape `--out-dir`,
/// and never overwrites an existing file — a colliding name gets a ` (n)` suffix.
fn cmd_attachments(args: &[String]) -> zpdf::Result<()> {
    let (args, password) = extract_password(args);
    if args.is_empty() {
        eprintln!(
            "Usage: zpdf attachments <file.pdf> [--extract <index|name|all>] [--out-dir <dir>] [--password <pw>]"
        );
        process::exit(1);
    }

    let pdf_path = &args[0];
    let mut extract: Option<String> = None;
    let mut out_dir = String::from(".");

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--extract" => {
                i += 1;
                extract = Some(args.get(i).cloned().unwrap_or_else(|| {
                    eprintln!("--extract requires a value (an index, a file name, or 'all')");
                    process::exit(2);
                }));
            }
            "--out-dir" => {
                i += 1;
                if let Some(s) = args.get(i) {
                    out_dir = s.clone();
                }
            }
            _ => {}
        }
        i += 1;
    }

    let doc = open_document(pdf_path, password.as_deref())?;
    let all = collect_attachments(&doc);

    if all.is_empty() {
        println!("No embedded or associated files.");
        return Ok(());
    }

    println!("{} embedded/associated file(s):", all.len());
    for (idx, ef) in all.iter().enumerate() {
        println!("  [{idx}] {}", describe_embedded_file(ef));
        if let Some(d) = &ef.description {
            println!("        description: {d}");
        }
        if let Some(c) = &ef.creation_date {
            println!("        created: {c}");
        }
        if let Some(m) = &ef.mod_date {
            println!("        modified: {m}");
        }
    }

    let Some(target) = extract else {
        return Ok(());
    };

    // `--extract` selects all files (`all`), one listing index, or every file
    // whose name matches exactly (so unnamed/duplicate files are reachable by
    // index, names by name).
    let by_index = target.parse::<usize>().ok().filter(|&n| n < all.len());

    fs::create_dir_all(&out_dir).map_err(zpdf::Error::Io)?;
    let mut extracted = 0usize;
    let mut matched = 0usize;
    for (idx, ef) in all.iter().enumerate() {
        let selected =
            target == "all" || by_index == Some(idx) || (by_index.is_none() && ef.name == target);
        if !selected {
            continue;
        }
        matched += 1;
        if !ef.is_embedded() {
            eprintln!(
                "  skip {:?}: external reference, no embedded bytes",
                ef.name
            );
            continue;
        }
        let bytes = match doc.embedded_file_bytes(ef) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("  skip {:?}: {e}", ef.name);
                continue;
            }
        };
        let base = sanitize_filename(&ef.name).unwrap_or_else(|| format!("attachment_{extracted}"));
        // create_unique never clobbers an existing file (atomic create_new),
        // which also folds run-internal uniqueness into the on-disk check.
        let (path, mut file) = match create_unique(&out_dir, &base) {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("  skip {:?}: {e}", ef.name);
                continue;
            }
        };
        if let Err(e) = file.write_all(&bytes) {
            eprintln!("  skip {:?}: {e}", ef.name);
            continue;
        }
        println!(
            "  extracted {:?} -> {} ({} bytes)",
            ef.name,
            path.display(),
            bytes.len()
        );
        extracted += 1;
    }

    if matched == 0 && target != "all" {
        eprintln!("No attachment matched {target:?}. Use --extract all to extract everything.");
        process::exit(1);
    }

    Ok(())
}

/// Pages scanned for page-level `/AF`, and the overall attachment cap — bound the
/// gather so an adversarial multi-page document cannot explode it.
const MAX_PAGES_SCANNED: usize = 1000;
const MAX_ATTACHMENTS: usize = 16_384;

/// Gather every embedded/associated file into one deduplicated list: name-tree
/// embedded files first, then catalog- and page-level associated files. Entries
/// are collapsed by embedded-stream object id (merging in the `/AF` relationship
/// and description from the associated-file copy), so a file present in both the
/// name tree and an `/AF` array — the PDF 2.0 norm — is reported once. A *named*
/// external (no-`/EF`) spec collapses with an identically-named one; unnamed
/// externals never merge. O(n) via hash indices and bounded against hostile input.
fn collect_attachments(doc: &zpdf::PdfDocument) -> Vec<zpdf::EmbeddedFile> {
    let mut all = doc.embedded_files();
    all.truncate(MAX_ATTACHMENTS);
    let mut stream_index: HashMap<(u32, u16), usize> = all
        .iter()
        .enumerate()
        .filter_map(|(i, e)| e.stream.map(|id| ((id.0, id.1), i)))
        .collect();
    let mut seen_named: HashSet<String> = all
        .iter()
        .filter(|e| e.stream.is_none() && !e.name.is_empty())
        .map(|e| e.name.clone())
        .collect();

    let mut associated = doc.associated_files();
    let pages = doc.page_count().min(MAX_PAGES_SCANNED);
    for pi in 0..pages {
        if associated.len() >= MAX_ATTACHMENTS {
            break;
        }
        if let Ok(page) = doc.page(pi) {
            associated.extend(doc.page_associated_files(&page));
        }
    }

    for af in associated {
        if all.len() >= MAX_ATTACHMENTS {
            break;
        }
        if let Some(id) = af.stream {
            let key = (id.0, id.1);
            if let Some(&idx) = stream_index.get(&key) {
                // Same embedded stream already listed: enrich missing metadata.
                let existing = &mut all[idx];
                if existing.relationship.is_none() {
                    existing.relationship = af.relationship.clone();
                }
                if existing.description.is_none() {
                    existing.description = af.description.clone();
                }
                continue;
            }
            stream_index.insert(key, all.len());
        } else if !af.name.is_empty() && !seen_named.insert(af.name.clone()) {
            continue;
        }
        all.push(af);
    }
    all
}

/// Reduce a PDF-declared file name to a safe, single-component output file name.
/// Strips directory components (defeating `../` traversal and absolute paths),
/// replaces path/Windows-reserved and control characters, strips trailing dots
/// and spaces (which Windows silently drops, otherwise re-enabling collisions),
/// and dodges Windows reserved device names. Returns `None` if nothing usable
/// remains.
fn sanitize_filename(name: &str) -> Option<String> {
    // Keep only the final path component — defeats `../` and absolute paths
    // regardless of which separator the producer used.
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name).trim();
    if base.is_empty() || base == "." || base == ".." {
        return None;
    }
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .take(200)
        .collect();
    // Windows strips trailing dots/spaces at create time, so "x.txt" and "x.txt."
    // would collide on disk despite being distinct strings; normalize them away.
    let cleaned = cleaned.trim_end_matches([' ', '.']);
    // An emptied name, or one of only dots/spaces/underscores, is hidden or
    // trims to nothing on some platforms; treat it as unusable.
    if cleaned.is_empty() || cleaned.chars().all(|c| matches!(c, '.' | ' ' | '_')) {
        return None;
    }
    // Windows reserved device names (apply to the stem, any extension).
    let stem = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    Some(if reserved {
        format!("_{cleaned}")
    } else {
        cleaned.to_string()
    })
}

/// Create a new file under `dir` named `base`, never overwriting an existing one:
/// on collision it tries `stem (1).ext`, `stem (2).ext`, … until an unused name
/// opens. `create_new` makes the existence check and creation one atomic step
/// (no TOCTOU; a pre-existing file in `dir` is preserved, and same-run name
/// collisions are disambiguated by the same mechanism).
fn create_unique(dir: &str, base: &str) -> std::io::Result<(std::path::PathBuf, fs::File)> {
    use std::io::ErrorKind;
    let (stem, ext) = match base.rfind('.') {
        Some(dot) if dot > 0 => (&base[..dot], &base[dot..]),
        _ => (base, ""),
    };
    for n in 0u64.. {
        let name = if n == 0 {
            base.to_string()
        } else {
            format!("{stem} ({n}){ext}")
        };
        let path = Path::new(dir).join(&name);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!("a u64 counter always finds a free name")
}

fn cmd_dump(args: &[String]) -> zpdf::Result<()> {
    let (args, password) = extract_password(args);
    if args.len() < 3 {
        eprintln!("Usage: zpdf dump <file.pdf> <obj_num> <gen_num> [--password <pw>]");
        process::exit(1);
    }

    let doc = open_document(&args[0], password.as_deref())?;

    let obj_num: u32 = args[1].parse().unwrap_or(0);
    let gen_num: u16 = args[2].parse().unwrap_or(0);
    let id = zpdf::ObjectId(obj_num, gen_num);

    let obj = doc.file().resolve(id)?;
    println!("{obj}");

    Ok(())
}

/// Pixel-compare two PNGs and report difference metrics. Serves as the
/// CPU↔reference (and, in Phase 3, GPU↔CPU) rendering comparison harness.
fn cmd_compare(args: &[String]) -> zpdf::Result<()> {
    if args.len() < 2 {
        eprintln!("Usage: zpdf compare <a.png> <b.png> [--out <diff.png>] [--threshold <0-255>]");
        process::exit(1);
    }
    let a_path = &args[0];
    let b_path = &args[1];
    let mut out_path: Option<String> = None;
    let mut threshold: u8 = 16;

    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                i += 1;
                out_path = args.get(i).cloned();
            }
            "--threshold" => {
                i += 1;
                threshold = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(16);
            }
            _ => {}
        }
        i += 1;
    }

    let load = |p: &str| {
        image::open(p)
            .map(|img| img.to_rgba8())
            .map_err(|e| zpdf::Error::StreamDecode(format!("open {p}: {e}")))
    };
    let a = load(a_path)?;
    let b = load(b_path)?;

    if a.dimensions() != b.dimensions() {
        println!(
            "DIMENSION MISMATCH: {:?} ({a_path}) vs {:?} ({b_path})",
            a.dimensions(),
            b.dimensions()
        );
        process::exit(2);
    }

    let (w, h) = a.dimensions();
    let total = w as u64 * h as u64;
    let mut diff_pixels: u64 = 0;
    let mut sum_abs: u64 = 0;
    let mut sum_sq: u64 = 0;
    let mut max_diff: u8 = 0;
    let mut heatmap = out_path.as_ref().map(|_| image::RgbaImage::new(w, h));

    for y in 0..h {
        for x in 0..w {
            let pa = a.get_pixel(x, y).0;
            let pb = b.get_pixel(x, y).0;
            let mut pix_max = 0u8;
            for c in 0..3 {
                let d = (pa[c] as i32 - pb[c] as i32).unsigned_abs() as u8;
                sum_abs += d as u64;
                sum_sq += d as u64 * d as u64;
                pix_max = pix_max.max(d);
            }
            max_diff = max_diff.max(pix_max);
            if pix_max > threshold {
                diff_pixels += 1;
            }
            if let Some(hm) = heatmap.as_mut() {
                // Dim grayscale of A, with differing pixels glowing red.
                let base = ((pa[0] as u16 + pa[1] as u16 + pa[2] as u16) / 3) as u8;
                let dim = base / 3 + 30;
                let r = (pix_max as u16 * 4).min(255) as u8;
                let other = if pix_max > threshold { dim / 2 } else { dim };
                hm.put_pixel(
                    x,
                    y,
                    image::Rgba([dim.saturating_add(r), other, other, 255]),
                );
            }
        }
    }

    let channels = (total * 3) as f64;
    let mae = sum_abs as f64 / channels;
    let rmse = (sum_sq as f64 / channels).sqrt();
    let pct = diff_pixels as f64 / total as f64 * 100.0;

    println!("Compare: {a_path}  vs  {b_path}");
    println!("  Size: {w}x{h} ({total} px)");
    println!("  Differing pixels (>{threshold}/channel): {diff_pixels} ({pct:.3}%)");
    println!("  MAE: {mae:.3}/255    RMSE: {rmse:.3}/255    Max channel diff: {max_diff}/255");

    if let (Some(hm), Some(op)) = (heatmap, out_path) {
        hm.save(&op)
            .map_err(|e| zpdf::Error::StreamDecode(format!("save {op}: {e}")))?;
        println!("  Diff heatmap: {op}");
    }

    Ok(())
}

fn cmd_text(args: &[String]) -> zpdf::Result<()> {
    let (args, password) = extract_password(args);
    if args.is_empty() {
        eprintln!("Usage: zpdf text <file.pdf> [-p <page>] [--all] [--struct] [--password <pw>]");
        process::exit(1);
    }

    let pdf_path = &args[0];
    let mut page_num: usize = 1;
    let mut all = false;
    // --struct: emit text in the Tagged-PDF structure tree's reading order
    // (with /ActualText / /Alt) instead of the geometric XY-cut.
    let mut use_struct = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                i += 1;
                page_num = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(1);
            }
            "--all" => all = true,
            "--struct" => use_struct = true,
            _ => {}
        }
        i += 1;
    }

    let doc = open_document(pdf_path, password.as_deref())?;

    // The structure tree (whole document) drives `--struct` ordering; each page's
    // marked content is selected by page index inside `struct_ordered_text`.
    let struct_tree = if use_struct { doc.struct_tree() } else { None };
    if use_struct && struct_tree.is_none() {
        eprintln!("(no structure tree; falling back to geometric reading order)");
    }

    let page_indices: Vec<usize> = if all {
        (0..doc.page_count()).collect()
    } else {
        vec![page_num.saturating_sub(1)]
    };

    // ICC transforms are per-document; share the cache across pages.
    let mut icc_cache = zpdf::IccCache::new();

    for &pi in &page_indices {
        let page = doc.page(pi)?;
        let mut font_cache = doc.load_page_fonts(&page);
        let content_bytes = doc.page_content_bytes(&page)?;
        let mut image_cache = zpdf::ImageCache::new();

        let mut spans: Vec<zpdf::TextSpan> = Vec::new();
        {
            let interpreter = zpdf::ContentInterpreter::new(page.effective_box())
                .with_fonts(&mut font_cache)
                .with_document(doc.file(), &page.resources)
                .with_images(&mut image_cache)
                .with_colors(&mut icc_cache)
                .with_text_sink(&mut spans);
            let _ = interpreter.interpret(&content_bytes);
        }

        if all {
            println!("===== Page {} =====", pi + 1);
        }
        let text = match &struct_tree {
            Some(tree) => zpdf::struct_ordered_text(&spans, pi, tree),
            None => zpdf::spans_to_text(spans, 2.0),
        };
        println!("{text}");
    }

    Ok(())
}

fn cmd_tables(args: &[String]) -> zpdf::Result<()> {
    let (args, password) = extract_password(args);
    if args.is_empty() {
        eprintln!("Usage: zpdf tables <file.pdf> [-p <page>] [--all] [--csv] [--password <pw>]");
        process::exit(1);
    }

    let pdf_path = &args[0];
    let mut page_num: usize = 1;
    let mut all = false;
    let mut csv = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                i += 1;
                page_num = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(1);
            }
            "--all" => all = true,
            "--csv" => csv = true,
            _ => {}
        }
        i += 1;
    }

    let doc = open_document(pdf_path, password.as_deref())?;
    let page_indices: Vec<usize> = if all {
        (0..doc.page_count()).collect()
    } else {
        vec![page_num.saturating_sub(1)]
    };

    // ICC transforms are per-document; share the cache across pages.
    let mut icc_cache = zpdf::IccCache::new();

    for &pi in &page_indices {
        let page = doc.page(pi)?;
        let mut font_cache = doc.load_page_fonts(&page);
        let content_bytes = doc.page_content_bytes(&page)?;
        let mut image_cache = zpdf::ImageCache::new();

        let mut spans: Vec<zpdf::TextSpan> = Vec::new();
        let mut rules: Vec<zpdf::RuleLine> = Vec::new();
        {
            let interpreter = zpdf::ContentInterpreter::new(page.effective_box())
                .with_fonts(&mut font_cache)
                .with_document(doc.file(), &page.resources)
                .with_images(&mut image_cache)
                .with_colors(&mut icc_cache)
                .with_text_sink(&mut spans)
                .with_rule_sink(&mut rules);
            let _ = interpreter.interpret(&content_bytes);
        }

        let tables = zpdf::detect_tables_with_rules(&spans, &rules);
        if all {
            println!("===== Page {} ({} table(s)) =====", pi + 1, tables.len());
        }
        for (ti, t) in tables.iter().enumerate() {
            println!("--- Table {} ({}x{}) ---", ti + 1, t.rows(), t.cols());
            println!("{}", if csv { t.to_csv() } else { t.to_tsv() });
        }
        if tables.is_empty() && !all {
            eprintln!("No tables detected on page {}.", pi + 1);
        }
    }

    Ok(())
}

fn cmd_render(args: &[String]) -> zpdf::Result<()> {
    let (args, password) = extract_password(args);
    if args.is_empty() {
        eprintln!(
            "Usage: zpdf render <file.pdf> [-p <page>] [-o <output.png>] [--dpi <dpi>] [--backend cpu|wgpu] [--stats] [--password <pw>]"
        );
        process::exit(1);
    }

    let pdf_path = &args[0];
    let mut page_num: usize = 1;
    let mut output = String::from("output.png");
    let mut dpi: f32 = 150.0;
    let mut backend = String::from("cpu");
    let mut stats = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                i += 1;
                page_num = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(1);
            }
            "-o" => {
                i += 1;
                if let Some(s) = args.get(i) {
                    output = s.clone();
                }
            }
            "--dpi" => {
                i += 1;
                dpi = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(150.0);
            }
            "--backend" => {
                i += 1;
                // The `_ => {}` arm below silently ignores unknown flags, so an
                // unparsed/typo'd backend value would otherwise render CPU silently.
                // Capture and validate explicitly at render time.
                backend = args.get(i).cloned().unwrap_or_else(|| {
                    eprintln!("--backend requires a value (cpu|wgpu)");
                    process::exit(2);
                });
            }
            "--stats" => stats = true,
            _ => {}
        }
        i += 1;
    }

    let doc = open_document(pdf_path, password.as_deref())?;
    if doc.is_encrypted() && password.is_none() {
        eprintln!("  note: document is encrypted; if output looks wrong, pass --password <pw>");
    }

    let page_index = page_num.saturating_sub(1);
    let page = doc.page(page_index)?;

    // CropBox ∩ MediaBox: the rect actually rendered (falls back to MediaBox).
    let page_box = page.effective_box();

    println!(
        "Rendering page {} ({:.0}x{:.0} pt) at {} DPI...",
        page_num,
        page_box.width(),
        page_box.height(),
        dpi
    );

    // Load page fonts
    let mut font_cache = doc.load_page_fonts(&page);
    let initial_fonts = font_cache.len();
    let initial_with_data = (0..initial_fonts)
        .filter(|&i| {
            font_cache
                .get(i as u32)
                .map(|f| f.has_font_data())
                .unwrap_or(false)
        })
        .count();
    println!(
        "  Fonts loaded: {} ({} with embedded data)",
        initial_fonts, initial_with_data
    );

    // Decode content stream
    let content_bytes = doc.page_content_bytes(&page)?;
    println!("  Content stream: {} bytes decoded", content_bytes.len());

    // Interpret content stream → display list
    let mut image_cache = zpdf::ImageCache::new();
    let annotations = doc.page_annotations(&page);
    let oc_config = doc.oc_config();
    let mut icc_cache = zpdf::IccCache::new();
    // PDF/X & PDF 2.0 output intents: colour-manage DeviceCMYK through the
    // page's (or document's) /DestOutputProfile when it is a CMYK ICC profile.
    // Compiled via `icc_cache` here, before `with_colors` takes its borrow.
    let doc_intents = doc.output_intents();
    let oi_cmyk = zpdf::output_intent_cmyk_profile(
        doc.file(),
        doc.page_output_intents(&page),
        &doc_intents,
        &mut icc_cache,
    );
    let mut interpreter = zpdf::ContentInterpreter::new(page_box)
        .with_page_rotation(page.rotate)
        .with_fonts(&mut font_cache)
        .with_document(doc.file(), &page.resources)
        .with_images(&mut image_cache)
        .with_colors(&mut icc_cache)
        .with_annotations(&annotations);
    if let Some(oc) = &oc_config {
        interpreter = interpreter.with_optional_content(oc);
    }
    if let Some(profile) = oi_cmyk {
        println!("  Output intent: DeviceCMYK colour-managed via /DestOutputProfile");
        interpreter = interpreter.with_output_intent_cmyk(profile);
    }
    let display_list = interpreter.interpret(&content_bytes);

    if font_cache.len() > initial_fonts {
        println!(
            "  Form XObject fonts: {} additional",
            font_cache.len() - initial_fonts
        );
    }
    println!("  Display list: {} commands", display_list.commands.len());

    // Count command types
    let mut fills = 0;
    let mut strokes = 0;
    let mut glyphs = 0;
    let mut clips = 0;
    let mut images = 0;
    for cmd in &display_list.commands {
        match cmd {
            zpdf::display_list::RenderCommand::FillPath { .. } => fills += 1,
            zpdf::display_list::RenderCommand::StrokePath { .. } => strokes += 1,
            zpdf::display_list::RenderCommand::DrawGlyphRun(_) => glyphs += 1,
            zpdf::display_list::RenderCommand::PushClip { .. } => clips += 1,
            zpdf::display_list::RenderCommand::DrawImage(_) => images += 1,
            _ => {}
        }
    }
    println!("    Fills: {fills}, Strokes: {strokes}, Glyph runs: {glyphs}, Clips: {clips}, Images: {images}");

    // Render with the selected backend. The DisplayList above is backend-agnostic;
    // we branch only here. Both arms route through `save_rgba` for one encoder path.
    #[allow(unused_imports)]
    use zpdf::RenderBackend;
    let scale = dpi / 72.0;

    match backend.as_str() {
        #[cfg(feature = "cpu")]
        "cpu" => {
            let mut renderer = zpdf::cpu::CpuRenderer::new()
                .with_fonts(&font_cache)
                .with_images(&image_cache);
            let start = std::time::Instant::now();
            let rendered: zpdf::cpu::RenderedPage = renderer
                .render_display_list(&display_list, scale)
                .map_err(|e| zpdf::Error::StreamDecode(e.to_string()))?;
            let wall = start.elapsed();
            println!(
                "  Rendered (cpu): {}x{} pixels",
                rendered.width, rendered.height
            );
            if stats {
                println!("  Stats: cpu wall {:.2}ms", wall.as_secs_f64() * 1000.0);
            }
            save_rgba(&output, rendered.width, rendered.height, &rendered.data)?;
        }
        #[cfg(not(feature = "cpu"))]
        "cpu" => {
            eprintln!("--backend cpu requires building with --features cpu");
            process::exit(1);
        }
        #[cfg(feature = "gpu")]
        "wgpu" => {
            let mut renderer = zpdf::gpu::WgpuRenderer::new()
                .with_fonts(&font_cache)
                .with_images(&image_cache);
            let start = std::time::Instant::now();
            let rendered = renderer
                .render_display_list(&display_list, scale)
                .map_err(|e| zpdf::Error::StreamDecode(e.to_string()))?;
            let wall = start.elapsed();
            println!(
                "  Rendered (wgpu): {}x{} pixels",
                rendered.width, rendered.height
            );
            if stats {
                print!("  Stats: cpu wall {:.2}ms", wall.as_secs_f64() * 1000.0);
                match renderer.last_gpu_time_ns() {
                    Some(ns) => println!(", gpu pass {:.2}ms", ns as f64 / 1_000_000.0),
                    None => {
                        println!(", gpu pass time unavailable (adapter lacks timestamp queries)")
                    }
                }
            }
            save_rgba(&output, rendered.width, rendered.height, &rendered.data)?;
        }
        #[cfg(not(feature = "gpu"))]
        "wgpu" => {
            eprintln!("--backend wgpu requires building with --features gpu");
            process::exit(1);
        }
        other => {
            eprintln!("unknown --backend '{other}' (expected cpu|wgpu)");
            process::exit(2);
        }
    }

    println!("  Saved to: {output}");

    Ok(())
}

/// Save a tight RGBA8 buffer (top-left origin, `len == w*h*4`) as a PNG. Shared by
/// both render backends so output goes through a single encoder path.
fn save_rgba(path: &str, w: u32, h: u32, data: &[u8]) -> zpdf::Result<()> {
    let img = image::RgbaImage::from_raw(w, h, data.to_vec())
        .ok_or_else(|| zpdf::Error::StreamDecode("rgba buffer size mismatch".into()))?;
    img.save(path)
        .map_err(|e| zpdf::Error::StreamDecode(format!("save {path}: {e}")))
}

fn cmd_debug_stream(args: &[String]) -> zpdf::Result<()> {
    if args.len() < 3 {
        eprintln!("Usage: zpdf debug-stream <file.pdf> <obj_num> <gen_num>");
        process::exit(1);
    }
    let data = fs::read(&args[0]).map_err(zpdf::Error::Io)?;
    let doc = zpdf::PdfDocument::open(data)?;
    let obj_num: u32 = args[1].parse().unwrap_or(0);
    let gen_num: u16 = args[2].parse().unwrap_or(0);
    let id = zpdf::ObjectId(obj_num, gen_num);
    let decoded = doc.file().resolve_stream_data(id)?;
    let text = String::from_utf8_lossy(&decoded);
    println!("{text}");
    Ok(())
}

/// Shared: build an IncrementalWriter from a file path, erroring on encrypted docs.
fn build_writer(
    path: &str,
    _password: Option<&str>,
) -> zpdf::Result<zpdf_writer::IncrementalWriter> {
    let data = fs::read(path).map_err(zpdf::Error::Io)?;
    zpdf_writer::IncrementalWriter::new(data)
}

/// Shared: write the incremental update to disk, refusing when output == input.
fn write_output(writer: &zpdf_writer::IncrementalWriter, out_path: &str) -> zpdf::Result<()> {
    let mut file = fs::File::create(out_path).map_err(zpdf::Error::Io)?;
    writer.write(&mut file).map_err(zpdf::Error::Io)?;
    Ok(())
}

/// Warn when the document has signatures (edits may invalidate them).
fn warn_signatures(doc: &zpdf::PdfDocument) {
    if !doc.signatures().is_empty() {
        eprintln!("Warning: document is digitally signed; this edit may invalidate signatures.");
    }
}

fn cmd_fill(args: &[String]) -> zpdf::Result<()> {
    let (args, password) = extract_password(args);
    let mut input = None;
    let mut output = None;
    let mut sets: Vec<(String, String)> = Vec::new();
    let mut list = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--set" => {
                if let Some(val) = args.get(i + 1) {
                    if let Some(pos) = val.find('=') {
                        let (name, value) = val.split_at(pos);
                        sets.push((name.to_string(), value[1..].to_string()));
                        i += 2;
                    } else {
                        eprintln!("--set requires NAME=VALUE");
                        process::exit(1);
                    }
                } else {
                    eprintln!("--set requires a value");
                    process::exit(1);
                }
            }
            "-o" => {
                output = args.get(i + 1).cloned();
                i += 2;
            }
            "--list" => {
                list = true;
                i += 1;
            }
            other if !other.starts_with('-') => {
                if input.is_none() {
                    input = Some(other.to_string());
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    let Some(input_path) = input else {
        eprintln!("Usage: zpdf fill <file.pdf> --set NAME=VALUE [--set ...] [--list] -o <out.pdf>");
        process::exit(1);
    };

    if list {
        let doc = open_document(&input_path, password.as_deref())?;
        if let Some(form) = doc.acro_form() {
            println!("Fields:");
            for field in &form.fields {
                let val = field
                    .value
                    .as_ref()
                    .map(|v| format!("{:?}", v))
                    .unwrap_or_else(|| "None".to_string());
                println!("  {} ({:?}): {}", field.name, field.kind, val);
            }
        } else {
            println!("No AcroForm.");
        }
        return Ok(());
    }

    let Some(out_path) = output else {
        eprintln!("-o <out.pdf> required");
        process::exit(1);
    };
    if out_path == input_path {
        eprintln!("Output path must differ from input");
        process::exit(1);
    }

    let doc = open_document(&input_path, password.as_deref())?;
    warn_signatures(&doc);
    let mut writer = build_writer(&input_path, password.as_deref())?;
    let mut filler = zpdf_writer::FormFiller::new(&mut writer)?;
    for (name, value) in &sets {
        filler.set(name, value)?;
    }
    filler.finish()?;
    write_output(&writer, &out_path)?;
    println!("Filled {} fields → {}", sets.len(), out_path);
    Ok(())
}

fn cmd_pages(args: &[String]) -> zpdf::Result<()> {
    let (args, password) = extract_password(args);
    let mut input = None;
    let mut output = None;
    let mut rotates: Vec<(Vec<usize>, i32)> = Vec::new();
    let mut deletes: Vec<usize> = Vec::new();
    let mut order: Option<Vec<usize>> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--rotate" => {
                if let Some(spec) = args.get(i + 1) {
                    let parts: Vec<&str> = spec.split(':').collect();
                    if parts.len() == 2 {
                        let pages = parse_page_list(parts[0]);
                        let deg: i32 = parts[1].parse().unwrap_or(0);
                        rotates.push((pages, deg));
                    }
                    i += 2;
                } else {
                    eprintln!("--rotate requires PAGES:DEG");
                    process::exit(1);
                }
            }
            "--delete" => {
                if let Some(list) = args.get(i + 1) {
                    deletes.extend(parse_page_list(list));
                    i += 2;
                } else {
                    eprintln!("--delete requires a page list");
                    process::exit(1);
                }
            }
            "--order" => {
                if let Some(list) = args.get(i + 1) {
                    order = Some(parse_page_list(list));
                    i += 2;
                } else {
                    eprintln!("--order requires a page list");
                    process::exit(1);
                }
            }
            "-o" => {
                output = args.get(i + 1).cloned();
                i += 2;
            }
            other if !other.starts_with('-') => {
                if input.is_none() {
                    input = Some(other.to_string());
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    let Some(input_path) = input else {
        eprintln!("Usage: zpdf pages <file.pdf> [--rotate PAGES:DEG] [--delete LIST] [--order LIST] -o <out.pdf>");
        process::exit(1);
    };
    let Some(out_path) = output else {
        eprintln!("-o <out.pdf> required");
        process::exit(1);
    };
    if out_path == input_path {
        eprintln!("Output path must differ from input");
        process::exit(1);
    }
    if order.is_some() && !deletes.is_empty() {
        eprintln!("--order and --delete are mutually exclusive in this version");
        process::exit(1);
    }

    let doc = open_document(&input_path, password.as_deref())?;
    warn_signatures(&doc);
    let mut writer = build_writer(&input_path, password.as_deref())?;
    for (pages, deg) in &rotates {
        for &idx in pages {
            writer.rotate_page(idx, *deg)?;
        }
    }
    if let Some(ord) = order {
        writer.reorder_pages(&ord)?;
    } else if !deletes.is_empty() {
        writer.delete_pages(&deletes)?;
    }
    write_output(&writer, &out_path)?;
    println!("Page ops applied → {}", out_path);
    Ok(())
}

fn cmd_set_meta(args: &[String]) -> zpdf::Result<()> {
    let (args, password) = extract_password(args);
    let mut input = None;
    let mut output = None;
    let mut update = zpdf_writer::InfoUpdate::default();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--title" => {
                update.title = args.get(i + 1).map(|s| Some(s.clone()));
                i += 2;
            }
            "--author" => {
                update.author = args.get(i + 1).map(|s| Some(s.clone()));
                i += 2;
            }
            "--subject" => {
                update.subject = args.get(i + 1).map(|s| Some(s.clone()));
                i += 2;
            }
            "--keywords" => {
                update.keywords = args.get(i + 1).map(|s| Some(s.clone()));
                i += 2;
            }
            "--creator" => {
                update.creator = args.get(i + 1).map(|s| Some(s.clone()));
                i += 2;
            }
            "--producer" => {
                update.producer = args.get(i + 1).map(|s| Some(s.clone()));
                i += 2;
            }
            "-o" => {
                output = args.get(i + 1).cloned();
                i += 2;
            }
            other if !other.starts_with('-') => {
                if input.is_none() {
                    input = Some(other.to_string());
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    let Some(input_path) = input else {
        eprintln!("Usage: zpdf set-meta <file.pdf> [--title S] [--author S] ... -o <out.pdf>");
        process::exit(1);
    };
    let Some(out_path) = output else {
        eprintln!("-o <out.pdf> required");
        process::exit(1);
    };
    if out_path == input_path {
        eprintln!("Output path must differ from input");
        process::exit(1);
    }
    if update.is_empty() {
        eprintln!("No metadata fields specified");
        process::exit(1);
    }

    let doc = open_document(&input_path, password.as_deref())?;
    warn_signatures(&doc);
    let mut writer = build_writer(&input_path, password.as_deref())?;
    writer.set_info(&update)?;
    write_output(&writer, &out_path)?;
    println!("Metadata updated → {}", out_path);
    Ok(())
}

fn cmd_stamp(args: &[String]) -> zpdf::Result<()> {
    let (args, password) = extract_password(args);
    let mut input = None;
    let mut output = None;
    let mut page: Option<usize> = None;
    let mut items: Vec<zpdf::StampItem> = Vec::new();
    let mut current_text: Option<(String, f64, f64)> = None;
    let mut current_font = "Helvetica".to_string();
    let mut current_size = 12.0;
    let mut current_color = (0.0, 0.0, 0.0);

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                if let Some(val) = args.get(i + 1).and_then(|s| s.parse::<usize>().ok()) {
                    page = Some(val - 1); // 1-based → 0-based
                    i += 2;
                } else {
                    eprintln!("-p requires a page number");
                    process::exit(1);
                }
            }
            "--text" => {
                if let Some(current) = current_text.take() {
                    items.push(zpdf::StampItem::Text {
                        text: current.0,
                        x: current.1,
                        y: current.2,
                        font: current_font.clone(),
                        size: current_size,
                        color: current_color,
                    });
                }
                if let Some(val) = args.get(i + 1) {
                    current_text = Some((val.clone(), 0.0, 0.0));
                    i += 2;
                } else {
                    eprintln!("--text requires a string");
                    process::exit(1);
                }
            }
            "--at" if current_text.is_some() => {
                if let Some(val) = args.get(i + 1) {
                    let parts: Vec<&str> = val.split(',').collect();
                    if parts.len() == 2 {
                        let x = parts[0].parse().unwrap_or(0.0);
                        let y = parts[1].parse().unwrap_or(0.0);
                        if let Some(ref mut t) = current_text {
                            t.1 = x;
                            t.2 = y;
                        }
                    }
                    i += 2;
                } else {
                    eprintln!("--at requires X,Y");
                    process::exit(1);
                }
            }
            "--font" => {
                if let Some(val) = args.get(i + 1) {
                    current_font = val.clone();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--size" => {
                if let Some(val) = args.get(i + 1).and_then(|s| s.parse().ok()) {
                    current_size = val;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--color" => {
                if let Some(val) = args.get(i + 1) {
                    let parts: Vec<&str> = val.split(',').collect();
                    if parts.len() == 3 {
                        current_color = (
                            parts[0].parse().unwrap_or(0.0),
                            parts[1].parse().unwrap_or(0.0),
                            parts[2].parse().unwrap_or(0.0),
                        );
                    }
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "-o" => {
                output = args.get(i + 1).cloned();
                i += 2;
            }
            other if !other.starts_with('-') => {
                if input.is_none() {
                    input = Some(other.to_string());
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    if let Some(current) = current_text {
        items.push(zpdf::StampItem::Text {
            text: current.0,
            x: current.1,
            y: current.2,
            font: current_font,
            size: current_size,
            color: current_color,
        });
    }

    let Some(input_path) = input else {
        eprintln!("Usage: zpdf stamp <file.pdf> -p N --text STR --at X,Y [--font F] [--size N] [--color R,G,B] -o <out.pdf>");
        process::exit(1);
    };
    let Some(out_path) = output else {
        eprintln!("-o <out.pdf> required");
        process::exit(1);
    };
    if out_path == input_path {
        eprintln!("Output path must differ from input");
        process::exit(1);
    }
    let Some(page_idx) = page else {
        eprintln!("-p <page> required");
        process::exit(1);
    };

    let doc = open_document(&input_path, password.as_deref())?;
    warn_signatures(&doc);
    let mut writer = build_writer(&input_path, password.as_deref())?;
    writer.stamp_page(page_idx, &items)?;
    write_output(&writer, &out_path)?;
    println!(
        "Stamped {} items on page {} → {}",
        items.len(),
        page_idx + 1,
        out_path
    );
    Ok(())
}

/// Parse "1,3-5,8" into 0-based indices [0,2,3,4,7]. "all" is not supported.
fn parse_page_list(s: &str) -> Vec<usize> {
    let mut result = Vec::new();
    for part in s.split(',') {
        if let Some(pos) = part.find('-') {
            let (start, end) = part.split_at(pos);
            let a: usize = start.parse::<usize>().unwrap_or(1).saturating_sub(1);
            let b: usize = end[1..].parse::<usize>().unwrap_or(1).saturating_sub(1);
            for i in a..=b {
                result.push(i);
            }
        } else if let Ok(n) = part.parse::<usize>() {
            result.push(n.saturating_sub(1));
        }
    }
    result
}
#[cfg(test)]
mod tests {
    use super::{collect_attachments, create_unique, sanitize_filename};
    use std::io::Write;

    #[test]
    fn sanitize_strips_traversal_and_absolute_paths() {
        assert_eq!(
            sanitize_filename("../../etc/passwd").as_deref(),
            Some("passwd")
        );
        assert_eq!(
            sanitize_filename("..\\..\\Windows\\system32\\evil.dll").as_deref(),
            Some("evil.dll")
        );
        assert_eq!(
            sanitize_filename("/abs/path/file.bin").as_deref(),
            Some("file.bin")
        );
        assert_eq!(
            sanitize_filename("C:\\Users\\me\\x.txt").as_deref(),
            Some("x.txt")
        );
        // A name that traverses then names a file still collapses to the base.
        assert_eq!(
            sanitize_filename("a/../../../../root/.bashrc").as_deref(),
            Some(".bashrc")
        );
    }

    #[test]
    fn sanitize_rejects_dot_segments_and_empties() {
        assert_eq!(sanitize_filename(""), None);
        assert_eq!(sanitize_filename("."), None);
        assert_eq!(sanitize_filename(".."), None);
        assert_eq!(sanitize_filename("   "), None);
        assert_eq!(sanitize_filename("foo/"), None); // trailing separator → empty base
        assert_eq!(sanitize_filename("..."), None); // only dots
        assert_eq!(sanitize_filename(". . ."), None); // dots + spaces
    }

    #[test]
    fn sanitize_replaces_dangerous_chars() {
        // Windows alternate-data-stream colon, wildcards, control chars → '_'.
        assert_eq!(
            sanitize_filename("file:stream").as_deref(),
            Some("file_stream")
        );
        assert_eq!(
            sanitize_filename("a<b>c|d?e*f").as_deref(),
            Some("a_b_c_d_e_f")
        );
        assert_eq!(sanitize_filename("tab\tname").as_deref(), Some("tab_name"));
        // No output path separators can survive sanitization.
        let s = sanitize_filename("x/y\\z").unwrap();
        assert!(!s.contains('/') && !s.contains('\\'));
    }

    #[test]
    fn sanitize_guards_windows_reserved_names() {
        assert_eq!(sanitize_filename("NUL").as_deref(), Some("_NUL"));
        assert_eq!(sanitize_filename("con.txt").as_deref(), Some("_con.txt"));
        assert_eq!(sanitize_filename("COM1").as_deref(), Some("_COM1"));
        assert_eq!(sanitize_filename("LPT9.dat").as_deref(), Some("_LPT9.dat"));
        // Not reserved: a longer stem that merely starts with the prefix.
        assert_eq!(
            sanitize_filename("complete.txt").as_deref(),
            Some("complete.txt")
        );
        assert_eq!(sanitize_filename("COM10").as_deref(), Some("COM10"));
    }

    #[test]
    fn sanitize_strips_trailing_dots_and_spaces() {
        // Windows silently drops trailing dots/spaces at create time, so two
        // names differing only there must collapse to one (else they collide on
        // disk while unique_name thinks they differ).
        assert_eq!(sanitize_filename("evil.txt.").as_deref(), Some("evil.txt"));
        assert_eq!(
            sanitize_filename("report.pdf .").as_deref(),
            Some("report.pdf")
        );
        assert_eq!(sanitize_filename("name   ").as_deref(), Some("name"));
        assert_eq!(sanitize_filename("config.").as_deref(), Some("config"));
        // The reserved-name guard still fires after trimming.
        assert_eq!(sanitize_filename("NUL.").as_deref(), Some("_NUL"));
    }

    /// Build a PDF from numbered object bodies (object i+1), with a correct xref
    /// and `/Root` = object 1 — mirrors zpdf-document's test helper.
    fn build(objs: &[&str]) -> Vec<u8> {
        let mut buf = Vec::from(&b"%PDF-1.7\n"[..]);
        let mut offsets = Vec::new();
        for (i, body) in objs.iter().enumerate() {
            offsets.push(buf.len());
            buf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref = buf.len();
        buf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes(),
        );
        for off in &offsets {
            buf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        buf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objs.len() + 1
            )
            .as_bytes(),
        );
        buf
    }

    #[test]
    fn collect_attachments_dedups_shared_stream_and_merges_relationship() {
        // A name-tree filespec WITHOUT /AFRelationship and a catalog-/AF filespec
        // WITH one both point at the same /EF stream (obj4): one merged entry.
        let doc = zpdf::PdfDocument::open(build(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles 5 0 R >> /AF [6 0 R] >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
            "<< /Type /EmbeddedFile /Length 1 >>\nstream\nx\nendstream",
            "<< /Names [ (a.bin) 7 0 R ] >>",
            "<< /Type /Filespec /F (a.bin) /AFRelationship /Data /EF << /F 4 0 R >> >>",
            "<< /Type /Filespec /F (a.bin) /EF << /F 4 0 R >> >>",
        ]))
        .expect("open");
        let all = collect_attachments(&doc);
        assert_eq!(all.len(), 1, "shared stream collapses to one entry");
        assert_eq!(all[0].relationship.as_deref(), Some("Data"));
    }

    #[test]
    fn collect_attachments_includes_page_level_af() {
        let doc = zpdf::PdfDocument::open(build(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /AF [4 0 R] >>",
            "<< /Type /Filespec /F (p.bin) /AFRelationship /Supplement /EF << /F 5 0 R >> >>",
            "<< /Type /EmbeddedFile /Length 0 >>\nstream\n\nendstream",
        ]))
        .expect("open");
        let all = collect_attachments(&doc);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "p.bin");
        assert_eq!(all[0].relationship.as_deref(), Some("Supplement"));
    }

    #[test]
    fn create_unique_never_overwrites_existing() {
        let dir = std::env::temp_dir().join(format!("zpdf_cu_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ds = dir.to_str().unwrap();
        std::fs::write(dir.join("a.txt"), b"OLD").unwrap();

        // The pre-existing a.txt is preserved; the new content lands beside it.
        let (p1, mut f1) = create_unique(ds, "a.txt").unwrap();
        f1.write_all(b"new1").unwrap();
        assert_eq!(p1.file_name().unwrap(), "a (1).txt");
        let (p2, mut f2) = create_unique(ds, "a.txt").unwrap();
        f2.write_all(b"new2").unwrap();
        assert_eq!(p2.file_name().unwrap(), "a (2).txt");
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"OLD");

        // Extensionless collisions disambiguate too.
        let (p3, _f3) = create_unique(ds, "data").unwrap();
        assert_eq!(p3.file_name().unwrap(), "data");
        let (p4, _f4) = create_unique(ds, "data").unwrap();
        assert_eq!(p4.file_name().unwrap(), "data (1)");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
