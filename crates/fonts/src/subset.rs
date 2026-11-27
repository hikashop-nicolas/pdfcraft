//! Glyph-keeping subsets of an embedded font program. Every glyph keeps its id, so the codes,
//! widths and CIDToGIDMap a PDF already has stay right; the glyphs not kept lose their outlines,
//! the cmap keeps only the kept glyphs, and the tables a PDF reader never uses are left out.

use std::collections::BTreeSet;

/// fsType bit 8: the font may only be embedded whole.
pub const NO_SUBSETTING: u16 = 0x0100;

/// Tables a PDF reader, or this engine reading the program back, uses.
const KEPT: [&[u8; 4]; 15] =
    [b"cmap", b"head", b"hhea", b"hmtx", b"maxp", b"OS/2", b"name", b"post", b"glyf", b"loca", b"cvt ", b"fpgm", b"prep", b"gasp", b"CFF "];

fn u16_at(d: &[u8], at: usize) -> Option<u16> {
    d.get(at..at.checked_add(2)?).map(|b| u16::from_be_bytes([b[0], b[1]]))
}

fn u32_at(d: &[u8], at: usize) -> Option<u32> {
    d.get(at..at.checked_add(4)?).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// The font's fsType, or 0 without an OS/2 table.
pub fn fs_type(bytes: &[u8]) -> u16 {
    tables(bytes).ok().and_then(|t| t.into_iter().find(|(tag, _)| tag == b"OS/2")).and_then(|(_, d)| u16_at(d, 8)).unwrap_or(0)
}

/// A table's tag and data.
type Table<'a> = ([u8; 4], &'a [u8]);

/// The table records of a standalone sfnt.
fn tables(bytes: &[u8]) -> Result<Vec<Table<'_>>, String> {
    let count = u16_at(bytes, 4).ok_or("not a font program")?;
    let mut out = Vec::with_capacity(usize::from(count));
    for i in 0..usize::from(count) {
        let rec = 12 + i * 16;
        let tag: [u8; 4] = bytes.get(rec..rec + 4).and_then(|t| t.try_into().ok()).ok_or("a table record is cut off")?;
        let offset = u32_at(bytes, rec + 8).ok_or("a table record is cut off")? as usize;
        let length = u32_at(bytes, rec + 12).ok_or("a table record is cut off")? as usize;
        let data = bytes.get(offset..offset.checked_add(length).ok_or("a table is too long")?).ok_or("a table lies outside the font")?;
        out.push((tag, data));
    }
    Ok(out)
}

fn table<'a>(list: &[Table<'a>], tag: &[u8; 4]) -> Option<&'a [u8]> {
    list.iter().find(|(t, _)| t == tag).map(|(_, d)| *d)
}

