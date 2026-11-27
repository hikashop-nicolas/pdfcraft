//! Subsetting on save: each full font program embedded for edited or added text (`/PCFullFont`)
//! keeps only the glyphs the document shows with it. Run on the copy being written, so the open
//! document keeps the whole face for later edits; each save subsets again from it.

use std::collections::{BTreeSet, HashMap, HashSet};

use pdfcraft_cos::{Dict, Document, ObjRef, Object, Stream};
use pdfcraft_fonts::pdf::Metrics;

/// Forms and appearances nested deeper than this are not followed.
const MAX_DEPTH: usize = 12;
/// The largest program read for subsetting.
const MAX_PROGRAM: usize = 32 << 20;

/// What the walk learned about one program.
#[derive(Default)]
struct Usage {
    glyphs: BTreeSet<u16>,
    /// A stream drawing with it could not be read, so it stays whole.
    whole: bool,
}

/// Where a font's codes lead: its program and how a CID becomes a glyph.
struct FontUse {
    program: ObjRef,
    /// The font dictionary's own object, when it has one: its maps can then be trimmed.
    font: Option<ObjRef>,
    metrics: Metrics,
    /// TrueType: the CIDToGIDMap (empty means identity); CFF: CIDs are glyph ids.
    cid_to_gid: Vec<u16>,
}

fn descriptor_program(doc: &Document, font: &Dict) -> Option<(ObjRef, Dict, Option<Dict>)> {
    if font.name(b"Subtype") != Some(b"Type0") {
        return None;
    }
    let cid = doc.dict(doc.resolve(font.get(b"DescendantFonts")?).as_array()?.first()?)?;
    let descriptor = doc.dict(cid.get(b"FontDescriptor")?)?;
    let program = descriptor.get(b"FontFile2").or_else(|| descriptor.get(b"FontFile3"))?.as_ref()?;
    Some((program, cid, Some(descriptor)))
}

fn read_cid_to_gid(doc: &Document, cid: &Dict) -> Vec<u16> {
    match cid.get(b"CIDToGIDMap").map(|o| doc.resolve(o)) {
        Some(o) => match o.as_ref() {
            Object::Stream(s) => {
                s.decoded_within(1 << 18).map(|d| d.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect()).unwrap_or_default()
            }
            _ => Vec::new(),
        },
        None => Vec::new(),
    }
}

struct Walk<'a> {
    doc: &'a Document,
    programs: &'a HashSet<ObjRef>,
    usage: HashMap<ObjRef, Usage>,
    /// Per font object, the codes shown with it: byte length and CID.
    shown: HashMap<ObjRef, std::collections::BTreeMap<u32, (usize, u32)>>,
    /// Font dicts read, by reference (a direct one is read again each time).
    fonts: HashMap<ObjRef, Option<std::rc::Rc<FontUse>>>,
    seen: HashSet<(ObjRef, usize)>,
}

