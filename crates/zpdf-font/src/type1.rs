//! Embedded Type 1 (PostScript) font program support.
//!
//! `ttf-parser` only understands sfnt/CFF, so embedded Type 1 fonts (the
//! `FontFile` form used pervasively by LaTeX / Computer Modern) would otherwise
//! render no glyphs. This module parses the font program — PFB/PFA reassembly,
//! `eexec` and charstring decryption — and interprets Type 1 charstrings into
//! glyph outlines, including subroutines, the flex/hint-replacement OtherSubrs,
//! and `seac` accented composites.

use std::collections::HashMap;

use crate::encoding::STANDARD_ENCODING;
use crate::{GlyphOutline, OutlineCommand};

const EEXEC_R: u16 = 55665;
const CHARSTRING_R: u16 = 4330;
const C1: u16 = 52845;
const C2: u16 = 22719;
const MAX_SUBR_DEPTH: u32 = 60;
/// `seac` components must not be accented characters themselves (Adobe Type 1
/// Font Format §6.4), so composition is one level deep. Each level builds two
/// components: a deeper limit lets a chain of distinct composites multiply a
/// leaf outline 2^depth times.
const MAX_SEAC_DEPTH: u32 = 1;
const MAX_CHARSTRING_STEPS: usize = 100_000;
const MAX_CHARSTRING_STACK: usize = 96;
const MAX_TYPE1_SUBRS: usize = 65_536;
const MAX_TYPE1_GLYPHS: usize = 65_536;
const MAX_GLYPH_NAME_LEN: usize = 127;

/// A parsed Type 1 font program.
pub struct Type1Font {
    /// Decrypted charstring bytes per glyph name.
    char_strings: HashMap<String, Vec<u8>>,
    /// Decrypted /Subrs by index.
    subrs: Vec<Vec<u8>>,
    /// Built-in /Encoding: character code → glyph name.
    builtin_encoding: Vec<Option<String>>,
    /// Glyph name per synthetic glyph id (used as the display-list `glyph_id`).
    gid_to_name: Vec<String>,
    name_to_gid: HashMap<String, u16>,
    /// units-per-em derived from the FontMatrix (usually 1000).
    pub units_per_em: f64,
    /// Full /FontMatrix from the font program. Usually the plain diagonal
    /// `[1/upem 0 0 1/upem 0 0]`, but dvips SlantFont/ExtendFont variants carry
    /// skew or unequal scales that must be applied to the outlines.
    font_matrix: [f64; 6],
}

impl Type1Font {
    pub(crate) fn estimated_cache_bytes(&self) -> u64 {
        let mut bytes = crate::estimated_map_bytes(&self.char_strings)
            .saturating_add(crate::estimated_map_bytes(&self.name_to_gid))
            .saturating_add(crate::allocation_bytes::<Vec<u8>>(self.subrs.capacity()))
            .saturating_add(crate::allocation_bytes::<Option<String>>(
                self.builtin_encoding.capacity(),
            ))
            .saturating_add(crate::allocation_bytes::<String>(
                self.gid_to_name.capacity(),
            ));
        for (name, data) in &self.char_strings {
            bytes = bytes
                .saturating_add(name.capacity() as u64)
                .saturating_add(data.capacity() as u64);
        }
        for data in &self.subrs {
            bytes = bytes.saturating_add(data.capacity() as u64);
        }
        for name in self
            .builtin_encoding
            .iter()
            .flatten()
            .chain(self.gid_to_name.iter())
            .chain(self.name_to_gid.keys())
        {
            bytes = bytes.saturating_add(name.capacity() as u64);
        }
        bytes
    }