/// A subset of `bytes` that draws only `keep` (glyph 0 is always kept, and so is every glyph a
/// kept composite glyph is made of).
pub fn subset(bytes: &[u8], keep: &BTreeSet<u16>) -> Result<Vec<u8>, String> {
    use skrifa::MetadataProvider as _;
    let list = tables(bytes)?;
    let font = skrifa::FontRef::new(bytes).map_err(|e| e.to_string())?;
    let num_glyphs = table(&list, b"maxp").and_then(|m| u16_at(m, 4)).ok_or("the font has no maxp table")?;
    let mut keep: BTreeSet<u16> = keep.iter().copied().filter(|g| *g < num_glyphs).collect();
    keep.insert(0);
    let mut out: Vec<([u8; 4], Vec<u8>)> = Vec::new();
    let mut head = table(&list, b"head").ok_or("the font has no head table")?.to_vec();
    if head.len() < 54 {
        return Err("the head table is cut off".into());
    }
    if let (Some(glyf), Some(loca)) = (table(&list, b"glyf"), table(&list, b"loca")) {
        let long = u16_at(&head, 50) == Some(1);
        let offset = |g: u16| -> Option<usize> {
            let g = usize::from(g);
            if long { u32_at(loca, g * 4).map(|v| v as usize) } else { u16_at(loca, g * 2).map(|v| usize::from(v) * 2) }
        };
        let glyph = |g: u16| -> &[u8] {
            match (offset(g), g.checked_add(1).and_then(offset)) {
                (Some(a), Some(b)) if a < b => glyf.get(a..b).unwrap_or_default(),
                _ => &[],
            }
        };
        // Components of composite glyphs, transitively.
        let mut pending: Vec<u16> = keep.iter().copied().collect();
        while let Some(g) = pending.pop() {
            for c in components(glyph(g)) {
                if c < num_glyphs && keep.insert(c) {
                    pending.push(c);
                }
            }
        }
        let mut new_glyf = Vec::new();
        let mut new_loca = Vec::with_capacity((usize::from(num_glyphs) + 1) * 4);
        for g in 0..num_glyphs {
            new_loca.extend_from_slice(&(new_glyf.len() as u32).to_be_bytes());
            if keep.contains(&g) {
                new_glyf.extend_from_slice(glyph(g));
                while new_glyf.len() % 4 != 0 {
                    new_glyf.push(0);
                }
            }
        }
        new_loca.extend_from_slice(&(new_glyf.len() as u32).to_be_bytes());
        head[50..52].copy_from_slice(&1u16.to_be_bytes());
        out.push((*b"glyf", new_glyf));
        out.push((*b"loca", new_loca));
    } else if let Some(cff) = table(&list, b"CFF ") {
        out.push((*b"CFF ", blank_charstrings(cff, &keep)?));
    } else {
        return Err("the font has no TrueType or CFF outlines".into());
    }
    // The cmap keeps the characters of kept glyphs, so a reader of the program finds no character
    // whose outline was dropped.
    let mut pairs: Vec<(u32, u16)> = font
        .charmap()
        .mappings()
        .filter_map(|(c, g)| u16::try_from(g.to_u32()).ok().filter(|g| *g != 0 && keep.contains(g)).map(|g| (c, g)))
        .collect();
    pairs.sort_unstable();
    out.push((*b"cmap", cmap12(&pairs)));
    // Metrics of the glyphs dropped go to zero (the PDF's own widths are what readers use).
    if let (Some(hmtx), Some(long)) = (table(&list, b"hmtx"), table(&list, b"hhea").and_then(|h| u16_at(h, 34))) {
        let mut m = hmtx.to_vec();
        for g in 0..num_glyphs {
            if keep.contains(&g) {
                continue;
            }
            let at = if g < long { usize::from(g) * 4 } else { usize::from(long) * 4 + usize::from(g - long) * 2 };
            let len = if g < long { 4 } else { 2 };
            if let Some(entry) = m.get_mut(at..at + len) {
                entry.fill(0);
            }
        }
        out.push((*b"hmtx", m));
    }
    if let Some(post) = table(&list, b"post").filter(|p| p.len() >= 32) {
        // Version 3: no glyph names.
        let mut p = post[..32].to_vec();
        p[0..4].copy_from_slice(&0x0003_0000u32.to_be_bytes());
        out.push((*b"post", p));
    }
    for (tag, data) in &list {
        if KEPT.contains(&tag) && !out.iter().any(|(t, _)| t == tag) && tag != b"head" {
            out.push((*tag, data.to_vec()));
        }
    }
    head[8..12].copy_from_slice(&[0; 4]);
    out.push((*b"head", head));
    out.sort_by_key(|(t, _)| *t);
    let version = u32_at(bytes, 0).unwrap_or(0x0001_0000);
    Ok(assemble(version, &out))
}

/// The glyph ids a composite glyph is made of.
fn components(g: &[u8]) -> Vec<u16> {
    let mut out = Vec::new();
    if u16_at(g, 0).is_none_or(|n| (n as i16) >= 0) {
        return out;
    }
    let mut at = 10;
    while let (Some(flags), Some(id)) = (u16_at(g, at), u16_at(g, at + 2)) {
        out.push(id);
        at += 4 + if flags & 0x0001 != 0 { 4 } else { 2 };
        at += if flags & 0x0008 != 0 {
            2
        } else if flags & 0x0040 != 0 {
            4
        } else if flags & 0x0080 != 0 {
            8
        } else {
            0
        };
        if flags & 0x0020 == 0 || out.len() > 1024 {
            break;
        }
    }
    out
}

