//! What is done to a whole document rather than to one page's content: redaction, hidden
//! information, watermarks, headers and footers, backgrounds, bookmarks and attached files.
//! Boxes are displayed points from the page's top-left, like the rest of the facade.

use std::sync::Arc;

use pdfcraft_engine::{Background, Edit, HeaderFooter, Hidden, MarkFile, MarkKind, NewAnnotation, OverlayLook, Shape, Style, Watermark};
use pdfcraft_render::AttachmentSource;
use serde_json::{Value, json};

use crate::core::Document;

const MAX_BOXES: usize = 2000;

const HIDDEN_IDS: [(&str, Hidden); 9] = [
    ("metadata", Hidden::Metadata),
    ("attachments", Hidden::Attachments),
    ("comments", Hidden::Comments),
    ("formFields", Hidden::FormFields),
    ("hiddenText", Hidden::HiddenText),
    ("hiddenLayers", Hidden::HiddenLayers),
    ("bookmarks", Hidden::Bookmarks),
    ("links", Hidden::LinksActionsScripts),
    ("privateData", Hidden::PrivateData),
];

fn colour(v: &Value, key: &str, default: [f64; 3]) -> Result<[f64; 3], String> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(c) => {
            let a = c.as_array().filter(|a| a.len() == 3).ok_or_else(|| format!("{key} is [r, g, b]"))?;
            let mut out = [0.0; 3];
            for (slot, n) in out.iter_mut().zip(a) {
                *slot = n.as_f64().filter(|n| (0.0..=1.0).contains(n)).ok_or_else(|| format!("{key} is [r, g, b], each from 0 to 1"))?;
            }
            Ok(out)
        }
    }
}

fn number(v: &Value, key: &str, default: f64, range: std::ops::RangeInclusive<f64>) -> Result<f64, String> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(n) => n.as_f64().filter(|n| range.contains(n)).ok_or_else(|| format!("{key} is a number from {} to {}", range.start(), range.end())),
    }
}

fn path(v: &Value) -> Result<Vec<usize>, String> {
    v.as_array()
        .ok_or("a bookmark path is a list of numbers")?
        .iter()
        .map(|n| n.as_u64().map(|n| n as usize).ok_or("a bookmark path is a list of numbers".to_string()))
        .collect()
}

impl Document {
    /// The pages a spec names (`"pages": [0, 2]`, 0-based) or every page.
    fn pages_of(&self, v: &Value) -> Result<Vec<usize>, String> {
        let count = self.page_count();
        match v.get("pages") {
            None | Some(Value::Null) => Ok((0..count).collect()),
            Some(list) => {
                let pages: Vec<usize> =
                    list.as_array().ok_or("pages is a list")?.iter().filter_map(|n| n.as_u64().map(|n| n as usize)).filter(|p| *p < count).collect();
                if pages.is_empty() { Err("no such pages".into()) } else { Ok(pages) }
            }
        }
    }

    // ------------------------------------------------------------------ redaction

    /// Marks areas of a page for redaction: `boxes` are displayed boxes, `overlay` the text shown on them once applied.
    pub fn add_redaction(&mut self, page: usize, boxes: &[[f64; 4]], overlay: &str) -> Result<(), String> {
        if boxes.is_empty() || boxes.len() > MAX_BOXES {
            return Err("redact between 1 and 2000 areas".into());
        }
        let info = self.page_info_ref(page)?;
        let mut quads = Vec::with_capacity(boxes.len());
        for b in boxes {
            if !b.iter().all(|n| n.is_finite()) {
                return Err("a box is finite numbers".into());
            }
            quads.push(info.view_rect_to_quad([b[0] as f32, b[1] as f32, b[2] as f32, b[3] as f32]));
        }
        let shape = Shape::Redact { quads, overlay: overlay.to_string(), look: OverlayLook::default() };
        let style = Style::default_for(&shape);
        self.apply(Edit::AddAnnotation(NewAnnotation { page, shape, style, contents: String::new(), author: String::new() }))
    }