    /// Parse a Type 1 font program. Returns `None` if it does not look like one.
    pub fn parse(data: &[u8]) -> Option<Type1Font> {
        // Plain PFA data can be parsed in place. Only PFB input needs a
        // reassembled copy, avoiding one full-font allocation in the common
        // path.
        let flattened = (data.first() == Some(&0x80)).then(|| depfb(data));
        let data = flattened.as_deref().unwrap_or(data);
        let eexec_pos = find(data, b"eexec")?;
        let clear = &data[..eexec_pos];

        // Binary (or hex) eexec section begins after `eexec` + whitespace.
        let mut p = eexec_pos + 5;
        while p < data.len() && is_ws(data[p]) {
            p += 1;
        }
        let enc = &data[p..];
        let private = if looks_hex(enc) {
            let binary = hex_decode(enc);
            decrypt(&binary, EEXEC_R, 4)
        } else {
            decrypt(enc, EEXEC_R, 4)
        };

        let font_matrix = parse_font_matrix(clear).unwrap_or([0.001, 0.0, 0.0, 0.001, 0.0, 0.0]);
        let units_per_em = if font_matrix[0].is_finite() && font_matrix[0].abs() > 1e-12 {
            1.0 / font_matrix[0].abs()
        } else {
            1000.0
        };
        let builtin_encoding = parse_builtin_encoding(clear);

        let len_iv = parse_len_iv(&private).unwrap_or(4);
        let subrs = parse_subrs(&private, len_iv);
        let char_strings = parse_charstrings(&private, len_iv);
        if char_strings.is_empty() {
            return None;
        }

        // Assign synthetic glyph ids: .notdef first, then the rest deterministically.
        let mut names: Vec<String> = char_strings.keys().cloned().collect();
        names.sort();
        if let Some(pos) = names.iter().position(|n| n == ".notdef") {
            names.swap(0, pos);
        }
        names.truncate(MAX_TYPE1_GLYPHS);
        let name_to_gid: HashMap<String, u16> = names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), i as u16))
            .collect();

        Some(Type1Font {
            char_strings,
            subrs,
            builtin_encoding,
            gid_to_name: names,
            name_to_gid,
            units_per_em,
            font_matrix,
        })
    }

    /// Synthetic glyph id for a glyph name.
    pub fn gid_for_name(&self, name: &str) -> Option<u16> {
        self.name_to_gid.get(name).copied()
    }

    /// Whether sampled non-space charstrings decode into usable outlines rather
    /// than failing or producing empty output across the whole font.
    pub fn has_usable_outlines(&self) -> bool {
        self.probe_outlines() != crate::EmbeddedOutlineProbe::Unusable
    }

    /// Classify sampled non-space charstrings as completely healthy, partially
    /// corrupt (usable as primary, with failing charstrings), or unusable.
    pub(crate) fn probe_outlines(&self) -> crate::EmbeddedOutlineProbe {
        let non_blank: Vec<&str> = self
            .gid_to_name
            .iter()
            .map(String::as_str)
            .filter(|name| !matches!(*name, "" | ".notdef" | "space" | "nbspace" | "uni00A0"))
            .collect();
        if non_blank.is_empty() {
            return crate::EmbeddedOutlineProbe::Healthy;
        }
        let step = (non_blank.len() / 32).max(1);
        let mut drawn = 0u32;
        let mut failed = 0u32;
        let mut i = 0;
        while i < non_blank.len() {
            let name = non_blank[i];
            if let Some(cs) = self.char_strings.get(name) {
                let mut interp = Interp::new();
                interp.run(cs, &self.subrs, 0);
                if interp.failed || !interp.done {
                    failed += 1;
                } else if let Some(seac) = interp.seac {
                    if self.compose_seac(&interp, seac, 1).is_some() {
                        drawn += 1;
                    } else {
                        failed += 1;
                    }
                } else if has_drawable_segment(&interp.cmds) {
                    drawn += 1;
                } else if crate::glyph_list::glyph_name_to_string(name)
                    .is_some_and(|s| crate::resolution::is_visible_text(&s))
                {
                    failed += 1;
                }
            }
            i += step;
        }
        if failed > 0 && failed > drawn {
            crate::EmbeddedOutlineProbe::Unusable
        } else if failed > 0 {
            crate::EmbeddedOutlineProbe::PartiallyCorrupt
        } else {
            crate::EmbeddedOutlineProbe::Healthy
        }
    }

    /// Classify `gid`'s embedded charstring for drawing-font resolution:
    /// drawable path (`Drawn`), valid no-ink charstring such as `/space`
    /// (`Blank`), or missing / failing / damaged empty visible charstring
    /// (`Missing`).
    pub(crate) fn glyph_state_by_gid(&self, gid: u16) -> crate::resolution::GlyphState {
        let Some(name) = self.gid_to_name.get(gid as usize) else {
            return crate::resolution::GlyphState::Missing;
        };
        if name.is_empty() || name == ".notdef" {
            return crate::resolution::GlyphState::Missing;
        }
        let Some(cs) = self.char_strings.get(name) else {
            return crate::resolution::GlyphState::Missing;
        };
        let mut interp = Interp::new();
        interp.run(cs, &self.subrs, 0);
        if interp.failed || !interp.done {
            return crate::resolution::GlyphState::Missing;
        }
        if let Some(seac) = interp.seac {
            return if self.compose_seac(&interp, seac, 1).is_some() {
                crate::resolution::GlyphState::Drawn
            } else {
                crate::resolution::GlyphState::Missing
            };
        }
        if has_drawable_segment(&interp.cmds) {
            crate::resolution::GlyphState::Drawn
        } else if crate::glyph_list::glyph_name_to_string(name)
            .is_some_and(|s| crate::resolution::is_visible_text(&s))
        {
            crate::resolution::GlyphState::Missing
        } else {
            crate::resolution::GlyphState::Blank
        }
    }

    /// Whether `gid` maps to a present non-`.notdef` charstring whose outline
    /// is physically blank (no drawing commands and not a known visible glyph
    /// name), such as `/space` or a custom subset glyph name `/g3`.
    pub(crate) fn is_blank_gid(&self, gid: u16) -> bool {
        let Some(name) = self.gid_to_name.get(gid as usize) else {
            return false;
        };
        if name.is_empty() || name == ".notdef" || !self.char_strings.contains_key(name) {
            return false;
        }
        self.glyph_state_by_gid(gid) == crate::resolution::GlyphState::Blank
    }

    /// Whether `gid` maps to an embedded charstring whose execution fails or
    /// emits no path for a known visible character name (as opposed to an
    /// unmapped `.notdef`, a space / no-ink character, or an unknown name).
    pub(crate) fn is_gid_unreadable(&self, gid: u16) -> bool {
        let Some(name) = self.gid_to_name.get(gid as usize) else {
            return false;
        };
        if name.is_empty() || name == ".notdef" || !self.char_strings.contains_key(name) {
            return false;
        }
        self.glyph_state_by_gid(gid) == crate::resolution::GlyphState::Missing
    }

    /// Built-in encoding glyph name for a character code.
    pub fn builtin_name(&self, code: u8) -> Option<&str> {
        self.builtin_encoding
            .get(code as usize)
            .and_then(|o| o.as_deref())
    }

    /// Outline for a synthetic glyph id.
    pub fn glyph_outline_by_gid(&self, gid: u16) -> Option<GlyphOutline> {
        let name = self.gid_to_name.get(gid as usize)?;
        self.glyph_outline_by_name(name)
    }

    /// Outline for a glyph name, interpreting its charstring. The font's
    /// /FontMatrix is honored in full: skew/extension components are baked
    /// into the returned commands (normalized to `units_per_em` space).
    pub fn glyph_outline_by_name(&self, name: &str) -> Option<GlyphOutline> {
        let outline = self.raw_outline_by_name(name)?;
        Some(self.apply_font_matrix(outline))
    }

    /// Outline in raw charstring space (no FontMatrix applied). `seac`
    /// composites offset base/accent in this space, so the matrix must only
    /// be applied once, at the public boundary.
    fn raw_outline_by_name(&self, name: &str) -> Option<GlyphOutline> {
        self.raw_outline_by_name_depth(name, 0)
    }

    fn raw_outline_by_name_depth(&self, name: &str, depth: u32) -> Option<GlyphOutline> {
        if depth > MAX_SEAC_DEPTH {
            return None;
        }
        let cs = self.char_strings.get(name)?;
        let mut interp = Interp::new();
        interp.run(cs, &self.subrs, 0);

        if interp.failed || !interp.done {
            return None;
        }

        if let Some(seac) = interp.seac {
            return self.compose_seac(&interp, seac, depth + 1);
        }
        if !has_drawable_segment(&interp.cmds) {
            return None;
        }
        Some(GlyphOutline {
            commands: interp.cmds,
            advance_width: interp.width,
        })
    }

    /// Apply the font program's /FontMatrix to an outline. The plain diagonal
    /// matrix `[1/upem 0 0 1/upem 0 0]` is already accounted for by consumers
    /// dividing by `units_per_em`, so it is left untouched (bit-exact);
    /// slanted/extended matrices are normalized into that same upem space:
    /// `p' = M(p) * units_per_em`. The advance is unchanged — its horizontal
    /// component under M is `a*w`, and `a * units_per_em == 1`.
    fn apply_font_matrix(&self, mut outline: GlyphOutline) -> GlyphOutline {
        let [a, b, c, d, e, f] = self.font_matrix;
        if b == 0.0 && c == 0.0 && e == 0.0 && f == 0.0 && a == d && a > 0.0 {
            return outline;
        }
        let s = self.units_per_em;
        let tx = |x: f64, y: f64| ((a * x + c * y + e) * s, (b * x + d * y + f) * s);
        for cmd in &mut outline.commands {
            *cmd = match *cmd {
                OutlineCommand::MoveTo(x, y) => {
                    let (x, y) = tx(x, y);
                    OutlineCommand::MoveTo(x, y)
                }
                OutlineCommand::LineTo(x, y) => {
                    let (x, y) = tx(x, y);
                    OutlineCommand::LineTo(x, y)
                }
                OutlineCommand::QuadTo(x1, y1, x, y) => {
                    let (x1, y1) = tx(x1, y1);
                    let (x, y) = tx(x, y);
                    OutlineCommand::QuadTo(x1, y1, x, y)
                }
                OutlineCommand::CurveTo(x1, y1, x2, y2, x, y) => {
                    let (x1, y1) = tx(x1, y1);
                    let (x2, y2) = tx(x2, y2);
                    let (x, y) = tx(x, y);
                    OutlineCommand::CurveTo(x1, y1, x2, y2, x, y)
                }
                OutlineCommand::Close => OutlineCommand::Close,
            };
        }
        outline
    }

    /// Build a `seac` accented composite (base glyph + offset accent glyph).
    fn compose_seac(&self, interp: &Interp, seac: Seac, depth: u32) -> Option<GlyphOutline> {
        let bname = STANDARD_ENCODING
            .get(seac.bchar as usize)
            .copied()
            .flatten()?;
        let aname = STANDARD_ENCODING
            .get(seac.achar as usize)
            .copied()
            .flatten()?;
        let base = self.raw_outline_by_name_depth(bname, depth)?;
        let accent = self.raw_outline_by_name_depth(aname, depth)?;

        let dx = interp.sbx + seac.adx - seac.asb;
        let dy = seac.ady;
        let mut cmds = base.commands;
        cmds.extend(
            accent
                .commands
                .into_iter()
                .map(|c| translate_cmd(c, dx, dy)),
        );
        Some(GlyphOutline {
            commands: cmds,
            advance_width: interp.width,
        })
    }
}

