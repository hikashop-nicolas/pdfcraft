//! What a form field does when it is used: its actions per trigger, JavaScript, and running one.
//! A script runs in the engine's sandbox; what it asks of the page (print, open a link, go to a
//! page) comes back as requests for the page to carry out.

use pdfcraft_engine::js::{JsOutput, Outcome, Request};
use pdfcraft_engine::{Edit, FieldAction, FieldTrigger as Trigger};
use serde_json::{Value, json};

use crate::core::Document;

fn list(v: &Value, key: &str) -> Vec<String> {
    v.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).take(500).collect()).unwrap_or_default()
}

fn action_json(a: &FieldAction) -> Value {
    match a {
        FieldAction::JavaScript(s) => json!({"kind": "javascript", "script": s}),
        FieldAction::Uri(u) => json!({"kind": "uri", "url": u}),
        FieldAction::Reset(f) => json!({"kind": "reset", "fields": f}),
        FieldAction::Named(n) => json!({"kind": "named", "name": n}),
        FieldAction::GoTo(p) => json!({"kind": "goto", "page": p}),
        FieldAction::ShowHide { fields, hide } => json!({"kind": "showhide", "fields": fields, "hide": hide}),
        FieldAction::Submit(u) => json!({"kind": "submit", "url": u}),
        FieldAction::Other(s) => json!({"kind": "other", "text": s}),
    }
}

fn parse_action(v: &Value) -> Result<FieldAction, String> {
    let text = |key: &str| v.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
    Ok(match v["kind"].as_str().ok_or("an action has a kind")? {
        "javascript" => FieldAction::JavaScript(text("script")),
        "uri" => FieldAction::Uri(text("url")),
        "reset" => FieldAction::Reset(list(v, "fields")),
        "named" => FieldAction::Named(text("name")),
        "goto" => FieldAction::GoTo(v["page"].as_u64().ok_or("a page number")? as usize),
        "showhide" => FieldAction::ShowHide { fields: list(v, "fields"), hide: v["hide"].as_bool().unwrap_or(true) },
        "submit" => FieldAction::Submit(text("url")),
        "other" => FieldAction::Other(text("text")),
        other => return Err(format!("unknown action {other:?}")),
    })
}

fn request_json(r: &Request) -> Value {
    match r {
        Request::Reset(n) => json!({"type": "reset", "fields": n}),
        Request::Print => json!({"type": "print"}),
        Request::GoToPage(p) => json!({"type": "goto", "page": p}),
        Request::LaunchUrl(u) => json!({"type": "url", "url": u}),
        Request::Submit(u) => json!({"type": "submit", "url": u}),
        Request::Focus(f) => json!({"type": "focus", "field": f}),
        Request::Beep => json!({"type": "beep"}),
        Request::SaveAs => json!({"type": "saveAs"}),
        Request::ShowPanel(p) => json!({"type": "panel", "panel": p.id()}),
    }
}

fn output_json(alerts: &[String], console: &[String], requests: &[Request], errors: &[String]) -> Value {
    json!({"alerts": alerts, "console": console, "requests": requests.iter().map(request_json).collect::<Vec<_>>(), "errors": errors})
}

fn outcome_json(o: &Outcome) -> Value {
    let errors: Vec<String> = o.error.iter().chain(o.doc_errors.iter()).cloned().collect();
    let mut out = output_json(&o.alerts, &o.console, &o.requests, &errors);
    out["result"] = json!(o.result);
    out
}