    /// How many redaction marks wait to be applied.
    pub fn redaction_count(&self) -> Result<usize, String> {
        Ok(self.doc()?.redaction_marks())
    }

    /// Removes for good what the marks cover; the engine checks that nothing under them is still drawn.
    pub fn apply_redactions(&mut self) -> Result<(), String> {
        self.apply(Edit::ApplyRedactions { pages: None })
    }

    pub fn clear_redactions(&mut self) -> Result<(), String> {
        self.apply(Edit::ClearRedactions)
    }

    /// What each kind of hidden information would remove, as JSON: `[{"id", "count"}]`.
    pub fn hidden_info_json(&self) -> Result<String, String> {
        let found = self.doc()?.hidden_info();
        let list: Vec<Value> =
            HIDDEN_IDS.iter().map(|(id, h)| json!({"id": id, "count": found.iter().find(|(k, _)| k == h).map_or(0, |(_, n)| *n)})).collect();
        serde_json::to_string(&list).map_err(|e| e.to_string())
    }

    pub fn remove_hidden(&mut self, ids: &[String]) -> Result<(), String> {
        let which: Vec<Hidden> = ids.iter().filter_map(|i| HIDDEN_IDS.iter().find(|(id, _)| id == i).map(|(_, h)| *h)).collect();
        if which.is_empty() {
            return Err("nothing chosen to remove".into());
        }
        self.apply(Edit::RemoveHidden { which })
    }

    // ------------------------------------------------------------------ marks on every page

    /// A watermark: `{"text", "size"?, "color"?, "opacity"?, "rotation"?, "behind"?, "scale"?, "pages"?}`, or a picture from `picture`.
    /// It replaces the document's earlier watermark.
    pub fn add_watermark(&mut self, spec: &str, picture: Option<(String, Vec<u8>)>) -> Result<(), String> {
        let v: Value = serde_json::from_str(spec).map_err(|e| e.to_string())?;
        let pages = self.pages_of(&v)?;
        let settings = Watermark {
            text: v["text"].as_str().unwrap_or_default().to_string(),
            source: None,
            scale: number(&v, "scale", 1.0, 0.05..=2.0)?,
            font_size: number(&v, "size", 0.0, 0.0..=400.0)?,
            color: colour(&v, "color", [0.6, 0.6, 0.6])?,
            opacity: number(&v, "opacity", 0.5, 0.0..=1.0)?,
            rotation: number(&v, "rotation", 45.0, -360.0..=360.0)?,
            behind: v["behind"].as_bool().unwrap_or(false),
            offset: [0.0, 0.0],
        };
        if picture.is_none() && settings.text.trim().is_empty() {
            return Err("a watermark needs some text or a picture".into());
        }
        if picture.is_none()
            && let Some(c) = settings.first_undrawable()
        {
            return Err(format!("the watermark font cannot draw {c:?}"));
        }
        let file = picture.map(|(name, bytes)| MarkFile { name, bytes: Arc::new(bytes), page: 0 });
        self.apply(Edit::AddWatermark { pages, settings, replace: true, file })
    }

    /// A header and footer: `{"text": [6 strings: left, centre, right of the header, then of the footer], "size"?, "color"?,
    /// "font"?, "margins"?, "start"?, "shrink"?, "pages"?}`; `<<1>>` is the page number and `<<n>>` the page count.
    pub fn add_header_footer(&mut self, spec: &str) -> Result<(), String> {
        let v: Value = serde_json::from_str(spec).map_err(|e| e.to_string())?;
        let pages = self.pages_of(&v)?;
        let mut settings = HeaderFooter::default();
        let text = v["text"].as_array().filter(|a| a.len() == 6).ok_or("text is six strings")?;
        for (slot, t) in settings.text.iter_mut().zip(text) {
            *slot = t.as_str().unwrap_or_default().to_string();
        }
        if settings.text.iter().all(|t| t.trim().is_empty()) {
            return Err("a header or footer needs some text".into());
        }
        settings.font_size = number(&v, "size", 8.0, 4.0..=72.0)?;
        settings.color = colour(&v, "color", [0.0; 3])?;
        settings.start_number = number(&v, "start", 1.0, 0.0..=100_000.0)? as u32;
        settings.shrink = v["shrink"].as_bool().unwrap_or(false);
        if let Some(m) = v["margins"].as_array().filter(|m| m.len() == 4) {
            for (slot, n) in settings.margins.iter_mut().zip(m) {
                *slot = n.as_f64().filter(|n| (0.0..=400.0).contains(n)).ok_or("margins are from 0 to 400 points")?;
            }
        }
        if let Some(c) = settings.first_undrawable() {
            return Err(format!("the font cannot draw {c:?}"));
        }
        self.apply(Edit::AddHeaderFooter { pages, settings, replace: true })
    }