fn has_drawable_segment(cmds: &[OutlineCommand]) -> bool {
    cmds.iter().any(|cmd| {
        matches!(
            cmd,
            OutlineCommand::LineTo(..) | OutlineCommand::QuadTo(..) | OutlineCommand::CurveTo(..)
        )
    })
}

fn translate_cmd(c: OutlineCommand, dx: f64, dy: f64) -> OutlineCommand {
    match c {
        OutlineCommand::MoveTo(x, y) => OutlineCommand::MoveTo(x + dx, y + dy),
        OutlineCommand::LineTo(x, y) => OutlineCommand::LineTo(x + dx, y + dy),
        OutlineCommand::QuadTo(a, b, x, y) => {
            OutlineCommand::QuadTo(a + dx, b + dy, x + dx, y + dy)
        }
        OutlineCommand::CurveTo(a, b, c2, d, x, y) => {
            OutlineCommand::CurveTo(a + dx, b + dy, c2 + dx, d + dy, x + dx, y + dy)
        }
        OutlineCommand::Close => OutlineCommand::Close,
    }
}

#[derive(Clone, Copy)]
struct Seac {
    asb: f64,
    adx: f64,
    ady: f64,
    bchar: u8,
    achar: u8,
}

/// Type 1 charstring interpreter state.
struct Interp {
    cmds: Vec<OutlineCommand>,
    stack: Vec<f64>,
    ps_stack: Vec<f64>,
    x: f64,
    y: f64,
    sbx: f64,
    width: f64,
    contour_open: bool,
    in_flex: bool,
    flex_pts: Vec<(f64, f64)>,
    seac: Option<Seac>,
    done: bool,
    failed: bool,
    steps: usize,
}

impl Interp {
    fn new() -> Self {
        Interp {
            cmds: Vec::new(),
            stack: Vec::new(),
            ps_stack: Vec::new(),
            x: 0.0,
            y: 0.0,
            sbx: 0.0,
            width: 0.0,
            contour_open: false,
            in_flex: false,
            flex_pts: Vec::new(),
            seac: None,
            done: false,
            failed: false,
            steps: 0,
        }
    }

    fn moveto(&mut self, dx: f64, dy: f64) {
        self.x += dx;
        self.y += dy;
        if self.in_flex {
            if self.flex_pts.len() < 7 {
                self.flex_pts.push((self.x, self.y));
            }
            return;
        }
        if self.contour_open {
            self.cmds.push(OutlineCommand::Close);
        }
        self.cmds.push(OutlineCommand::MoveTo(self.x, self.y));
        self.contour_open = true;
    }

    fn lineto(&mut self, dx: f64, dy: f64) {
        self.x += dx;
        self.y += dy;
        self.cmds.push(OutlineCommand::LineTo(self.x, self.y));
    }

    fn curveto(&mut self, dx1: f64, dy1: f64, dx2: f64, dy2: f64, dx3: f64, dy3: f64) {
        let x1 = self.x + dx1;
        let y1 = self.y + dy1;
        let x2 = x1 + dx2;
        let y2 = y1 + dy2;
        self.x = x2 + dx3;
        self.y = y2 + dy3;
        self.cmds
            .push(OutlineCommand::CurveTo(x1, y1, x2, y2, self.x, self.y));
    }

    fn close(&mut self) {
        if self.contour_open {
            self.cmds.push(OutlineCommand::Close);
            self.contour_open = false;
        }
    }