impl Walk<'_> {
    fn font_use(&mut self, font: &Object) -> Option<std::rc::Rc<FontUse>> {
        let read = |doc: &Document, programs: &HashSet<ObjRef>| -> Option<std::rc::Rc<FontUse>> {
            let dict = doc.dict(font)?;
            let (program, cid, _) = descriptor_program(doc, &dict)?;
            if !programs.contains(&program) {
                return None;
            }
            let ttf = cid.name(b"Subtype") == Some(b"CIDFontType2");
            Some(std::rc::Rc::new(FontUse {
                program,
                font: font.as_ref(),
                metrics: Metrics::from_dict(doc, &dict),
                cid_to_gid: if ttf { read_cid_to_gid(doc, &cid) } else { Vec::new() },
            }))
        };
        match font.as_ref() {
            Some(r) => {
                if let Some(known) = self.fonts.get(&r) {
                    return known.clone();
                }
                let got = read(self.doc, self.programs);
                self.fonts.insert(r, got.clone());
                got
            }
            None => read(self.doc, self.programs),
        }
    }

    /// Every one of our fonts a resource dictionary names, marked to stay whole.
    fn keep_whole(&mut self, resources: &Dict) {
        let fonts = resources.get(b"Font").and_then(|f| self.doc.dict(f)).unwrap_or_default();
        for (_, f) in fonts.iter() {
            if let Some(u) = self.font_use(f) {
                self.usage.entry(u.program).or_default().whole = true;
            }
        }
    }

    fn stream(&mut self, stream: &Stream, resources: &Dict, depth: usize) {
        let Ok(data) = stream.decoded_within(pdfcraft_cos::MAX_PAGE_DECODED) else {
            self.keep_whole(resources);
            return;
        };
        self.content(&data, resources, depth);
    }

    fn content(&mut self, data: &[u8], resources: &Dict, depth: usize) {
        let fonts = resources.get(b"Font").and_then(|f| self.doc.dict(f)).unwrap_or_default();
        let xobjects = resources.get(b"XObject").and_then(|f| self.doc.dict(f)).unwrap_or_default();
        let mut current: Option<std::rc::Rc<FontUse>> = None;
        let mut stack: Vec<Option<std::rc::Rc<FontUse>>> = Vec::new();
        for op in pdfcraft_content::ops(data) {
            match op.op.as_slice() {
                b"q" => stack.push(current.clone()),
                b"Q" => current = stack.pop().unwrap_or(None),
                b"Tf" => current = op.name_back(1).and_then(|n| fonts.get(n).cloned()).and_then(|f| self.font_use(&f)),
                b"Tj" | b"'" | b"\"" | b"TJ" => {
                    let Some(font) = &current else { continue };
                    let mut strings = Vec::new();
                    match op.operands.last() {
                        Some(Object::Array(items)) => strings.extend(items.iter().filter_map(Object::as_string)),
                        Some(other) => strings.extend(other.as_string()),
                        None => {}
                    }
                    let usage = self.usage.entry(font.program).or_default();
                    let shown = font.font.map(|r| self.shown.entry(r).or_default());
                    let mut shown = shown;
                    for s in strings {
                        for (code, len, cid) in font.metrics.code_cids(&s.bytes) {
                            if let Some(shown) = shown.as_mut() {
                                shown.insert(code, (len, cid));
                            }
                            let gid = if font.cid_to_gid.is_empty() {
                                u16::try_from(cid).ok()
                            } else {
                                usize::try_from(cid).ok().and_then(|c| font.cid_to_gid.get(c).copied())
                            };
                            usage.glyphs.extend(gid);
                        }
                    }
                }
                b"Do" if depth < MAX_DEPTH => {
                    let Some(Object::Ref(r)) = op.last_name().and_then(|n| xobjects.get(n)).cloned() else { continue };
                    self.form(r, resources, depth + 1);
                }
                _ => {}
            }
        }
    }

    fn form(&mut self, r: ObjRef, parent: &Dict, depth: usize) {
        if !self.seen.insert((r, depth.min(1))) {
            return;
        }
        let object = self.doc.get(r);
        let Object::Stream(s) = object.as_ref() else { return };
        if s.dict.name(b"Subtype") != Some(b"Form") {
            return;
        }
        let resources = s.dict.get(b"Resources").and_then(|o| self.doc.dict(o)).unwrap_or_else(|| parent.clone());
        self.stream(s, &resources, depth);
    }

    fn appearance(&mut self, o: &Object, depth: usize) {
        match self.doc.resolve(o).as_ref() {
            Object::Stream(_) => {
                if let Some(r) = o.as_ref() {
                    self.form(r, &Dict::new(), depth);
                }
            }
            Object::Dict(states) => {
                for (_, v) in states.iter() {
                    if let Some(r) = v.as_ref() {
                        self.form(r, &Dict::new(), depth);
                    }
                }
            }
            _ => {}
        }
    }
}