/// A format 12 cmap (Windows, full Unicode) of `pairs`, sorted by character.
fn cmap12(pairs: &[(u32, u16)]) -> Vec<u8> {
    let mut groups: Vec<(u32, u32, u32)> = Vec::new();
    for &(c, g) in pairs {
        match groups.last_mut() {
            Some((start, end, gid)) if c == *end + 1 && u32::from(g) == *gid + (c - *start) => *end = c,
            _ => groups.push((c, c, u32::from(g))),
        }
    }
    let length = 16 + 12 * groups.len() as u32;
    let mut out = Vec::with_capacity(12 + length as usize);
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&3u16.to_be_bytes());
    out.extend_from_slice(&10u16.to_be_bytes());
    out.extend_from_slice(&12u32.to_be_bytes());
    out.extend_from_slice(&12u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&(groups.len() as u32).to_be_bytes());
    for (start, end, gid) in groups {
        for v in [start, end, gid] {
            out.extend_from_slice(&v.to_be_bytes());
        }
    }
    out
}

fn checksum(d: &[u8]) -> u32 {
    d.chunks(4).fold(0u32, |sum, c| {
        let mut b = [0u8; 4];
        b[..c.len()].copy_from_slice(c);
        sum.wrapping_add(u32::from_be_bytes(b))
    })
}