    fn run(&mut self, cs: &[u8], subrs: &[Vec<u8>], depth: u32) {
        if depth > MAX_SUBR_DEPTH {
            self.failed = true;
            return;
        }
        if self.done || self.failed {
            return;
        }
        let mut i = 0;
        while i < cs.len() {
            if self.done || self.failed {
                return;
            }
            self.steps += 1;
            if self.steps > MAX_CHARSTRING_STEPS {
                self.failed = true;
                return;
            }
            let b = cs[i];
            i += 1;
            match b {
                // Numbers.
                32..=246 => self.stack.push(b as f64 - 139.0),
                247..=250 => {
                    if i >= cs.len() {
                        self.failed = true;
                        return;
                    }
                    let w = cs[i] as f64;
                    i += 1;
                    self.stack.push((b as f64 - 247.0) * 256.0 + w + 108.0);
                }
                251..=254 => {
                    if i >= cs.len() {
                        self.failed = true;
                        return;
                    }
                    let w = cs[i] as f64;
                    i += 1;
                    self.stack.push(-(b as f64 - 251.0) * 256.0 - w - 108.0);
                }
                255 => {
                    if i + 4 > cs.len() {
                        self.failed = true;
                        return;
                    }
                    let v = i32::from_be_bytes([cs[i], cs[i + 1], cs[i + 2], cs[i + 3]]);
                    i += 4;
                    self.stack.push(v as f64);
                }
                // Operators.
                13 => {
                    // hsbw: sbx wx
                    if self.stack.len() >= 2 {
                        self.sbx = self.stack[0];
                        self.width = self.stack[1];
                        self.x = self.sbx;
                        self.y = 0.0;
                    }
                    self.stack.clear();
                }
                9 => {
                    // closepath
                    self.close();
                    self.stack.clear();
                }
                21 => {
                    // rmoveto
                    let n = self.stack.len();
                    if n >= 2 {
                        self.moveto(self.stack[n - 2], self.stack[n - 1]);
                    }
                    self.stack.clear();
                }
                22 => {
                    // hmoveto
                    if let Some(&dx) = self.stack.last() {
                        self.moveto(dx, 0.0);
                    }
                    self.stack.clear();
                }
                4 => {
                    // vmoveto
                    if let Some(&dy) = self.stack.last() {
                        self.moveto(0.0, dy);
                    }
                    self.stack.clear();
                }
                5 => {
                    // rlineto
                    let n = self.stack.len();
                    if n >= 2 {
                        self.lineto(self.stack[n - 2], self.stack[n - 1]);
                    }
                    self.stack.clear();
                }
                6 => {
                    // hlineto
                    if let Some(&dx) = self.stack.last() {
                        self.lineto(dx, 0.0);
                    }
                    self.stack.clear();
                }
                7 => {
                    // vlineto
                    if let Some(&dy) = self.stack.last() {
                        self.lineto(0.0, dy);
                    }
                    self.stack.clear();
                }
                8 => {
                    // rrcurveto
                    let n = self.stack.len();
                    if n >= 6 {
                        let s = &self.stack[n - 6..];
                        self.curveto(s[0], s[1], s[2], s[3], s[4], s[5]);
                    }
                    self.stack.clear();
                }
                30 => {
                    // vhcurveto: dy1 dx2 dy2 dx3
                    let n = self.stack.len();
                    if n >= 4 {
                        let s = &self.stack[n - 4..];
                        self.curveto(0.0, s[0], s[1], s[2], s[3], 0.0);
                    }
                    self.stack.clear();
                }
                31 => {
                    // hvcurveto: dx1 dx2 dy2 dy3
                    let n = self.stack.len();
                    if n >= 4 {
                        let s = &self.stack[n - 4..];
                        self.curveto(s[0], 0.0, s[1], s[2], 0.0, s[3]);
                    }
                    self.stack.clear();
                }
                1 | 3 => {
                    // hstem / vstem — hints, ignored for outlines.
                    self.stack.clear();
                }
                10 => {
                    // callsubr
                    if let Some(idx) = self.stack.pop() {
                        let idx = idx as i64;
                        if idx >= 0 && (idx as usize) < subrs.len() {
                            self.run(&subrs[idx as usize], subrs, depth + 1);
                        } else {
                            self.failed = true;
                            return;
                        }
                    } else {
                        self.failed = true;
                        return;
                    }
                }
                11 => {
                    if depth == 0 {
                        self.failed = true;
                    }
                    return; // return
                }
                14 => {
                    // endchar
                    self.close();
                    self.done = true;
                    return;
                }
                12 => {
                    if i >= cs.len() {
                        self.failed = true;
                        return;
                    }
                    let b2 = cs[i];
                    i += 1;
                    self.escape(b2);
                }
                _ => {
                    // Unknown operator: drop operands and continue.
                    self.stack.clear();
                }
            }
            if self.stack.len() > MAX_CHARSTRING_STACK
                || self.ps_stack.len() > MAX_CHARSTRING_STACK
                || self.cmds.len() > MAX_CHARSTRING_STEPS
            {
                self.failed = true;
                return;
            }
        }
        // Adobe Type 1 Font Format §8: a top-level charstring must finish with
        // `endchar` or `seac`, and a subroutine must finish with `return`
        // (exiting early above) or `endchar`/`seac`. Reaching EOF with `!done`
        // at any depth is a truncated charstring or subroutine.
        if !self.done {
            self.failed = true;
        }
    }

    fn escape(&mut self, b2: u8) {
        match b2 {
            0 => self.stack.clear(),     // dotsection
            1 | 2 => self.stack.clear(), // vstem3 / hstem3
            6 => {
                // seac: asb adx ady bchar achar
                if self.stack.len() >= 5 {
                    let s = &self.stack[self.stack.len() - 5..];
                    self.seac = Some(Seac {
                        asb: s[0],
                        adx: s[1],
                        ady: s[2],
                        bchar: s[3] as u8,
                        achar: s[4] as u8,
                    });
                    self.done = true;
                }
                self.stack.clear();
            }
            7 => {
                // sbw: sbx sby wx wy
                if self.stack.len() >= 4 {
                    self.sbx = self.stack[0];
                    self.width = self.stack[2];
                    self.x = self.stack[0];
                    self.y = self.stack[1];
                }
                self.stack.clear();
            }
            12 => {
                // div
                let n = self.stack.len();
                if n >= 2 {
                    let b = self.stack[n - 1];
                    let a = self.stack[n - 2];
                    self.stack.truncate(n - 2);
                    self.stack.push(if b != 0.0 { a / b } else { 0.0 });
                }
            }
            16 => self.callothersubr(),
            17 => {
                // pop: move a value from the PostScript stack to the operand stack.
                let v = self.ps_stack.pop().unwrap_or(0.0);
                self.stack.push(v);
            }
            33 => {
                // setcurrentpoint — operands are the top two values (x y).
                let n = self.stack.len();
                if n >= 2 {
                    self.x = self.stack[n - 2];
                    self.y = self.stack[n - 1];
                }
                self.stack.clear();
            }
            _ => self.stack.clear(),
        }
    }

    fn callothersubr(&mut self) {
        // Operand layout: arg1 .. argN  N  othersubr#
        let othersubr = self.stack.pop().unwrap_or(0.0) as i64;
        let n = self.stack.pop().unwrap_or(0.0) as i64;
        if n < 0 {
            self.failed = true;
            return;
        }
        let n = n as usize;
        if n > self.stack.len() || n > MAX_CHARSTRING_STACK {
            self.failed = true;
            return;
        }
        let args = self.stack.split_off(self.stack.len() - n);

        match othersubr {
            1 => {
                // Start flex: collect the upcoming 7 reference points.
                self.in_flex = true;
                self.flex_pts.clear();
            }
            2 => {
                // Add flex point — the rmoveto already recorded the point.
            }
            0 => {
                // End flex: build two curves from the 7 collected points.
                self.in_flex = false;
                if self.flex_pts.len() >= 7 {
                    let pts = &self.flex_pts;
                    // pts[0] is the reference point; pts[1..4] and pts[4..7] are curves.
                    self.cmds.push(OutlineCommand::CurveTo(
                        pts[1].0, pts[1].1, pts[2].0, pts[2].1, pts[3].0, pts[3].1,
                    ));
                    self.cmds.push(OutlineCommand::CurveTo(
                        pts[4].0, pts[4].1, pts[5].0, pts[5].1, pts[6].0, pts[6].1,
                    ));
                    self.x = pts[6].0;
                    self.y = pts[6].1;
                }
                self.flex_pts.clear();
                // OtherSubr 0 returns the endpoint via two `pop`s consumed by the
                // following `setcurrentpoint` (x y setcurrentpoint). The first `pop`
                // must yield x, the second y, so push y then x.
                self.ps_stack.push(self.y);
                self.ps_stack.push(self.x);
            }
            3 => {
                // Hint replacement: return the subr number for the following `pop`.
                self.ps_stack.push(args.first().copied().unwrap_or(3.0));
            }
            _ => {
                // Unknown OtherSubr: push args back so subsequent `pop`s retrieve them.
                for a in args.into_iter().rev() {
                    self.ps_stack.push(a);
                }
            }
        }
    }
}

// ----- charstring / eexec decryption -----

fn decrypt(data: &[u8], mut r: u16, skip: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len().saturating_sub(skip));
    for (index, &c) in data.iter().enumerate() {
        let p = c ^ (r >> 8) as u8;
        r = (c as u16).wrapping_add(r).wrapping_mul(C1).wrapping_add(C2);
        if index >= skip {
            out.push(p);
        }
    }
    out
}