/// A ToUnicode CMap of the codes shown.
fn to_unicode(m: &Metrics, codes: &std::collections::BTreeMap<u32, (usize, u32)>) -> Vec<u8> {
    let mut entries = Vec::new();
    for (code, (len, _)) in codes {
        let Some(text) = m.text_of(*code) else { continue };
        let hex: String = text.encode_utf16().map(|u| format!("{u:04X}")).collect();
        let width = (*len).clamp(1, 4) * 2;
        entries.push(format!("<{code:0width$X}> <{hex}>"));
    }
    let mut out = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
         /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
    );
    for chunk in entries.chunks(100) {
        out.push_str(&format!("{} beginbfchar\n{}\nendbfchar\n", chunk.len(), chunk.join("\n")));
    }
    out.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    out.into_bytes()
}

/// The CID font's widths for the CIDs shown only, and its CIDToGIDMap without the others.
fn trim_cid_font(doc: &Document, out: &mut Document, cid: &mut Dict, m: &Metrics, codes: &std::collections::BTreeMap<u32, (usize, u32)>) {
    let mut widths: std::collections::BTreeMap<u32, f64> = std::collections::BTreeMap::new();
    for (code, (_, c)) in codes {
        widths.insert(*c, (m.width(*code) * 1000.0 * 1000.0).round() / 1000.0);
    }
    let w: Vec<Object> = widths.iter().flat_map(|(c, w)| [Object::Int(i64::from(*c)), Object::Array(vec![Object::Real(*w)])]).collect();
    cid.set(b"W".to_vec(), Object::Array(w));
    if let Some(Object::Ref(map_ref)) = cid.get(b"CIDToGIDMap").cloned()
        && let Object::Stream(st) = doc.get(map_ref).as_ref()
        && let Ok(mut data) = st.decoded_within(1 << 18)
    {
        for (i, pair) in data.chunks_exact_mut(2).enumerate() {
            if !widths.contains_key(&(i as u32)) {
                pair.fill(0);
            }
        }
        let mut d = st.dict.clone();
        d.remove(b"Filter");
        d.remove(b"DecodeParms");
        out.set(map_ref, Object::Stream(Stream::flate(d, &data)));
    }
}

/// Six capital letters naming a subset (ISO 32000-2 §9.9.2), from what it keeps.
fn tag(program: ObjRef, glyphs: &BTreeSet<u16>) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    program.num.hash(&mut h);
    glyphs.hash(&mut h);
    let mut v = h.finish();
    (0..6)
        .map(|_| {
            let c = char::from(b'A' + (v % 26) as u8);
            v /= 26;
            c
        })
        .collect()
}

fn tagged(name: &[u8], tag: &str) -> Option<Object> {
    let s = String::from_utf8_lossy(name);
    let bare = s.rsplit('+').next().unwrap_or(&s);
    Some(Object::name(format!("{tag}+{bare}").as_str()))
}