/// A font file of `tables` (sorted by tag), with checksums and head's adjustment.
fn assemble(version: u32, tables: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
    let n = tables.len() as u16;
    let mut power = 1u16;
    let mut log = 0u16;
    while power.saturating_mul(2) <= n {
        power *= 2;
        log += 1;
    }
    let mut out = Vec::new();
    out.extend_from_slice(&version.to_be_bytes());
    out.extend_from_slice(&n.to_be_bytes());
    out.extend_from_slice(&(power * 16).to_be_bytes());
    out.extend_from_slice(&log.to_be_bytes());
    out.extend_from_slice(&(n * 16 - power * 16).to_be_bytes());
    let mut offset = 12 + 16 * tables.len();
    let mut head_at = None;
    for (tag, data) in tables {
        out.extend_from_slice(tag);
        out.extend_from_slice(&checksum(data).to_be_bytes());
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        if tag == b"head" {
            head_at = Some(offset);
        }
        offset += data.len().div_ceil(4) * 4;
    }
    for (_, data) in tables {
        out.extend_from_slice(data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
    if let Some(at) = head_at {
        let adjust = 0xB1B0_AFBAu32.wrapping_sub(checksum(&out));
        out[at + 8..at + 12].copy_from_slice(&adjust.to_be_bytes());
    }
    out
}

/// A CFF INDEX at `at`: its item ranges (absolute) and where it ends.
fn cff_index(d: &[u8], at: usize) -> Result<(Vec<std::ops::Range<usize>>, usize), String> {
    let count = usize::from(u16_at(d, at).ok_or("a CFF INDEX is cut off")?);
    if count == 0 {
        return Ok((Vec::new(), at + 2));
    }
    let size = usize::from(*d.get(at + 2).ok_or("a CFF INDEX is cut off")?);
    if !(1..=4).contains(&size) {
        return Err("a CFF INDEX has a bad offset size".into());
    }
    let read = |i: usize| -> Result<usize, String> {
        let p = at + 3 + i * size;
        let b = d.get(p..p + size).ok_or("a CFF INDEX is cut off")?;
        Ok(b.iter().fold(0usize, |v, x| (v << 8) | usize::from(*x)))
    };
    let base = at + 3 + (count + 1) * size - 1;
    let mut items = Vec::with_capacity(count);
    let mut prev = read(0)?;
    for i in 1..=count {
        let next = read(i)?;
        if next < prev || base + next > d.len() {
            return Err("a CFF INDEX item lies outside the table".into());
        }
        items.push(base + prev..base + next);
        prev = next;
    }
    Ok((items, base + prev))
}

/// A CFF DICT's operators (an escaped one as 1200 + its second byte) with their integer operands.
fn dict_ops(dict: &[u8]) -> Vec<(u16, Vec<i64>)> {
    let mut out = Vec::new();
    let mut operands: Vec<i64> = Vec::new();
    let mut i = 0;
    while i < dict.len() {
        let b = dict[i];
        let at = |k: usize| dict.get(i + k).copied().map(i64::from);
        match b {
            32..=246 => {
                operands.push(i64::from(b) - 139);
                i += 1;
            }
            247..=250 => {
                let Some(n) = at(1) else { break };
                operands.push((i64::from(b) - 247) * 256 + n + 108);
                i += 2;
            }
            251..=254 => {
                let Some(n) = at(1) else { break };
                operands.push(-(i64::from(b) - 251) * 256 - n - 108);
                i += 2;
            }
            28 => {
                let Some(v) = u16_at(dict, i + 1) else { break };
                operands.push(i64::from(v as i16));
                i += 3;
            }
            29 => {
                let Some(v) = u32_at(dict, i + 1) else { break };
                operands.push(i64::from(v as i32));
                i += 5;
            }
            30 => {
                i += 1;
                while i < dict.len() && dict[i] & 0x0F != 0x0F && dict[i] >> 4 != 0x0F {
                    i += 1;
                }
                i += 1;
                operands.push(0);
            }
            12 => {
                let Some(n) = at(1) else { break };
                out.push((1200 + n as u16, std::mem::take(&mut operands)));
                i += 2;
            }
            _ => {
                out.push((u16::from(b), std::mem::take(&mut operands)));
                i += 1;
            }
        }
    }
    out
}

fn dict_get(ops: &[(u16, Vec<i64>)], op: u16) -> Option<&[i64]> {
    ops.iter().find(|(o, _)| *o == op).map(|(_, v)| v.as_slice())
}

fn offset(v: i64) -> Option<usize> {
    usize::try_from(v).ok()
}

/// A Private DICT's local subroutines: its `Subrs` INDEX, item ranges.
fn local_subrs(cff: &[u8], private: &[i64]) -> Result<Vec<std::ops::Range<usize>>, String> {
    let [size, at] = private else { return Ok(Vec::new()) };
    let (size, at) = (offset(*size).ok_or("bad Private DICT")?, offset(*at).ok_or("bad Private DICT")?);
    let dict = cff.get(at..at + size).ok_or("the Private DICT lies outside the table")?;
    match dict_get(&dict_ops(dict), 19).and_then(|v| v.first().copied()).and_then(offset) {
        Some(rel) => Ok(cff_index(cff, at + rel)?.0),
        None => Ok(Vec::new()),
    }
}

fn bias(count: usize) -> i64 {
    if count < 1240 {
        107
    } else if count < 33900 {
        1131
    } else {
        32768
    }
}

/// Walks Type 2 charstrings to find the subroutines they call.
struct SubrWalk<'a> {
    cff: &'a [u8],
    global: &'a [std::ops::Range<usize>],
    used_global: BTreeSet<usize>,
    used_local: BTreeSet<usize>,
    stems: usize,
    stack: usize,
    last: i64,
}

impl SubrWalk<'_> {
    /// `Err` when the charstring cannot be followed, so nothing is dropped.
    fn run(&mut self, code: &[u8], local: &[std::ops::Range<usize>], depth: usize) -> Result<bool, ()> {
        if depth > 10 {
            return Err(());
        }
        let mut i = 0;
        while i < code.len() {
            let b = code[i];
            match b {
                32..=246 => {
                    self.push(i64::from(b) - 139);
                    i += 1;
                }
                247..=254 => {
                    let n = i64::from(*code.get(i + 1).ok_or(())?);
                    self.push(if b < 251 { (i64::from(b) - 247) * 256 + n + 108 } else { -(i64::from(b) - 251) * 256 - n - 108 });
                    i += 2;
                }
                28 => {
                    self.push(i64::from(u16_at(code, i + 1).ok_or(())? as i16));
                    i += 3;
                }
                255 => {
                    self.push(i64::from((u32_at(code, i + 1).ok_or(())? as i32) >> 16));
                    i += 5;
                }
                1 | 3 | 18 | 23 => {
                    self.stems += self.stack / 2;
                    self.clear();
                    i += 1;
                }
                19 | 20 => {
                    // hintmask and cntrmask: operands before them are vstems
                    self.stems += self.stack / 2;
                    self.clear();
                    i += 1 + self.stems.div_ceil(8);
                }
                10 | 29 => {
                    let subrs = if b == 10 { local } else { self.global };
                    let n = usize::try_from(self.last + bias(subrs.len())).map_err(|_| ())?;
                    let range = subrs.get(n).ok_or(())?.clone();
                    self.stack = self.stack.saturating_sub(1);
                    if b == 10 {
                        self.used_local.insert(n)
                    } else {
                        self.used_global.insert(n)
                    };
                    let cff = self.cff;
                    if self.run(&cff[range], local, depth + 1)? {
                        return Ok(true);
                    }
                    i += 1;
                }
                11 => return Ok(false),
                14 => return Ok(true),
                12 => {
                    self.clear();
                    i += 2;
                }
                _ => {
                    self.clear();
                    i += 1;
                }
            }
        }
        Ok(false)
    }

    fn push(&mut self, v: i64) {
        self.stack += 1;
        self.last = v;
    }

    fn clear(&mut self) {
        self.stack = 0;
    }
}