// ----- PFB / PFA handling -----

/// Reassemble a PFB-segmented font into a flat byte stream; pass through otherwise.
fn depfb(data: &[u8]) -> Vec<u8> {
    if data.first() != Some(&0x80) {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(data.len());
    let mut p = 0;
    while p + 6 <= data.len() {
        if data[p] != 0x80 {
            break;
        }
        let kind = data[p + 1];
        if kind == 3 {
            break; // EOF segment
        }
        let len = u32::from_le_bytes([data[p + 2], data[p + 3], data[p + 4], data[p + 5]]) as usize;
        p += 6;
        let Some(end) = p.checked_add(len) else {
            break;
        };
        if end > data.len() {
            break;
        }
        out.extend_from_slice(&data[p..end]);
        p = end;
    }
    out
}

fn looks_hex(data: &[u8]) -> bool {
    data.iter()
        .filter(|b| !is_ws(**b))
        .take(4)
        .all(|b| b.is_ascii_hexdigit())
        && data.iter().any(|b| !is_ws(*b))
}

fn hex_decode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut hi: Option<u8> = None;
    for &b in data {
        let nib = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => continue,
        };
        match hi {
            None => hi = Some(nib),
            Some(h) => {
                out.push((h << 4) | nib);
                hi = None;
            }
        }
    }
    out
}

// ----- clear-text parsing (FontMatrix, Encoding) -----

fn parse_font_matrix(clear: &[u8]) -> Option<[f64; 6]> {
    let pos = find(clear, b"/FontMatrix")?;
    let open = pos + clear[pos..].iter().position(|&b| b == b'[')?;
    let close = open + clear[open..].iter().position(|&b| b == b']')?;
    let body = std::str::from_utf8(&clear[open + 1..close]).ok()?;
    let mut values = [0.0; 6];
    let mut tokens = body.split_whitespace();
    for value in &mut values {
        let parsed = tokens.next()?.parse::<f64>().ok()?;
        if !parsed.is_finite() {
            return None;
        }
        *value = parsed;
    }
    Some(values)
}

fn parse_builtin_encoding(clear: &[u8]) -> Vec<Option<String>> {
    let mut enc: Vec<Option<String>> = vec![None; 256];
    if let Some(pos) = find(clear, b"/Encoding") {
        let region = &clear[pos..];
        if find_token(region, b"StandardEncoding").is_some() {
            for (i, slot) in enc.iter_mut().enumerate() {
                *slot = STANDARD_ENCODING[i].map(|s| s.to_string());
            }
            return enc;
        }
        // Custom: a sequence of `dup <code> /<name> put`.
        let mut i = 0;
        while let Some(rel) = find(&region[i..], b"dup ") {
            let mut p = i + rel + 4;
            let (code, np) = match read_uint(region, p) {
                Some(v) => v,
                None => {
                    i = p;
                    continue;
                }
            };
            p = np;
            while p < region.len() && is_ws(region[p]) {
                p += 1;
            }
            if p < region.len() && region[p] == b'/' {
                p += 1;
                let start = p;
                while p < region.len() && !is_ws(region[p]) && !is_delim(region[p]) {
                    p += 1;
                }
                if code < 256 && p - start <= MAX_GLYPH_NAME_LEN {
                    let name = String::from_utf8_lossy(&region[start..p]).into_owned();
                    enc[code] = Some(name);
                }
            }
            i = p;
        }
    }
    enc
}

// ----- decrypted private parsing (lenIV, Subrs, CharStrings) -----

/// A `/lenIV` of -1 means the charstrings are stored unencrypted (Type 1 Font
/// Format §7.2); otherwise decrypt and drop the `lenIV` leading bytes.
fn decrypt_charstring(data: &[u8], len_iv: i32) -> Vec<u8> {
    match usize::try_from(len_iv) {
        Ok(skip) => decrypt(data, CHARSTRING_R, skip),
        Err(_) => data.to_vec(),
    }
}

fn parse_len_iv(private: &[u8]) -> Option<i32> {
    let pos = find(private, b"/lenIV")?;
    let (v, _) = read_int(private, pos + 6)?;
    i32::try_from(v).ok()
}

fn parse_subrs(private: &[u8], len_iv: i32) -> Vec<Vec<u8>> {
    let mut subrs: Vec<Vec<u8>> = Vec::new();
    let start = match find(private, b"/Subrs") {
        Some(s) => s,
        None => return subrs,
    };
    // Optional count to pre-size.
    if let Some((count, _)) = read_int(private, start + 6) {
        if (0..MAX_TYPE1_SUBRS as i64).contains(&count) {
            subrs.resize(count as usize, Vec::new());
        }
    }
    let mut i = start;
    // Stop scanning at CharStrings (the next section).
    let end = find(private, b"/CharStrings").unwrap_or(private.len());
    while i < end {
        let rel = match find(&private[i..end], b"dup ") {
            Some(r) => r,
            None => break,
        };
        let mut p = i + rel + 4;
        let idx = match read_int(private, p) {
            Some((v, np)) => {
                p = np;
                v
            }
            None => {
                i = i + rel + 4;
                continue;
            }
        };
        let len = match read_int(private, p) {
            Some((v, np)) => {
                p = np;
                v
            }
            None => {
                i = p;
                continue;
            }
        };
        // Binary token: RD or -|
        let (tok, np) = read_token(private, p);
        p = np;
        if tok != b"RD" && tok != b"-|" {
            i = p;
            continue;
        }
        let Some(data_start) = p.checked_add(1) else {
            break;
        }; // single separator space
        let len = len.max(0) as usize;
        let Some(end) = data_start.checked_add(len) else {
            break;
        };
        if len > private.len() || end > private.len() {
            break;
        }
        let cs = decrypt_charstring(&private[data_start..end], len_iv);
        if idx >= 0 {
            let idx = idx as usize;
            if idx >= MAX_TYPE1_SUBRS {
                i = end;
                continue;
            }
            if idx >= subrs.len() {
                subrs.resize(idx + 1, Vec::new());
            }
            subrs[idx] = cs;
        }
        i = end;
    }
    subrs
}