/// A JavaScript string literal for `s`.
fn quoted(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

impl Document {
    /// A field's actions as JSON: `[{"trigger": "mouse_up", "kind": "javascript", ...}]` (kinds: javascript
    /// `script`, uri `url`, reset `fields`, named `name`, goto `page`, showhide `fields` and `hide`, submit `url`, other `text`).
    pub fn field_actions_json(&self, name: &str) -> Result<String, String> {
        let list: Vec<Value> = self
            .doc()?
            .field_actions(name)
            .iter()
            .map(|(t, a)| {
                let mut v = action_json(a);
                v["trigger"] = json!(t.id());
                v
            })
            .collect();
        serde_json::to_string(&list).map_err(|e| e.to_string())
    }

    /// Replaces a field's actions with a list shaped like [`Document::field_actions_json`]'s.
    pub fn set_field_actions(&mut self, name: &str, actions: &str) -> Result<(), String> {
        let v: Value = serde_json::from_str(actions).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for a in v.as_array().ok_or("actions are a list")? {
            let t = Trigger::from_id(a["trigger"].as_str().ok_or("an action has a trigger")?).ok_or("unknown trigger")?;
            out.push((t, parse_action(a)?));
        }
        self.apply(Edit::SetFieldActions { name: name.to_string(), actions: out })
    }

    /// A field's custom scripts as JSON, `{"keystroke"?, "format"?, "validate"?, "calculate"?}`.
    pub fn field_scripts_json(&self, name: &str) -> Result<String, String> {
        let map: serde_json::Map<String, Value> = self.doc()?.field_scripts(name).into_iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
        Ok(Value::Object(map).to_string())
    }

    /// Sets or removes (an empty script) the script a field runs for `event`: keystroke, format, validate, calculate or mouse_up.
    pub fn set_field_script(&mut self, name: &str, event: &str, script: &str) -> Result<(), String> {
        let script = (!script.trim().is_empty()).then(|| script.to_string());
        self.apply(Edit::SetFieldScript { name: name.to_string(), event: event.to_string(), script })
    }

    /// Runs a script as `target` field's button click (or on its own); field changes are applied as one step.
    pub fn run_javascript(&mut self, script: &str, target: Option<&str>) -> Result<String, String> {
        let o = self.session.run_javascript(self.id, script, target).map_err(|e| e.to_string())?;
        Ok(outcome_json(&o).to_string())
    }

    /// Carries out an action as `target` field's button click: scripts, resets and showing or hiding fields run in the
    /// engine, the rest comes back as requests for the page (a link, a page to go to, a menu item, a form to submit).
    pub fn run_action(&mut self, action: &str, target: Option<&str>) -> Result<String, String> {
        let v: Value = serde_json::from_str(action).map_err(|e| e.to_string())?;
        // `{"kind": "click"}`: the button's own click, which an XFA form keeps in its template (the PDF has no action for it)
        if v["kind"] == "click" {
            return self.run_javascript("", Some(target.ok_or("a click needs a field")?));
        }
        let script = match parse_action(&v)? {
            FieldAction::JavaScript(s) => s,
            FieldAction::Reset(f) => format!("this.resetForm({});", serde_json::to_string(&f).map_err(|e| e.to_string())?),
            FieldAction::ShowHide { fields, hide } => {
                let shown = if hide { "display.hidden" } else { "display.visible" };
                fields.iter().map(|f| format!("this.getField({}).display = {shown};", quoted(f))).collect::<String>()
            }
            FieldAction::Uri(u) => return Ok(output_json(&[], &[], &[Request::LaunchUrl(u)], &[]).to_string()),
            FieldAction::GoTo(p) => return Ok(output_json(&[], &[], &[Request::GoToPage(p)], &[]).to_string()),
            FieldAction::Submit(u) => return Ok(output_json(&[], &[], &[Request::Submit(u)], &[]).to_string()),
            FieldAction::Named(n) => {
                let r = if n == "Print" { json!({"type": "print"}) } else { json!({"type": "named", "name": n}) };
                return Ok(json!({"alerts": [], "console": [], "requests": [r], "errors": []}).to_string());
            }
            FieldAction::Other(_) => return Err("this action cannot be run".into()),
        };
        self.run_javascript(&script, target)
    }

    /// What field scripts printed or asked for during edits since the last call (alerts, requests, errors).
    pub fn take_js_output(&mut self) -> String {
        let o: JsOutput = self.session.take_js_output(self.id);
        output_json(&o.alerts, &o.console, &o.requests, &o.errors).to_string()
    }

    pub fn set_javascript(&mut self, on: bool) {
        self.session.set_javascript(on);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use crate::core::Document;
    use crate::core::tests::fixture;

    fn open() -> Document {
        let mut doc = Document::open(&fixture(1), None, || 1_700_000_000).expect("opens");
        doc.add_field(0, [20.0, 20.0, 120.0, 42.0], r#"{"type":"text","name":"Qty"}"#).expect("a text field");
        doc.add_field(0, [20.0, 60.0, 120.0, 82.0], r#"{"type":"text","name":"Total"}"#).expect("a text field");
        doc.add_field(0, [20.0, 100.0, 120.0, 122.0], r#"{"type":"button","name":"Go","caption":"Go"}"#).expect("a button");
        doc
    }

    fn caption_of(doc: &Document, name: &str) -> Option<String> {
        let fields: Vec<Value> = serde_json::from_str(&doc.fields_json(0).expect("fields")).expect("json");
        fields.iter().find(|f| f["name"] == name).and_then(|f| f["caption"].as_str().map(str::to_string))
    }

    fn value_of(doc: &Document, name: &str) -> Option<String> {
        let fields: Vec<Value> = serde_json::from_str(&doc.fields_json(0).expect("fields")).expect("json");
        fields.iter().find(|f| f["name"] == name).and_then(|f| f["value"].as_str().map(str::to_string))
    }

    #[test]
    fn a_button_keeps_its_actions_and_runs_them() {
        let mut doc = open();
        doc.set_field("Qty", r#"{"text":"4"}"#).expect("fills it");
        let js = r#"[{"trigger":"mouse_up","kind":"javascript","script":"this.getField('Total').value = Number(this.getField('Qty').value) * 5; app.alert('done'); this.print();"}]"#;
        doc.set_field_actions("Go", js).expect("sets the action");
        let back: Vec<Value> = serde_json::from_str(&doc.field_actions_json("Go").expect("lists them")).expect("json");
        assert_eq!(back.len(), 1);
        assert_eq!(back[0]["kind"], "javascript");
        assert_eq!(back[0]["trigger"], "mouse_up");
        let out: Value = serde_json::from_str(&doc.run_action(&back[0].to_string(), Some("Go")).expect("runs")).expect("json");
        assert_eq!(out["alerts"][0], "done");
        assert_eq!(out["requests"][0]["type"], "print");
        assert_eq!(value_of(&doc, "Total").as_deref(), Some("20"));
        doc.run_action(r#"{"kind":"reset","fields":[]}"#, Some("Go")).expect("resets");
        assert_eq!(value_of(&doc, "Qty").as_deref().unwrap_or(""), "");
        let link: Value = serde_json::from_str(&doc.run_action(r#"{"kind":"uri","url":"https://example.org"}"#, None).expect("link")).expect("json");
        assert_eq!(link["requests"][0]["url"], "https://example.org");
        doc.set_field_props("Go", r#"{"caption":"Send it"}"#).expect("relabels it");
        assert_eq!(caption_of(&doc, "Go").as_deref(), Some("Send it"));
        doc.set_field_actions("Go", "[]").expect("clears them");
        assert_eq!(doc.field_actions_json("Go").expect("lists them"), "[]");
    }

    #[test]
    fn a_field_keeps_a_calculate_script() {
        let mut doc = open();
        doc.set_field("Qty", r#"{"text":"3"}"#).expect("fills it");
        doc.set_field_script("Total", "calculate", "event.value = Number(this.getField('Qty').value) * 2;").expect("sets it");
        let scripts: Value = serde_json::from_str(&doc.field_scripts_json("Total").expect("reads them")).expect("json");
        assert!(scripts["calculate"].as_str().is_some_and(|s| s.contains("getField")), "{scripts}");
        assert_eq!(value_of(&doc, "Total").as_deref(), Some("6"), "the total follows the quantity");
        doc.set_field("Qty", r#"{"text":"5"}"#).expect("changes it");
        assert_eq!(value_of(&doc, "Total").as_deref(), Some("10"));
        doc.set_field_script("Total", "calculate", "").expect("removes it");
        assert_eq!(doc.field_scripts_json("Total").expect("reads them"), "{}");
    }

    #[test]
    fn field_scripts_run_during_a_fill() {
        let mut doc = open();
        doc.set_field_actions("Total", r#"[{"trigger":"mouse_up","kind":"showhide","fields":["Qty"],"hide":true}]"#).expect("sets it");
        doc.run_action(r#"{"kind":"showhide","fields":["Qty"],"hide":true}"#, Some("Go")).expect("hides it");
        let out: Value =
            serde_json::from_str(&doc.run_action(r#"{"kind":"javascript","script":"nosuchthing();"}"#, None).expect("runs")).expect("json");
        assert!(out["errors"].as_array().is_some_and(|e| !e.is_empty()), "a failing script reports its error: {out}");
        assert!(doc.run_action(r#"{"kind":"nope"}"#, None).is_err());
    }
}