/// A copy of `doc` with its full embedded programs subset, or `None` when it has none.
pub fn subset_for_save(doc: &Document) -> Option<Document> {
    let programs: HashSet<ObjRef> =
        doc.scan_objects().filter(|(_, o)| matches!(o.as_ref(), Object::Stream(s) if s.dict.get(b"PCFullFont").is_some())).map(|(r, _)| r).collect();
    if programs.is_empty() {
        return None;
    }
    let mut walk = Walk { doc, programs: &programs, usage: HashMap::new(), shown: HashMap::new(), fonts: HashMap::new(), seen: HashSet::new() };
    for page in pdfcraft_model::pages(doc) {
        let resources = page.dict.get(b"Resources").and_then(|o| doc.dict(o)).unwrap_or_default();
        let contents = page.dict.get(b"Contents").map(|c| doc.resolve(c));
        let parts: Vec<Object> = match contents.as_deref() {
            Some(Object::Array(a)) => a.clone(),
            Some(_) => page.dict.get(b"Contents").cloned().into_iter().collect(),
            None => Vec::new(),
        };
        // The parts of a page's contents join into one stream (ISO 32000-2 §7.8.2).
        let mut data = Vec::new();
        let mut readable = true;
        for part in parts {
            if let Object::Stream(s) = doc.resolve(&part).as_ref() {
                match s.decoded_within(pdfcraft_cos::MAX_PAGE_DECODED) {
                    Ok(d) => {
                        data.extend_from_slice(&d);
                        data.push(b'\n');
                    }
                    Err(_) => readable = false,
                }
            }
        }
        if readable {
            walk.content(&data, &resources, 0);
        } else {
            walk.keep_whole(&resources);
        }
        let annots = page.dict.get(b"Annots").map(|a| doc.resolve(a));
        for a in annots.as_deref().and_then(Object::as_array).into_iter().flatten() {
            let Some(annot) = doc.dict(a) else { continue };
            let Some(ap) = annot.get(b"AP").and_then(|o| doc.dict(o)) else { continue };
            for key in [b"N".as_slice(), b"R", b"D"] {
                if let Some(o) = ap.get(key) {
                    walk.appearance(o, 1);
                }
            }
        }
    }
    let usage = walk.usage;
    let shown = walk.shown;
    let mut out = doc.clone();
    let mut renamed: HashMap<ObjRef, String> = HashMap::new();
    for program in &programs {
        let object = doc.get(*program);
        let Object::Stream(s) = object.as_ref() else { continue };
        let u = usage.get(program);
        if u.is_some_and(|u| u.whole) {
            continue;
        }
        let Ok(bytes) = s.decoded_within(MAX_PROGRAM) else { continue };
        if pdfcraft_fonts::subset::fs_type(&bytes) & pdfcraft_fonts::subset::NO_SUBSETTING != 0 {
            continue;
        }
        let glyphs = u.map(|u| u.glyphs.clone()).unwrap_or_default();
        let Ok(small) = pdfcraft_fonts::subset::subset(&bytes, &glyphs) else { continue };
        let mut dict = s.dict.clone();
        dict.remove(b"PCFullFont");
        dict.remove(b"Filter");
        dict.remove(b"DecodeParms");
        dict.set(b"PCSubset".to_vec(), Object::Bool(true));
        if dict.get(b"Length1").is_some() {
            dict.set(b"Length1".to_vec(), Object::Int(small.len() as i64));
        }
        out.set(*program, Object::Stream(Stream::flate(dict, &small)));
        renamed.insert(*program, tag(*program, &glyphs));
    }
    // Subset fonts carry a tag in their names.
    for (r, o) in doc.scan_objects() {
        let Object::Dict(font) = o.as_ref() else { continue };
        let Some((program, cid, descriptor)) = descriptor_program(doc, font) else { continue };
        let Some(tag) = renamed.get(&program) else { continue };
        let mut top = font.clone();
        if let Some(n) = font.name(b"BaseFont").and_then(|n| tagged(n, tag)) {
            top.set(b"BaseFont".to_vec(), n);
        }
        // A font made for a whole face maps every character: its maps keep what is shown.
        let codes = shown.get(&r);
        let metrics = codes.map(|_| Metrics::from_dict(doc, font));
        if let (Some(codes), Some(m)) = (codes, &metrics) {
            top.set(b"ToUnicode".to_vec(), Object::Ref(out.add(Object::Stream(Stream::flate(Dict::new(), &to_unicode(m, codes))))));
        }
        out.set(r, Object::Dict(top));
        if let Some(Object::Ref(cid_ref)) =
            doc.resolve(font.get(b"DescendantFonts").unwrap_or(&Object::Null)).as_array().and_then(|a| a.first()).cloned()
        {
            let mut c = cid.clone();
            if let Some(n) = cid.name(b"BaseFont").and_then(|n| tagged(n, tag)) {
                c.set(b"BaseFont".to_vec(), n);
            }
            if let (Some(codes), Some(m)) = (codes, &metrics) {
                trim_cid_font(doc, &mut out, &mut c, m, codes);
            }
            out.set(cid_ref, Object::Dict(c));
            if let (Some(Object::Ref(d_ref)), Some(mut d)) = (cid.get(b"FontDescriptor").cloned(), descriptor) {
                if let Some(n) = d.name(b"FontName").and_then(|n| tagged(n, tag)) {
                    d.set(b"FontName".to_vec(), n);
                }
                out.set(d_ref, Object::Dict(d));
            }
        }
    }
    Some(out)
}