    /// A background behind the content: `{"color"?, "opacity"?, "scale"?, "pages"?}`, or a picture from `picture`.
    pub fn add_background(&mut self, spec: &str, picture: Option<(String, Vec<u8>)>) -> Result<(), String> {
        let v: Value = serde_json::from_str(spec).map_err(|e| e.to_string())?;
        let pages = self.pages_of(&v)?;
        let settings = Background {
            color: colour(&v, "color", [1.0, 1.0, 0.85])?,
            opacity: number(&v, "opacity", 1.0, 0.0..=1.0)?,
            source: None,
            scale: number(&v, "scale", 1.0, 0.05..=2.0)?,
        };
        let file = picture.map(|(name, bytes)| MarkFile { name, bytes: Arc::new(bytes), page: 0 });
        self.apply(Edit::AddBackground { pages, settings, replace: true, file })
    }

    /// Removes every mark of a kind: `headerFooter`, `watermark` or `background`.
    pub fn remove_marks(&mut self, kind: &str) -> Result<(), String> {
        let kind = match kind {
            "headerFooter" => MarkKind::HeaderFooter,
            "watermark" => MarkKind::Watermark,
            "background" => MarkKind::Background,
            other => return Err(format!("unknown mark {other:?}")),
        };
        self.apply(Edit::RemoveMarks { kind })
    }

    // ------------------------------------------------------------------ bookmarks

    /// The bookmarks in outline order as JSON: `path` (child indices), `title`, `page` (0-based or null), `open`, `bold`, `italic`.
    pub fn bookmarks_json(&self) -> Result<String, String> {
        let mut rows = Vec::new();
        let mut offset = 0;
        while let Some(page) = self.session.bookmark_page(self.id, offset, 500) {
            for b in &page.rows {
                rows.push(json!({"path": b.path, "title": b.title, "page": b.page, "open": b.open, "bold": b.bold, "italic": b.italic}));
            }
            match page.next {
                Some(n) if rows.len() < 5000 => offset = n,
                _ => break,
            }
        }
        serde_json::to_string(&rows).map_err(|e| e.to_string())
    }

    /// Adds a bookmark to a page as child `index` of the bookmark at `parent` (`[]`: the top level).
    pub fn add_bookmark(&mut self, parent: &str, index: usize, title: &str, page: usize) -> Result<(), String> {
        let parent = path(&serde_json::from_str(parent).map_err(|e| e.to_string())?)?;
        if page >= self.page_count() {
            return Err("no such page".into());
        }
        self.apply(Edit::AddBookmark { parent, index, title: title.to_string(), page, left: None, top: None })
    }

    pub fn rename_bookmark(&mut self, path_json: &str, title: &str) -> Result<(), String> {
        let path = path(&serde_json::from_str(path_json).map_err(|e| e.to_string())?)?;
        self.apply(Edit::RenameBookmark { path, title: title.to_string() })
    }

    pub fn delete_bookmark(&mut self, path_json: &str) -> Result<(), String> {
        let path = path(&serde_json::from_str(path_json).map_err(|e| e.to_string())?)?;
        self.apply(Edit::DeleteBookmark { path })
    }