/// The CharStrings INDEX at `at` written again where it is: the glyphs not kept become a lone
/// `endchar`, so their offsets step by one, and the bytes freed after it are zeroed. What follows
/// the INDEX is found by offset, so nothing else moves.
fn compact_charstrings(cff: &[u8], out: &mut [u8], at: usize, glyphs: &[std::ops::Range<usize>], keep: &BTreeSet<u16>) -> Result<(), String> {
    let size = usize::from(*cff.get(at + 2).ok_or("the CFF table is cut off")?);
    let base = at + 3 + (glyphs.len() + 1) * size - 1;
    let end = glyphs.last().map_or(base + 1, |r| r.end);
    let mut data = Vec::with_capacity(end - base);
    let mut offsets = Vec::with_capacity(glyphs.len() + 1);
    for (g, range) in glyphs.iter().enumerate() {
        offsets.push(data.len() + 1);
        if u16::try_from(g).is_ok_and(|g| keep.contains(&g)) {
            data.extend_from_slice(&cff[range.clone()]);
        } else {
            data.push(14);
        }
    }
    offsets.push(data.len() + 1);
    if base + 1 + data.len() > end {
        return Err("the CharStrings grew".into());
    }
    for (i, o) in offsets.into_iter().enumerate() {
        let p = at + 3 + i * size;
        let be = (o as u32).to_be_bytes();
        out[p..p + size].copy_from_slice(&be[4 - size..]);
    }
    out[base + 1..base + 1 + data.len()].copy_from_slice(&data);
    out[base + 1 + data.len()..end].fill(0);
    Ok(())
}

