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
}

impl Type1Font {
    /// Parse a Type 1 font program. Returns `None` if it does not look like one.
    pub fn parse(data: &[u8]) -> Option<Type1Font> {
        let data = depfb(data);
        let eexec_pos = find(&data, b"eexec")?;
        let clear = &data[..eexec_pos];

        // Binary (or hex) eexec section begins after `eexec` + whitespace.
        let mut p = eexec_pos + 5;
        while p < data.len() && is_ws(data[p]) {
            p += 1;
        }
        let enc = &data[p..];
        let binary = if looks_hex(enc) {
            hex_decode(enc)
        } else {
            enc.to_vec()
        };
        let private = decrypt(&binary, EEXEC_R, 4);

        let units_per_em = parse_font_matrix(clear)
            .map(|m| {
                if m[0].abs() > 1e-12 {
                    1.0 / m[0]
                } else {
                    1000.0
                }
            })
            .unwrap_or(1000.0);
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
        let name_to_gid: HashMap<String, u16> = names
            .iter()
            .enumerate()
            .take(u16::MAX as usize)
            .map(|(i, n)| (n.clone(), i as u16))
            .collect();

        Some(Type1Font {
            char_strings,
            subrs,
            builtin_encoding,
            gid_to_name: names,
            name_to_gid,
            units_per_em,
        })
    }