    /// Moves a bookmark to child `index` of `to_parent` (the indices count after it has been taken out).
    pub fn move_bookmark(&mut self, from: &str, to_parent: &str, index: usize) -> Result<(), String> {
        let from = path(&serde_json::from_str(from).map_err(|e| e.to_string())?)?;
        let to_parent = path(&serde_json::from_str(to_parent).map_err(|e| e.to_string())?)?;
        self.apply(Edit::MoveBookmark { from, to_parent, index })
    }

    pub fn set_bookmark_page(&mut self, path_json: &str, page: usize) -> Result<(), String> {
        let path = path(&serde_json::from_str(path_json).map_err(|e| e.to_string())?)?;
        if page >= self.page_count() {
            return Err("no such page".into());
        }
        self.apply(Edit::SetBookmarkPage { path, page })
    }

    // ------------------------------------------------------------------ attached files

    /// The attached files as JSON: `index`, `name`, `description`, `size`, `where` (`document` or `page`), `key` (a document-level one's).
    pub fn attachments_json(&self) -> Result<String, String> {
        let list: Vec<Value> = self
            .doc()?
            .info
            .attachments
            .iter()
            .enumerate()
            .map(|(index, a)| match &a.source {
                AttachmentSource::Document { key } => {
                    json!({"index": index, "name": a.name, "description": a.description, "size": a.size, "where": "document", "key": key})
                }
                AttachmentSource::Annotation { page, .. } => {
                    json!({"index": index, "name": a.name, "description": a.description, "size": a.size, "where": "page", "page": page})
                }
            })
            .collect();
        serde_json::to_string(&list).map_err(|e| e.to_string())
    }

    pub fn add_attachment(&mut self, name: &str, bytes: &[u8], description: Option<String>) -> Result<(), String> {
        if bytes.is_empty() {
            return Err("the file is empty".into());
        }
        let mime = pdfcraft_engine::mime_for_name(name).map(str::to_string);
        self.apply(Edit::AddAttachment { name: name.to_string(), bytes: bytes.to_vec(), description: description.filter(|d| !d.is_empty()), mime })
    }

    /// Removes a document-level attachment by its `key`.
    pub fn delete_attachment(&mut self, key: &str) -> Result<(), String> {
        self.apply(Edit::DeleteAttachment { key: key.to_string() })
    }