/// The CFF table with every glyph not kept drawn as nothing (its charstring becomes `endchar`
/// repeated) and the subroutines no kept glyph calls emptied, so offsets stay and the bytes
/// compress away.
fn blank_charstrings(cff: &[u8], keep: &BTreeSet<u16>) -> Result<Vec<u8>, String> {
    let header = usize::from(*cff.get(2).ok_or("the CFF table is cut off")?);
    let (_, after_names) = cff_index(cff, header)?;
    let (top, after_top) = cff_index(cff, after_names)?;
    let top = dict_ops(&cff[top.first().ok_or("the CFF table has no Top DICT")?.clone()]);
    let (_, after_strings) = cff_index(cff, after_top)?;
    let (global, _) = cff_index(cff, after_strings)?;
    let at = dict_get(&top, 17).and_then(|v| v.first().copied()).and_then(offset).ok_or("the CFF table has no CharStrings")?;
    let (glyphs, _) = cff_index(cff, at)?;
    // Local subroutines per glyph: one Private DICT, or one per Font DICT of a CID-keyed font.
    let mut locals: Vec<Vec<std::ops::Range<usize>>> = Vec::new();
    let mut fd_of: Vec<usize> = vec![0; glyphs.len()];
    if let (Some(fd_array), Some(fd_select)) = (dict_get(&top, 1236), dict_get(&top, 1237)) {
        let (fonts, _) = cff_index(cff, fd_array.first().copied().and_then(offset).ok_or("bad FDArray")?)?;
        for f in &fonts {
            locals.push(local_subrs(cff, dict_get(&dict_ops(&cff[f.clone()]), 18).unwrap_or(&[]))?);
        }
        let sel = fd_select.first().copied().and_then(offset).ok_or("bad FDSelect")?;
        match cff.get(sel) {
            Some(0) => {
                for (g, fd) in fd_of.iter_mut().enumerate() {
                    *fd = usize::from(*cff.get(sel + 1 + g).ok_or("FDSelect is cut off")?);
                }
            }
            Some(3) => {
                let n = usize::from(u16_at(cff, sel + 1).ok_or("FDSelect is cut off")?);
                for r in 0..n {
                    let at = sel + 3 + r * 3;
                    let first = usize::from(u16_at(cff, at).ok_or("FDSelect is cut off")?);
                    let fd = usize::from(*cff.get(at + 2).ok_or("FDSelect is cut off")?);
                    let end = usize::from(u16_at(cff, at + 3).ok_or("FDSelect is cut off")?);
                    for g in first..end.min(fd_of.len()) {
                        fd_of[g] = fd;
                    }
                }
            }
            _ => return Err("unknown FDSelect format".into()),
        }
    } else {
        locals.push(local_subrs(cff, dict_get(&top, 18).unwrap_or(&[]))?);
    }
    let mut out = cff.to_vec();
    let mut walk = SubrWalk { cff, global: &global, used_global: BTreeSet::new(), used_local: BTreeSet::new(), stems: 0, stack: 0, last: 0 };
    let mut used_local: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); locals.len()];
    let mut followed = true;
    for (g, range) in glyphs.iter().enumerate() {
        if u16::try_from(g).is_ok_and(|g| !keep.contains(&g)) {
            continue;
        }
        let fd = fd_of.get(g).copied().unwrap_or(0);
        let Some(local) = locals.get(fd) else {
            followed = false;
            continue;
        };
        walk.stems = 0;
        walk.stack = 0;
        walk.used_local.clear();
        if walk.run(&cff[range.clone()], local, 0).is_err() {
            followed = false;
        }
        used_local[fd].extend(walk.used_local.iter().copied());
    }
    compact_charstrings(cff, &mut out, at, &glyphs, keep)?;
    // Every kept glyph followed to its end: the subroutines none of them called are emptied.
    if followed {
        for (n, range) in global.iter().enumerate() {
            if !walk.used_global.contains(&n) {
                out[range.clone()].fill(11);
            }
        }
        for (fd, list) in locals.iter().enumerate() {
            for (n, range) in list.iter().enumerate() {
                if !used_local[fd].contains(&n) {
                    out[range.clone()].fill(11);
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use skrifa::MetadataProvider as _;

    #[test]
    fn a_subset_keeps_glyph_ids_and_only_the_kept_outlines() {
        let bytes = include_bytes!("../../../assets/fonts/Inter-Regular.ttf");
        let font = skrifa::FontRef::new(bytes).unwrap();
        let gid = |c| font.charmap().map(c).unwrap().to_u32() as u16;
        let keep: BTreeSet<u16> = "Hello".chars().map(gid).collect();
        let small = subset(bytes, &keep).unwrap();
        assert!(small.len() * 5 < bytes.len(), "{} of {} bytes", small.len(), bytes.len());
        let sub = skrifa::FontRef::new(&small).unwrap();
        for c in "Hello".chars() {
            assert_eq!(sub.charmap().map(c).map(|g| g.to_u32() as u16), Some(gid(c)), "{c} keeps its glyph id");
        }
        assert_eq!(sub.charmap().map('Z'), None, "a dropped glyph has no character");
        let outlines = sub.outline_glyphs();
        let mut pen = skrifa::outline::pen::ControlBoundsPen::default();
        let settings = skrifa::outline::DrawSettings::unhinted(skrifa::instance::Size::unscaled(), skrifa::instance::LocationRef::default());
        outlines.get(skrifa::GlyphId::new(u32::from(gid('H')))).unwrap().draw(settings, &mut pen).unwrap();
        assert!(pen.bounding_box().is_some(), "H still has its outline");
        let advances = sub.glyph_metrics(skrifa::instance::Size::unscaled(), skrifa::instance::LocationRef::default());
        let full = font.glyph_metrics(skrifa::instance::Size::unscaled(), skrifa::instance::LocationRef::default());
        let h = skrifa::GlyphId::new(u32::from(gid('H')));
        assert_eq!(advances.advance_width(h), full.advance_width(h));
        assert!(crate::embedding::validate(&small).is_ok(), "the subset embeds like a font");
        assert_eq!(checksum(&small), 0xB1B0_AFBA, "the whole-font checksum");
    }

    #[test]
    fn components_of_a_composite_glyph() {
        // numberOfContours -1, bbox, then two components (words for arguments on the first).
        let mut g = vec![0xFF, 0xFF, 0, 0, 0, 0, 0, 0, 0, 0];
        g.extend_from_slice(&[0x00, 0x21, 0x00, 0x05, 0, 1, 0, 2]);
        g.extend_from_slice(&[0x00, 0x00, 0x00, 0x07, 1, 2]);
        assert_eq!(components(&g), [5, 7]);
        assert!(components(&[0, 1, 0, 0]).is_empty(), "a simple glyph");
    }

    #[test]
    fn a_cmap12_groups_runs() {
        let c = cmap12(&[(65, 10), (66, 11), (67, 12), (90, 3)]);
        assert_eq!(u32_at(&c, 12 + 12), Some(2), "two groups");
    }
}