fn parse_charstrings(private: &[u8], len_iv: i32) -> HashMap<String, Vec<u8>> {
    let mut out = HashMap::new();
    let start = match find(private, b"/CharStrings") {
        Some(s) => s,
        None => return out,
    };
    // Skip to the dictionary body (after "begin").
    let mut i = match find(&private[start..], b"begin") {
        Some(b) => start + b + 5,
        None => start + 12,
    };
    while i < private.len() {
        if out.len() >= MAX_TYPE1_GLYPHS {
            break;
        }
        // Next glyph entry starts with '/'.
        let rel = match private[i..].iter().position(|&b| b == b'/') {
            Some(r) => r,
            None => break,
        };
        let mut p = i + rel + 1;
        let name_start = p;
        while p < private.len() && !is_ws(private[p]) && !is_delim(private[p]) {
            p += 1;
        }
        if p - name_start > MAX_GLYPH_NAME_LEN {
            i = p;
            continue;
        }
        let name = String::from_utf8_lossy(&private[name_start..p]).into_owned();

        let len = match read_int(private, p) {
            Some((v, np)) => {
                p = np;
                v
            }
            None => {
                i = p;
                continue;
            }
        };
        let (tok, np) = read_token(private, p);
        p = np;
        if tok != b"RD" && tok != b"-|" {
            // Not a charstring entry (e.g. reached "end"); stop.
            break;
        }
        let Some(data_start) = p.checked_add(1) else {
            break;
        }; // single separator space
        let len = len.max(0) as usize;
        let Some(end) = data_start.checked_add(len) else {
            break;
        };
        if end > private.len() {
            break;
        }
        let cs = decrypt_charstring(&private[data_start..end], len_iv);
        out.insert(name, cs);
        i = end;
    }
    out
}

// ----- byte helpers -----

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b'\x00' | b'\x0c')
}

