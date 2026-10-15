//! Scalar cmap/advance queries without constructing the outline parser state.
//! Variable fonts retain ttf-parser's full variation-aware metrics path.

use crate::LoadedFont;
use ttf_parser::{cmap, head, hhea, hmtx, maxp, Face, RawFace, RawFaceTables};

#[derive(Default)]
struct QueryTables<'a> {
    head: &'a [u8],
    hhea: &'a [u8],
    maxp: &'a [u8],
    cmap: Option<&'a [u8]>,
    hmtx: Option<&'a [u8]>,
    fvar: Option<&'a [u8]>,
    avar: Option<&'a [u8]>,
    hvar: Option<&'a [u8]>,
    gvar: Option<&'a [u8]>,
    glyf: Option<&'a [u8]>,
    loca: Option<&'a [u8]>,
}

pub(crate) fn lookup<const METRICS: bool>(font: &LoadedFont, code: u32) -> Option<(u16, f64)> {
    let character = char::from_u32(code)?;
    lookup_program::<METRICS>(
        font.font_data.as_deref()?,
        font.face_index,
        character,
        Some(font),
    )
}

pub(crate) fn scalar_gid(data: &[u8], face_index: u32, character: char) -> Option<u16> {
    lookup_program::<false>(data, face_index, character, None).map(|(gid, _)| gid)
}

fn lookup_program<const METRICS: bool>(
    data: &[u8],
    face_index: u32,
    character: char,
    font: Option<&LoadedFont>,
) -> Option<(u16, f64)> {
    let units_per_em = if METRICS { font?.units_per_em } else { 1.0 };
    let raw = RawFace::parse(data, face_index).ok()?;
    let mut tables = QueryTables::default();
    // Face::parse also walks the directory and keeps the last record, even
    // when that record clears an earlier table with an invalid data range.
    // Do not impose a new sorted-directory assumption on existing callers.
    for record in raw.table_records {
        let start = record.offset as usize;
        let Some(end) = start.checked_add(record.length as usize) else {
            continue;
        };
        let data = raw.data.get(start..end);
        match &record.tag.to_bytes() {
            b"head" => tables.head = data.unwrap_or_default(),
            b"hhea" => tables.hhea = data.unwrap_or_default(),
            b"maxp" => tables.maxp = data.unwrap_or_default(),
            b"cmap" => tables.cmap = data,
            b"hmtx" => tables.hmtx = data,
            b"fvar" if METRICS => tables.fvar = data,
            b"avar" if METRICS => tables.avar = data,
            b"HVAR" if METRICS => tables.hvar = data,
            b"gvar" if METRICS => tables.gvar = data,
            b"glyf" if METRICS => tables.glyf = data,
            b"loca" if METRICS => tables.loca = data,
            _ => {}
        }
    }
    if METRICS && tables.fvar.is_some() {
        return variable_lookup(font?, character, tables);
    }
    // Preserve Face::parse's required-table checks and optional-hmtx behavior.
    head::Table::parse(tables.head)?;
    let hhea = hhea::Table::parse(tables.hhea)?;
    let maxp = maxp::Table::parse(tables.maxp)?;
    let cmap = cmap::Table::parse(tables.cmap?)?;
    let gid = cmap
        .subtables
        .into_iter()
        .filter(|table| table.is_unicode())
        .find_map(|table| table.glyph_index(character as u32))?;
    let advance = if METRICS {
        tables
            .hmtx
            .and_then(|bytes| {
                hmtx::Table::parse(hhea.number_of_metrics, maxp.number_of_glyphs, bytes)
            })
            .and_then(|table| table.advance(gid))
            .map_or(500.0, |value| value as f64 * 1000.0 / units_per_em)
    } else {
        0.0
    };
    Some((gid.0, advance))
}

// Keep the full Face allocation and copy off the static-font lookup stack.
#[inline(never)]
fn variable_lookup(
    font: &LoadedFont,
    character: char,
    query: QueryTables<'_>,
) -> Option<(u16, f64)> {
    // Retain every dependency of cmap lookup and horizontal variation metrics.
    // Face applies fvar/avar coordinates and HVAR or gvar phantom-point deltas;
    // unrelated color/layout/name tables need not be parsed for these queries.
    let tables = RawFaceTables {
        head: query.head,
        hhea: query.hhea,
        maxp: query.maxp,
        cmap: query.cmap,
        hmtx: query.hmtx,
        fvar: query.fvar,
        avar: query.avar,
        hvar: query.hvar,
        gvar: query.gvar,
        glyf: query.glyf,
        loca: query.loca,
        ..Default::default()
    };
    let mut face = Face::from_raw_tables(tables).ok()?;
    font.apply_variations(&mut face);
    let gid = face.glyph_index(character)?;
    let advance = face
        .glyph_hor_advance(gid)
        .map_or(500.0, |value| value as f64 * 1000.0 / font.units_per_em);
    Some((gid.0, advance))
}