    /// The bytes of attachment `index` (as `attachments_json` numbers them), as the document is now.
    pub fn attachment_bytes(&self, index: usize) -> Result<Vec<u8>, String> {
        let doc = self.doc()?;
        let att = doc.info.attachments.get(index).ok_or("no such attachment")?;
        let saved = self.save()?;
        pdfcraft_render::attachment_data(&saved, doc.password.as_deref(), att)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use crate::core::Document;
    use crate::core::tests::fixture;

    fn open(pages: usize) -> Document {
        Document::open(&fixture(pages), None, || 1_700_000_000).expect("opens")
    }

    /// The pages where `query` is found.
    fn hits(doc: &mut Document, query: &str) -> Vec<usize> {
        let found: Vec<Value> = serde_json::from_str(&doc.find_json(query, true, false, 50).expect("finds")).expect("json");
        found.iter().filter_map(|f| f["page"].as_u64().map(|p| p as usize)).collect()
    }

    #[test]
    fn redacts_text_for_good() {
        let mut doc = open(1);
        assert_eq!(hits(&mut doc, "Page"), [0], "the page shows its text first");
        let found: Vec<Value> = serde_json::from_str(&doc.find_json("Page", false, false, 5).expect("finds")).expect("json");
        let r: Vec<f64> = found[0]["rects"][0].as_array().expect("rect").iter().map(|v| v.as_f64().expect("n")).collect();
        doc.add_redaction(0, &[[r[0], r[1], r[2], r[3]]], "").expect("marks it");
        assert_eq!(doc.redaction_count().expect("counts"), 1);
        doc.apply_redactions().expect("applies");
        assert_eq!(doc.redaction_count().expect("counts"), 0);
        assert!(hits(&mut doc, "Page").is_empty(), "what was under the mark is gone");
        let mut saved = Document::open(&doc.save_full().expect("saves"), None, || 0).expect("reopens");
        assert!(hits(&mut saved, "Page").is_empty(), "and it stays gone in the saved file");
        doc.undo().expect("undoes");
        assert_eq!(hits(&mut doc, "Page"), [0]);
    }

    #[test]
    fn marks_every_page_and_removes_them_again() {
        let mut doc = open(3);
        doc.add_header_footer(r#"{"text":["Draft","","","","Page <<1>> of <<n>>",""]}"#).expect("adds a footer");
        doc.add_watermark(r#"{"text":"CONFIDENTIAL"}"#, None).expect("adds a watermark");
        doc.add_background(r#"{"color":[1,1,0.8]}"#, None).expect("adds a background");
        assert_eq!(hits(&mut doc, "Page 3 of 3"), [2]);
        assert_eq!(hits(&mut doc, "Draft"), [0, 1, 2]);
        assert_eq!(hits(&mut doc, "CONFIDENTIAL"), [0, 1, 2]);
        assert!(doc.add_watermark(r#"{"text":" "}"#, None).is_err());
        doc.remove_marks("watermark").expect("removes it");
        assert!(hits(&mut doc, "CONFIDENTIAL").is_empty());
        assert!(doc.remove_marks("nope").is_err());
        assert!(doc.add_header_footer(r#"{"text":["","","","","",""]}"#).is_err());
    }

    #[test]
    fn keeps_bookmarks_and_attachments() {
        let mut doc = open(3);
        doc.add_bookmark("[]", 0, "First", 0).expect("adds one");
        doc.add_bookmark("[]", 1, "Third", 2).expect("adds another");
        doc.add_bookmark("[1]", 0, "Detail", 2).expect("adds a child");
        let list: Vec<Value> = serde_json::from_str(&doc.bookmarks_json().expect("lists")).expect("json");
        assert_eq!(list.len(), 3);
        assert_eq!(list[1]["title"], "Third");
        assert_eq!(list[2]["path"], serde_json::json!([1, 0]));
        doc.rename_bookmark("[0]", "Start").expect("renames");
        doc.set_bookmark_page("[1]", 1).expect("repoints");
        doc.move_bookmark("[1, 0]", "[]", 0).expect("moves");
        let list: Vec<Value> = serde_json::from_str(&doc.bookmarks_json().expect("lists")).expect("json");
        assert_eq!(list[0]["title"], "Detail");
        doc.delete_bookmark("[0]").expect("deletes");
        assert!(doc.add_bookmark("[]", 0, "Nowhere", 9).is_err());
        doc.add_attachment("notes.txt", b"hello", Some("a note".into())).expect("attaches");
        let files: Vec<Value> = serde_json::from_str(&doc.attachments_json().expect("lists")).expect("json");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["name"], "notes.txt");
        assert_eq!(doc.attachment_bytes(0).expect("reads it back"), b"hello");
        let key = files[0]["key"].as_str().expect("key").to_string();
        doc.delete_attachment(&key).expect("removes it");
        assert_eq!(doc.attachments_json().expect("lists"), "[]");
    }

    #[test]
    fn lists_hidden_information() {
        let mut doc = open(1);
        doc.add_attachment("a.txt", b"x", None).expect("attaches");
        let found: Vec<Value> = serde_json::from_str(&doc.hidden_info_json().expect("scans")).expect("json");
        let n = found.iter().find(|f| f["id"] == "attachments").map(|f| f["count"].as_u64().unwrap_or(0));
        assert_eq!(n, Some(1));
        doc.remove_hidden(&["attachments".to_string()]).expect("removes it");
        assert_eq!(doc.attachments_json().expect("lists"), "[]");
        assert!(doc.remove_hidden(&[]).is_err());
    }
}