fn is_delim(b: u8) -> bool {
    matches!(
        b,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Find a needle only when it stands as a whole token (delimited by whitespace).
fn find_token(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let mut from = 0;
    while let Some(rel) = find(&haystack[from..], needle) {
        let pos = from + rel;
        let before_ok = pos == 0 || is_ws(haystack[pos - 1]);
        let after = pos + needle.len();
        let after_ok =
            after >= haystack.len() || is_ws(haystack[after]) || is_delim(haystack[after]);
        if before_ok && after_ok {
            return Some(pos);
        }
        from = pos + 1;
    }
    None
}

/// Read a signed integer starting at/after `pos` (skipping leading whitespace).
fn read_int(data: &[u8], mut pos: usize) -> Option<(i64, usize)> {
    while pos < data.len() && is_ws(data[pos]) {
        pos += 1;
    }
    let start = pos;
    if pos < data.len() && (data[pos] == b'-' || data[pos] == b'+') {
        pos += 1;
    }
    let digit_start = pos;
    while pos < data.len() && data[pos].is_ascii_digit() {
        pos += 1;
    }
    if pos == digit_start {
        return None;
    }
    let v: i64 = std::str::from_utf8(&data[start..pos]).ok()?.parse().ok()?;
    Some((v, pos))
}

fn read_uint(data: &[u8], pos: usize) -> Option<(usize, usize)> {
    let (v, np) = read_int(data, pos)?;
    if v < 0 {
        None
    } else {
        Some((v as usize, np))
    }
}

/// Read a whitespace-delimited token (skipping leading whitespace).
fn read_token(data: &[u8], mut pos: usize) -> (&[u8], usize) {
    while pos < data.len() && is_ws(data[pos]) {
        pos += 1;
    }
    let start = pos;
    while pos < data.len() && !is_ws(data[pos]) {
        pos += 1;
    }
    (&data[start..pos], pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decrypt_roundtrip() {
        // Encrypt then decrypt yields the original (minus the random skip bytes).
        let plain = b"\x00\x00\x00\x00hello type1";
        let mut r = EEXEC_R;
        let mut enc = Vec::new();
        for &p in plain.iter() {
            let c = p ^ (r >> 8) as u8;
            r = (c as u16).wrapping_add(r).wrapping_mul(C1).wrapping_add(C2);
            enc.push(c);
        }
        let dec = decrypt(&enc, EEXEC_R, 4);
        assert_eq!(&dec, b"hello type1");
    }

    #[test]
    fn negative_len_iv_keeps_charstrings_unencrypted() {
        // Founder "FzBookMaker" subsets declare `/lenIV -1` and store plain
        // charstrings; decrypting them anyway scrambles every outline.
        let plain: &[u8] = &[0x8B, 0xF8, 0x88, 0x0D, 0x0E];
        let private = [
            b"/lenIV -1 def /Subrs 1 array dup 0 5 RD ".as_slice(),
            plain,
            b" NP ND /CharStrings 1 dict dup begin /a 5 RD ".as_slice(),
            plain,
            b" ND end".as_slice(),
        ]
        .concat();
        let len_iv = parse_len_iv(&private).unwrap();
        assert_eq!(len_iv, -1);
        assert_eq!(parse_subrs(&private, len_iv), vec![plain.to_vec()]);
        assert_eq!(parse_charstrings(&private, len_iv)["a"], plain);
    }

    #[test]
    fn number_operand_parsing() {
        // 139 -> 0, 'b'=255 then 4 bytes -> i32. Use a tiny synthetic charstring:
        // 0x8B (=139 -> 0), then 0xFF 00 00 01 00 (-> 256), then endchar.
        let cs = [0x8B, 0xFF, 0x00, 0x00, 0x01, 0x00, 14];
        let mut interp = Interp::new();
        interp.run(&cs, &[], 0);
        // Stack consumed by endchar path; just assert it doesn't panic and closes.
        assert!(interp.done);
    }

    #[test]
    fn depfb_passthrough_for_plain() {
        let data = b"%!FontType1 plain";
        assert_eq!(depfb(data), data);
    }

    #[test]
    fn find_token_is_whole_word() {
        let hay = b"/Encoding StandardEncoding def";
        assert!(find_token(hay, b"StandardEncoding").is_some());
        assert!(find_token(b"xStandardEncodingy", b"StandardEncoding").is_none());
    }

    #[test]
    fn parse_font_matrix_reads_full_six_tuple() {
        let clear = b"/FontMatrix [0.001 0 0.000167 0.001 0 0] readonly def";
        let m = parse_font_matrix(clear).expect("matrix");
        assert_eq!(m, [0.001, 0.0, 0.000167, 0.001, 0.0, 0.0]);
    }

    /// Type1Font with a single glyph "A":
    /// hsbw 0 500; rmoveto 0 100; rlineto 100 0; endchar.
    fn font_with_matrix(font_matrix: [f64; 6]) -> Type1Font {
        let mut char_strings = HashMap::new();
        // 139 = 0; (248, 136) = 500; 13 = hsbw; 239 = 100;
        // 21 = rmoveto; 5 = rlineto; 14 = endchar.
        char_strings.insert(
            "A".to_string(),
            vec![139, 248, 136, 13, 139, 239, 21, 239, 139, 5, 14],
        );
        Type1Font {
            char_strings,
            subrs: Vec::new(),
            builtin_encoding: vec![None; 256],
            gid_to_name: vec!["A".to_string()],
            name_to_gid: HashMap::from([("A".to_string(), 0u16)]),
            units_per_em: 1000.0,
            font_matrix,
        }
    }

    #[test]
    fn standard_font_matrix_leaves_outline_untouched() {
        let font = font_with_matrix([0.001, 0.0, 0.0, 0.001, 0.0, 0.0]);
        let outline = font.glyph_outline_by_name("A").expect("outline");
        assert_eq!(outline.advance_width, 500.0);
        match outline.commands[0] {
            OutlineCommand::MoveTo(x, y) => assert_eq!((x, y), (0.0, 100.0)),
            ref c => panic!("expected MoveTo, got {c:?}"),
        }
        match outline.commands[1] {
            OutlineCommand::LineTo(x, y) => assert_eq!((x, y), (100.0, 100.0)),
            ref c => panic!("expected LineTo, got {c:?}"),
        }
    }

    #[test]
    fn slanted_font_matrix_skews_outline() {
        // dvips SlantFont 0.25: [0.001 0 0.00025 0.001 0 0].
        let font = font_with_matrix([0.001, 0.0, 0.00025, 0.001, 0.0, 0.0]);
        let outline = font.glyph_outline_by_name("A").expect("outline");
        // Advance is the matrix x-scale times upem — unchanged.
        assert_eq!(outline.advance_width, 500.0);
        // (0, 100) → x' = (0 + 0.00025*100) * 1000 = 25, y' = 100.
        match outline.commands[0] {
            OutlineCommand::MoveTo(x, y) => {
                assert!((x - 25.0).abs() < 1e-9, "x = {x}");
                assert!((y - 100.0).abs() < 1e-9, "y = {y}");
            }
            ref c => panic!("expected MoveTo, got {c:?}"),
        }
        // (100, 100) → x' = (0.1 + 0.025) * 1000 = 125, y' = 100.
        match outline.commands[1] {
            OutlineCommand::LineTo(x, y) => {
                assert!((x - 125.0).abs() < 1e-9, "x = {x}");
                assert!((y - 100.0).abs() < 1e-9, "y = {y}");
            }
            ref c => panic!("expected LineTo, got {c:?}"),
        }
    }

    #[test]
    fn extended_font_matrix_rescales_y_relative_to_x() {
        // ExtendFont 1.2: x-scale 0.0012, y-scale 0.001. units_per_em = 1/a,
        // so x stays put and y shrinks by d/a relative to it.
        let font = Type1Font {
            units_per_em: 1.0 / 0.0012,
            ..font_with_matrix([0.0012, 0.0, 0.0, 0.001, 0.0, 0.0])
        };
        let outline = font.glyph_outline_by_name("A").expect("outline");
        match outline.commands[1] {
            OutlineCommand::LineTo(x, y) => {
                assert!((x - 100.0).abs() < 1e-9, "x = {x}");
                assert!((y - 100.0 * 0.001 / 0.0012).abs() < 1e-9, "y = {y}");
            }
            ref c => panic!("expected LineTo, got {c:?}"),
        }
    }

    #[test]
    fn cyclic_seac_composites_are_bounded() {
        let mut font = font_with_matrix([0.001, 0.0, 0.0, 0.001, 0.0, 0.0]);
        // seac 0 0 0 /A /A: both StandardEncoding character codes are 65.
        font.char_strings
            .insert("A".to_string(), vec![139, 139, 139, 204, 204, 12, 6]);
        assert!(font.glyph_outline_by_name("A").is_none());
        assert!(!font.has_usable_outlines());
    }

    #[test]
    fn seac_composes_base_and_accent_but_not_nested_composites() {
        let mut font = font_with_matrix([0.001, 0.0, 0.0, 0.001, 0.0, 0.0]);
        let simple = font.char_strings["A"].clone();
        font.char_strings.insert("grave".to_string(), simple);
        // seac 0 0 0 65 193: /A with /grave (StandardEncoding 193).
        let agrave = vec![139, 139, 139, 204, 247, 85, 12, 6];
        font.char_strings
            .insert("Agrave".to_string(), agrave.clone());
        let base = font.glyph_outline_by_name("A").expect("base");
        let outline = font.glyph_outline_by_name("Agrave").expect("composite");
        assert_eq!(
            outline.commands.len(),
            2 * base.commands.len(),
            "base and accent outlines"
        );

        // A composite-only font with valid base/accent is usable; once a
        // referenced component is missing, it is no longer usable.
        let mut composite_only = font_with_matrix([0.001, 0.0, 0.0, 0.001, 0.0, 0.0]);
        let simple_a = composite_only.char_strings["A"].clone();
        composite_only
            .char_strings
            .insert("grave".to_string(), simple_a);
        composite_only
            .char_strings
            .insert("Agrave".to_string(), agrave.clone());
        composite_only.gid_to_name = vec![".notdef".to_string(), "Agrave".to_string()];
        assert!(composite_only.has_usable_outlines());
        composite_only.char_strings.remove("A");
        assert!(!composite_only.has_usable_outlines());

        // A composite whose base is itself a composite is malformed.
        font.char_strings.insert("A".to_string(), agrave);
        assert!(font.glyph_outline_by_name("Agrave").is_none());
    }

    #[test]
    fn oversized_othersubr_argument_count_is_rejected_without_allocating() {
        let mut cs = vec![255];
        cs.extend_from_slice(&i32::MAX.to_be_bytes()); // argument count
        cs.push(139); // OtherSubr 0
        cs.extend_from_slice(&[12, 16]); // callothersubr
        let mut interp = Interp::new();
        interp.run(&cs, &[], 0);
        assert!(interp.failed);
    }

    #[test]
    fn sparse_huge_subr_index_does_not_resize_the_table() {
        let private = b"/Subrs 0 array dup 999999 1 RD x /CharStrings";
        assert!(parse_subrs(private, 0).is_empty());
    }

    #[test]
    fn no_ink_only_type1_subset_stays_healthy_while_empty_visible_is_unusable() {
        // hsbw(0, 250) + endchar (14): valid empty charstring.
        let empty_cs = vec![139, 247, 142, 13, 14];

        let mut no_ink = font_with_matrix([0.001, 0.0, 0.0, 0.001, 0.0, 0.0]);
        no_ink.char_strings.clear();
        no_ink
            .char_strings
            .insert(".notdef".into(), empty_cs.clone());
        no_ink
            .char_strings
            .insert("uni0020".into(), empty_cs.clone());
        no_ink
            .char_strings
            .insert("uni200B".into(), empty_cs.clone());
        no_ink.gid_to_name = vec![".notdef".into(), "uni0020".into(), "uni200B".into()];
        no_ink.name_to_gid = HashMap::from([
            (".notdef".into(), 0),
            ("uni0020".into(), 1),
            ("uni200B".into(), 2),
        ]);
        assert_eq!(
            no_ink.probe_outlines(),
            crate::EmbeddedOutlineProbe::Healthy
        );
        assert!(no_ink.has_usable_outlines());
        assert_eq!(
            no_ink.glyph_state_by_gid(1),
            crate::resolution::GlyphState::Blank
        );
        assert!(!no_ink.is_gid_unreadable(1));
        assert!(no_ink.is_blank_gid(1));

        // An empty or terminated move-only (`hsbw`, `rmoveto`, `endchar`)
        // charstring for a known visible glyph name (/A) has no drawable path
        // segments, so it must count as failed / Missing rather than Drawn.
        let move_only_cs = vec![139, 248, 136, 13, 189, 139, 21, 14];
        for (label, cs) in [
            ("empty-visible-A", empty_cs.clone()),
            ("move-only-visible-A", move_only_cs.clone()),
        ] {
            let mut bad_visible = font_with_matrix([0.001, 0.0, 0.0, 0.001, 0.0, 0.0]);
            bad_visible.char_strings.clear();
            bad_visible
                .char_strings
                .insert(".notdef".into(), empty_cs.clone());
            bad_visible.char_strings.insert("A".into(), cs);
            bad_visible.gid_to_name = vec![".notdef".into(), "A".into()];
            bad_visible.name_to_gid = HashMap::from([(".notdef".into(), 0), ("A".into(), 1)]);
            assert_eq!(
                bad_visible.probe_outlines(),
                crate::EmbeddedOutlineProbe::Unusable,
                "{label}"
            );
            assert!(!bad_visible.has_usable_outlines(), "{label}");
            assert_eq!(
                bad_visible.glyph_state_by_gid(1),
                crate::resolution::GlyphState::Missing,
                "{label}"
            );
            assert!(bad_visible.is_gid_unreadable(1), "{label}");
            assert!(bad_visible.glyph_outline_by_gid(1).is_none(), "{label}");
        }

        // A terminated move-only space stays Healthy / Blank.
        let mut move_only_space = font_with_matrix([0.001, 0.0, 0.0, 0.001, 0.0, 0.0]);
        move_only_space.char_strings.clear();
        move_only_space
            .char_strings
            .insert(".notdef".into(), empty_cs.clone());
        move_only_space
            .char_strings
            .insert("uni0020".into(), move_only_cs);
        move_only_space.gid_to_name = vec![".notdef".into(), "uni0020".into()];
        move_only_space.name_to_gid = HashMap::from([(".notdef".into(), 0), ("uni0020".into(), 1)]);
        assert_eq!(
            move_only_space.probe_outlines(),
            crate::EmbeddedOutlineProbe::Healthy
        );
        assert_eq!(
            move_only_space.glyph_state_by_gid(1),
            crate::resolution::GlyphState::Blank
        );
        assert!(!move_only_space.is_gid_unreadable(1));

        // Truncated charstrings on an unknown glyph name (/g1) — ending
        // mid-operand, mid-escape, or without endchar — must be classified as
        // unreadable in both `probe_outlines` and per-glyph checks (never
        // accepted as Healthy/Blank).
        for (label, truncated_cs) in [
            ("mid-2byte-pos-operand", vec![139u8, 247]),
            ("mid-2byte-neg-operand", vec![139, 251]),
            ("mid-5byte-operand", vec![255, 0, 1]),
            ("mid-escape-op", vec![139, 12]),
            ("missing-endchar", vec![139, 247, 142, 13]),
            ("top-level-return", vec![139, 11]),
            ("empty-bytes", vec![]),
        ] {
            let mut truncated = font_with_matrix([0.001, 0.0, 0.0, 0.001, 0.0, 0.0]);
            truncated.char_strings.clear();
            truncated
                .char_strings
                .insert(".notdef".into(), empty_cs.clone());
            truncated.char_strings.insert("g1".into(), truncated_cs);
            truncated.gid_to_name = vec![".notdef".into(), "g1".into()];
            truncated.name_to_gid = HashMap::from([(".notdef".into(), 0), ("g1".into(), 1)]);
            assert_eq!(
                truncated.probe_outlines(),
                crate::EmbeddedOutlineProbe::Unusable,
                "{label}: truncated /g1 subset must be unusable"
            );
            assert!(!truncated.has_usable_outlines(), "{label}");
            assert_eq!(
                truncated.glyph_state_by_gid(1),
                crate::resolution::GlyphState::Missing,
                "{label}: truncated /g1 must not be classified as Blank"
            );
            assert!(truncated.is_gid_unreadable(1), "{label}");
            assert!(!truncated.is_blank_gid(1), "{label}");
            assert!(truncated.glyph_outline_by_gid(1).is_none(), "{label}");
        }
    }

    #[test]
    fn subroutine_unexpected_eof_fails_while_explicit_return_succeeds() {
        let empty_cs = vec![139, 247, 142, 13, 14];
        // /g1 delegates its strokes to subr 0 and then executes endchar:
        // hsbw(0, 500), 0 callsubr, endchar.
        let caller_cs = vec![139, 248, 136, 13, 139, 10, 14];
        let make_font = |subrs: Vec<Vec<u8>>| {
            let mut font = font_with_matrix([0.001, 0.0, 0.0, 0.001, 0.0, 0.0]);
            font.char_strings.clear();
            font.char_strings.insert(".notdef".into(), empty_cs.clone());
            font.char_strings.insert("g1".into(), caller_cs.clone());
            font.gid_to_name = vec![".notdef".into(), "g1".into()];
            font.name_to_gid = HashMap::from([(".notdef".into(), 0), ("g1".into(), 1)]);
            font.subrs = subrs;
            font
        };

        // Unexpected EOF inside a subroutine (empty /Subrs[0], strokes without
        // `return`, or missing /Subrs entry) must fail even though the caller
        // has a trailing `endchar`.
        for (label, subrs) in [
            ("empty-subr-eof", vec![vec![]]),
            (
                "strokes-without-return",
                vec![vec![139, 239, 21, 239, 139, 5]],
            ),
            ("missing-subr-index", vec![]),
        ] {
            let font = make_font(subrs);
            assert_eq!(
                font.probe_outlines(),
                crate::EmbeddedOutlineProbe::Unusable,
                "{label}"
            );
            assert!(!font.has_usable_outlines(), "{label}");
            assert_eq!(
                font.glyph_state_by_gid(1),
                crate::resolution::GlyphState::Missing,
                "{label}"
            );
            assert!(font.is_gid_unreadable(1), "{label}");
            assert!(!font.is_blank_gid(1), "{label}");
            assert!(font.glyph_outline_by_gid(1).is_none(), "{label}");
        }

        // Explicit `return` (11) exits cleanly and lets the caller's `endchar`
        // complete: both a no-ink `return` and a stroked `return` (or `endchar`
        // inside the subr) remain healthy.
        let blank_ret = make_font(vec![vec![11]]);
        assert_eq!(
            blank_ret.probe_outlines(),
            crate::EmbeddedOutlineProbe::Healthy
        );
        assert_eq!(
            blank_ret.glyph_state_by_gid(1),
            crate::resolution::GlyphState::Blank
        );
        assert!(!blank_ret.is_gid_unreadable(1));
        assert!(blank_ret.is_blank_gid(1));

        for (label, subr) in [
            (
                "explicit-return-strokes",
                vec![139, 239, 21, 239, 139, 5, 11],
            ),
            ("subr-endchar-strokes", vec![139, 239, 21, 239, 139, 5, 14]),
        ] {
            let font = make_font(vec![subr]);
            assert_eq!(
                font.probe_outlines(),
                crate::EmbeddedOutlineProbe::Healthy,
                "{label}"
            );
            assert_eq!(
                font.glyph_state_by_gid(1),
                crate::resolution::GlyphState::Drawn,
                "{label}"
            );
            assert!(!font.is_gid_unreadable(1), "{label}");
            assert!(font.glyph_outline_by_gid(1).is_some(), "{label}");
        }
    }
}