    /// Synthetic glyph id for a glyph name.
    pub fn gid_for_name(&self, name: &str) -> Option<u16> {
        self.name_to_gid.get(name).copied()
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

    /// Outline for a glyph name, interpreting its charstring.
    pub fn glyph_outline_by_name(&self, name: &str) -> Option<GlyphOutline> {
        let cs = self.char_strings.get(name)?;
        let mut interp = Interp::new(&self.subrs);
        interp.run(cs, 0);

        if let Some(seac) = interp.seac {
            return self.compose_seac(&interp, seac);
        }
        if interp.cmds.is_empty() {
            return None;
        }
        Some(GlyphOutline {
            commands: interp.cmds,
            advance_width: interp.width,
        })
    }

    /// Build a `seac` accented composite (base glyph + offset accent glyph).
    fn compose_seac(&self, interp: &Interp, seac: Seac) -> Option<GlyphOutline> {
        let bname = STANDARD_ENCODING
            .get(seac.bchar as usize)
            .copied()
            .flatten()?;
        let aname = STANDARD_ENCODING
            .get(seac.achar as usize)
            .copied()
            .flatten()?;
        let base = self.glyph_outline_by_name(bname)?;
        let accent = self.glyph_outline_by_name(aname)?;

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
struct Interp<'a> {
    subrs: &'a [Vec<u8>],
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
}

impl<'a> Interp<'a> {
    fn new(subrs: &'a [Vec<u8>]) -> Self {
        Interp {
            subrs,
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
        }
    }

    fn moveto(&mut self, dx: f64, dy: f64) {
        self.x += dx;
        self.y += dy;
        if self.in_flex {
            self.flex_pts.push((self.x, self.y));
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

    fn run(&mut self, cs: &[u8], depth: u32) {
        if depth > MAX_SUBR_DEPTH || self.done {
            return;
        }
        let mut i = 0;
        while i < cs.len() {
            if self.done {
                return;
            }
            let b = cs[i];
            i += 1;
            match b {
                // Numbers.
                32..=246 => self.stack.push(b as f64 - 139.0),
                247..=250 => {
                    if i >= cs.len() {
                        break;
                    }
                    let w = cs[i] as f64;
                    i += 1;
                    self.stack.push((b as f64 - 247.0) * 256.0 + w + 108.0);
                }
                251..=254 => {
                    if i >= cs.len() {
                        break;
                    }
                    let w = cs[i] as f64;
                    i += 1;
                    self.stack.push(-(b as f64 - 251.0) * 256.0 - w - 108.0);
                }
                255 => {
                    if i + 4 > cs.len() {
                        break;
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
                        if idx >= 0 && (idx as usize) < self.subrs.len() {
                            let sub = self.subrs[idx as usize].clone();
                            self.run(&sub, depth + 1);
                        }
                    }
                }
                11 => return, // return
                14 => {
                    // endchar
                    self.close();
                    self.done = true;
                    return;
                }
                12 => {
                    if i >= cs.len() {
                        break;
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
        let n = n.max(0) as usize;
        let mut args = Vec::with_capacity(n);
        for _ in 0..n {
            args.push(self.stack.pop().unwrap_or(0.0));
        }
        args.reverse(); // now in original order

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
    let mut out = Vec::with_capacity(data.len());
    for &c in data {
        let p = c ^ (r >> 8) as u8;
        r = (c as u16).wrapping_add(r).wrapping_mul(C1).wrapping_add(C2);
        out.push(p);
    }
    if out.len() >= skip {
        out.drain(0..skip);
    } else {
        out.clear();
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
        if p + len > data.len() {
            break;
        }
        out.extend_from_slice(&data[p..p + len]);
        p += len;
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
    let nums: Vec<f64> = body
        .split_whitespace()
        .filter_map(|t| t.parse::<f64>().ok())
        .collect();
    if nums.len() >= 6 {
        Some([nums[0], nums[1], nums[2], nums[3], nums[4], nums[5]])
    } else {
        None
    }
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
                let name = String::from_utf8_lossy(&region[start..p]).into_owned();
                if code < 256 {
                    enc[code] = Some(name);
                }
            }
            i = p;
        }
    }
    enc
}

// ----- decrypted private parsing (lenIV, Subrs, CharStrings) -----

fn parse_len_iv(private: &[u8]) -> Option<i32> {
    let pos = find(private, b"/lenIV")?;
    let (v, _) = read_int(private, pos + 6)?;
    Some(v as i32)
}

fn parse_subrs(private: &[u8], len_iv: i32) -> Vec<Vec<u8>> {
    let mut subrs: Vec<Vec<u8>> = Vec::new();
    let start = match find(private, b"/Subrs") {
        Some(s) => s,
        None => return subrs,
    };
    // Optional count to pre-size.
    if let Some((count, _)) = read_int(private, start + 6) {
        if (0..100_000).contains(&count) {
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
        if tok != "RD" && tok != "-|" {
            i = p;
            continue;
        }
        p += 1; // single separator space
        let len = len.max(0) as usize;
        if len > private.len() || p + len > private.len() {
            break;
        }
        let cs = decrypt(&private[p..p + len], CHARSTRING_R, len_iv.max(0) as usize);
        if idx >= 0 {
            let idx = idx as usize;
            if idx >= subrs.len() {
                subrs.resize(idx + 1, Vec::new());
            }
            subrs[idx] = cs;
        }
        i = p + len;
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
        if tok != "RD" && tok != "-|" {
            // Not a charstring entry (e.g. reached "end"); stop.
            break;
        }
        p += 1; // single separator space
        let len = len.max(0) as usize;
        if p + len > private.len() {
            break;
        }
        let cs = decrypt(&private[p..p + len], CHARSTRING_R, len_iv.max(0) as usize);
        out.insert(name, cs);
        i = p + len;
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
fn read_token(data: &[u8], mut pos: usize) -> (String, usize) {
    while pos < data.len() && is_ws(data[pos]) {
        pos += 1;
    }
    let start = pos;
    while pos < data.len() && !is_ws(data[pos]) {
        pos += 1;
    }
    (String::from_utf8_lossy(&data[start..pos]).into_owned(), pos)
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
    fn number_operand_parsing() {
        // 139 -> 0, 'b'=255 then 4 bytes -> i32. Use a tiny synthetic charstring:
        // 0x8B (=139 -> 0), then 0xFF 00 00 01 00 (-> 256), then endchar.
        let cs = [0x8B, 0xFF, 0x00, 0x00, 0x01, 0x00, 14];
        let mut interp = Interp::new(&[]);
        interp.run(&cs, 0);
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
}
