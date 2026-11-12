use super::host::host;
use super::instances::instance_manager;
use super::model::{MAX_DEPTH, XfaBox, XfaDoc, XfaEffect, XfaEvent, clip, reset_names};
use super::nodes::{node_index, node_object, noop, resolve_som, resolver, sink, sink_apply, visible_children};
use super::{
    Attribute, Context, HashMap, HashSet, JsResult, JsString, JsValue, NativeFunction, ObjectInitializer, arg, function, js_string, platform, printd,
    printf, printx, s, scand, text,
};
use std::sync::Arc;

// ── xfa.host, xfa.layout, xfa.event, app, console ───────────────────────────────────────────

/// Records effect `e` on the host, against the run's limits, as [`XHost::push`] does.
fn effect(ctx: &mut Context, e: XfaEffect) -> JsResult<()> {
    host(ctx)?.borrow_mut().push(e);
    Ok(())
}

/// `xfa.host.messageBox(message, title, icon, buttons)`: records the message with its title,
/// icon and buttons, its text and title cut to [`MAX_LINE`] characters, and returns 1.
fn message_box(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let m = crate::globals::message_box_message(args, ctx)?;
    effect(ctx, XfaEffect::MessageBox(m))?;
    // Always OK for now: the viewer can't ask the user and run the script on with the answer
    // yet (#4291).
    Ok(JsValue::from(1.0))
}

/// `app.alert(cMsg, nIcon, nType, cTitle)` in an XFA form's script: the same message box, its
/// arguments in `app.alert`'s order (or one object with those keys).
fn app_alert(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let m = crate::globals::alert_message(args, ctx)?;
    effect(ctx, XfaEffect::MessageBox(m))?;
    // The OK button, as for `messageBox` (#4291).
    Ok(JsValue::from(1.0))
}

/// `xfa.host.resetData(names)`: asks to reset the comma-separated fields, or every field when
/// the list is empty.
pub(super) fn reset_data(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let list = text(&arg(args, 0), ctx)?;
    effect(ctx, XfaEffect::ResetData(reset_names(&list)))?;
    Ok(JsValue::undefined())
}

/// `xfa.host.print(…)`: asks to print; its arguments are ignored.
fn print(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    effect(ctx, XfaEffect::Print)?;
    Ok(JsValue::undefined())
}

/// `xfa.host.beep()`: asks to beep.
fn beep(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    effect(ctx, XfaEffect::Beep)?;
    Ok(JsValue::undefined())
}

/// `xfa.host.gotoURL(url)`: asks to open the URL.
fn goto_url(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let u = text(&arg(args, 0), ctx)?;
    effect(ctx, XfaEffect::LaunchUrl(u))?;
    Ok(JsValue::undefined())
}

/// `xfa.host.setFocus(target)`: asks to focus the node, given as a node object or as a SOM
/// expression resolved from the current node; an expression that names no node is passed on
/// as written.
fn set_focus(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let target = arg(args, 0);
    let som = match target.as_object() {
        Some(_) if node_index(&target, ctx).is_ok() => {
            let i = node_index(&target, ctx)?;
            host(ctx)?.borrow().nodes.get(i).map(|n| n.som.clone()).unwrap_or_default()
        }
        _ => {
            let expr = text(&target, ctx)?;
            let h = host(ctx)?.borrow();
            let found = resolve_som(&h, h.current, &expr);
            found.first().and_then(|i| h.nodes.get(*i)).map(|n| n.som.clone()).unwrap_or(expr)
        }
    };
    effect(ctx, XfaEffect::SetFocus(som))?;
    Ok(JsValue::undefined())
}

/// `app.execMenuItem(name)`: the effect for the request the shared menu table gives the name
/// (Save opens Save As too); an unknown name does nothing.
fn exec_menu_item(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let item = text(&arg(args, 0), ctx)?;
    match crate::menu_item(&item) {
        Some(crate::Request::SaveAs) => effect(ctx, XfaEffect::SaveAs)?,
        Some(crate::Request::Print) => effect(ctx, XfaEffect::Print)?,
        Some(crate::Request::ShowPanel(p)) => effect(ctx, XfaEffect::ShowPanel(p))?,
        _ => {}
    }
    Ok(JsValue::undefined())
}

pub(super) fn recalculate(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    effect(ctx, XfaEffect::Recalculate)?;
    Ok(JsValue::undefined())
}

pub(super) fn relayout(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    effect(ctx, XfaEffect::Relayout)?;
    Ok(JsValue::undefined())
}

fn console_println(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let m = clip(text(&arg(args, 0), ctx)?);
    host(ctx)?.borrow_mut().print(m);
    Ok(JsValue::undefined())
}

fn response(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::null())
}

fn layout_page0(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(host(ctx)?.borrow().doc.page as f64))
}

fn layout_page_count(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(host(ctx)?.borrow().doc.page_count as f64))
}

/// The first box laid out for the object at `som`.
pub(super) fn node_box(ctx: &Context, som: &str) -> Option<XfaBox> {
    host(ctx).ok()?.borrow().boxes.iter().find(|b| b.som == som).cloned()
}

/// The first box laid out for the object `args[0]` stands for, or the `index`-th one when it was split over several.
fn box_of(args: &[JsValue], index_at: usize, ctx: &mut Context) -> JsResult<Option<XfaBox>> {
    let Ok(idx) = node_index(&arg(args, 0), ctx) else { return Ok(None) };
    let index = if args.len() > index_at { arg(args, index_at).to_number(ctx).unwrap_or(0.0).max(0.0) as usize } else { 0 };
    let h = host(ctx)?.borrow();
    let Some(som) = h.nodes.get(idx).map(|n| n.som.clone()) else { return Ok(None) };
    let mut found = h.boxes.iter().filter(|b| b.som == som);
    Ok(found.nth(index).or_else(|| h.boxes.iter().find(|b| b.som == som)).cloned())
}

/// Points as the unit a script asked for (`in` by default).
fn in_unit(points: f64, unit: &str) -> f64 {
    match unit.trim() {
        "pt" => points,
        "cm" => points / 72.0 * 2.54,
        "mm" => points / 72.0 * 25.4,
        "mp" => points * 1000.0,
        _ => points / 72.0,
    }
}

fn layout_measure(args: &[JsValue], ctx: &mut Context, pick: fn(&XfaBox) -> f64) -> JsResult<JsValue> {
    let unit = if args.len() > 1 { text(&arg(args, 1), ctx)? } else { String::new() };
    Ok(JsValue::from(box_of(args, 2, ctx)?.map_or(0.0, |b| in_unit(pick(&b), &unit))))
}

fn layout_x(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    layout_measure(args, ctx, |b| b.x)
}

fn layout_y(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    layout_measure(args, ctx, |b| b.y)
}

fn layout_w(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    layout_measure(args, ctx, |b| b.w)
}

fn layout_h(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    layout_measure(args, ctx, |b| b.h)
}

/// `layout.page(node)`: the page (1-based) the object starts on.
fn layout_page_of(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // an object not laid out (or no layout at all): the page the script runs for
    let placed = if args.is_empty() { None } else { box_of(args, 1, ctx)? };
    Ok(JsValue::from(placed.map_or(host(ctx)?.borrow().doc.page as f64 + 1.0, |b| b.page as f64 + 1.0)))
}

/// `layout.pageSpan(node, index)`: how many pages the object covers.
fn layout_page_span(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Ok(idx) = node_index(&arg(args, 0), ctx) else { return Ok(JsValue::from(0.0)) };
    let h = host(ctx)?.borrow();
    let Some(som) = h.nodes.get(idx).map(|n| n.som.as_str()) else { return Ok(JsValue::from(0.0)) };
    let pages: HashSet<usize> = h.boxes.iter().filter(|b| b.som == som).map(|b| b.page).collect();
    Ok(JsValue::from(pages.len() as f64))
}

/// `layout.pageContent(page, class, relevant)`: the objects laid out on a page (0-based), the content areas first; with
/// `relevant` the page area and its own content, without it the flow.
fn layout_page_content(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let page = arg(args, 0).to_number(ctx).unwrap_or(0.0).max(0.0) as usize;
    let class = if args.len() > 1 { text(&arg(args, 1), ctx)? } else { String::new() };
    let relevant = arg(args, 2).to_boolean();
    let found: Vec<usize> = {
        let h = host(ctx)?.borrow();
        let on_page = h.boxes.iter().filter(|b| b.page == page && (class.is_empty() || b.class == class));
        let (areas, rest): (Vec<&XfaBox>, Vec<&XfaBox>) = on_page
            .filter(|b| if relevant { b.master } else { !b.master || b.class == "contentArea" })
            .partition(|b| b.class == "contentArea" || b.class == "pageArea");
        areas.into_iter().chain(rest).filter_map(|b| h.nodes.iter().position(|n| n.som == b.som && !n.gone)).collect()
    };
    super::instances::list_object(ctx, &found)
}

/// `__xfa_text(name)`: the text of a data script object.
fn xfa_text(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = text(&arg(args, 0), ctx)?;
    let found = host(ctx)?.borrow().texts.iter().find(|(n, _)| *n == name).map(|(_, t)| std::sync::Arc::clone(t));
    Ok(s(found.as_deref().unwrap_or_default()))
}

fn null_value(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::null())
}

pub(super) fn false_value(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(false))
}

/// A list with nothing in it (`length` 0): the deltas, the page content, the nodes of a dataset that is not there.
pub(super) fn empty_list(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    super::instances::list_object(ctx, &[])
}

fn xfa_resolve(ctx: &mut Context, many: bool) -> JsResult<JsValue> {
    let current = host(ctx)?.borrow().current;
    resolver(ctx, current, many)
}

/// Defines the globals for one script on node `current`: `xfa` (`form`, `host`, `layout`, `event`,
/// `resolveNode` and more), `event` as the same object as `xfa.event`, `app`, `console`, `util`,
/// and the nodes in scope by name, with `_name` for each repeatable one's instance manager.
pub(super) fn install(ctx: &mut Context, event: &XfaEvent, doc: &XfaDoc, current: usize, code: &str) -> JsResult<Vec<String>> {
    let rw = Attribute::CONFIGURABLE;
    let form = node_object(ctx, 0)?;
    let this_node = node_object(ctx, current)?;
    let host_obj = ObjectInitializer::new(ctx)
        .function(NativeFunction::from_fn_ptr(message_box), js_string!("messageBox"), 4)
        .function(NativeFunction::from_fn_ptr(reset_data), js_string!("resetData"), 1)
        .function(NativeFunction::from_fn_ptr(print), js_string!("print"), 8)
        .function(NativeFunction::from_fn_ptr(beep), js_string!("beep"), 1)
        .function(NativeFunction::from_fn_ptr(goto_url), js_string!("gotoURL"), 2)
        .function(NativeFunction::from_fn_ptr(set_focus), js_string!("setFocus"), 1)
        .function(NativeFunction::from_fn_ptr(response), js_string!("response"), 4)
        .function(NativeFunction::from_fn_ptr(noop), js_string!("openList"), 1)
        .function(NativeFunction::from_fn_ptr(noop), js_string!("exportData"), 2)
        .function(NativeFunction::from_fn_ptr(noop), js_string!("importData"), 1)
        .function(NativeFunction::from_fn_ptr(noop), js_string!("pageDown"), 0)
        .function(NativeFunction::from_fn_ptr(noop), js_string!("pageUp"), 0)
        .function(NativeFunction::from_fn_ptr(noop), js_string!("getFocus"), 0)
        .property(js_string!("currentPage"), JsValue::from(doc.page as f64), rw)
        .property(js_string!("numPages"), JsValue::from(doc.page_count as f64), rw)
        .property(js_string!("name"), s("Acrobat"), Attribute::READONLY)
        .property(js_string!("appType"), s("Exchange-Pro"), Attribute::READONLY)
        .property(js_string!("version"), s("11"), Attribute::READONLY)
        .property(js_string!("variation"), s("Full"), Attribute::READONLY)
        .property(js_string!("language"), s("en"), Attribute::READONLY)
        .property(js_string!("platform"), s(platform()), Attribute::READONLY)
        .property(js_string!("calculationsEnabled"), JsValue::from(true), rw)
        .property(js_string!("validationsEnabled"), JsValue::from(true), rw)
        .property(js_string!("title"), s(&doc.file_name), rw)
        .build();
    let layout = ObjectInitializer::new(ctx)
        .function(NativeFunction::from_fn_ptr(layout_page_of), js_string!("page"), 1)
        .function(NativeFunction::from_fn_ptr(layout_page0), js_string!("absPage"), 1)
        .function(NativeFunction::from_fn_ptr(layout_page_count), js_string!("pageCount"), 0)
        .function(NativeFunction::from_fn_ptr(layout_page_count), js_string!("absPageCount"), 0)
        // On screen every page is a sheet of its own: the sheet is the page, counted from 0
        // as absPage counts (a form shows `sheet(this) + 1` as the sheet number), and the
        // sheet count is the page count.
        .function(NativeFunction::from_fn_ptr(layout_page0), js_string!("sheet"), 1)
        .function(NativeFunction::from_fn_ptr(layout_page_count), js_string!("sheetCount"), 0)
        .function(NativeFunction::from_fn_ptr(relayout), js_string!("relayout"), 0)
        .function(NativeFunction::from_fn_ptr(relayout), js_string!("relayoutPageArea"), 0)
        .function(NativeFunction::from_fn_ptr(layout_h), js_string!("h"), 3)
        .function(NativeFunction::from_fn_ptr(layout_w), js_string!("w"), 3)
        .function(NativeFunction::from_fn_ptr(layout_x), js_string!("x"), 3)
        .function(NativeFunction::from_fn_ptr(layout_y), js_string!("y"), 3)
        .function(NativeFunction::from_fn_ptr(layout_page_span), js_string!("pageSpan"), 2)
        .function(NativeFunction::from_fn_ptr(layout_page_content), js_string!("pageContent"), 3)
        .function(NativeFunction::from_fn_ptr(empty_list), js_string!("pageArea"), 1)
        .property(js_string!("ready"), JsValue::from(true), Attribute::READONLY)
        .build();
    let ev = ObjectInitializer::new(ctx)
        .property(js_string!("name"), s(&event.activity), Attribute::all())
        .property(js_string!("newText"), s(&event.new_text), Attribute::all())
        .property(js_string!("prevText"), s(&event.prev_text), Attribute::all())
        .property(js_string!("fullText"), s(&event.new_text), Attribute::all())
        .property(js_string!("change"), s(""), Attribute::all())
        .property(js_string!("target"), this_node.clone(), Attribute::all())
        .property(js_string!("selStart"), JsValue::from(0.0), Attribute::all())
        .property(js_string!("selEnd"), JsValue::from(0.0), Attribute::all())
        .property(js_string!("cancelAction"), JsValue::from(false), Attribute::all())
        .property(js_string!("reenter"), JsValue::from(false), Attribute::all())
        .property(js_string!("shift"), JsValue::from(false), Attribute::all())
        .property(js_string!("modifier"), JsValue::from(false), Attribute::all())
        .property(js_string!("keyDown"), JsValue::from(false), Attribute::all())
        .property(js_string!("commitKey"), JsValue::from(0.0), Attribute::all())
        .function(NativeFunction::from_fn_ptr(noop), js_string!("emit"), 0)
        .function(NativeFunction::from_fn_ptr(noop), js_string!("reset"), 0)
        .build();
    // The datasets are not editable from a script here: creating a node or loading XML into one changes nothing.
    let dataset_nodes = sink(ctx)?;
    let datasets = ObjectInitializer::new(ctx)
        .property(js_string!("data"), form.clone(), rw)
        .property(js_string!("nodes"), dataset_nodes, rw)
        .function(NativeFunction::from_fn_ptr(sink_apply), js_string!("createNode"), 2)
        .function(NativeFunction::from_fn_ptr(null_value), js_string!("resolveNode"), 1)
        .function(NativeFunction::from_fn_ptr(empty_list), js_string!("resolveNodes"), 1)
        .build();
    let template = sink(ctx)?;
    let resolve_one = xfa_resolve(ctx, false)?;
    let resolve_many = xfa_resolve(ctx, true)?;
    // The SOM shortcuts a script may use as variables.
    for (name, value) in [
        ("$", &this_node),
        ("$form", &form),
        ("$record", &form),
        ("$data", &form),
        ("$host", &host_obj.clone().into()),
        ("$layout", &layout.clone().into()),
        ("$template", &template),
        ("$event", &ev.clone().into()),
    ] {
        ctx.register_global_property(JsString::from(name), value.clone(), rw)?;
    }
    let xfa = ObjectInitializer::new(ctx)
        .property(js_string!("form"), form.clone(), rw)
        .property(js_string!("host"), host_obj, rw)
        .property(js_string!("layout"), layout, rw)
        .property(js_string!("event"), ev.clone(), rw)
        .property(js_string!("datasets"), datasets, rw)
        .property(js_string!("record"), form.clone(), rw)
        .property(js_string!("template"), template, rw)
        .property(js_string!("resolveNode"), resolve_one, rw)
        .property(js_string!("resolveNodes"), resolve_many, rw)
        .function(NativeFunction::from_fn_ptr(recalculate), js_string!("recalculate"), 1)
        .function(NativeFunction::from_fn_ptr(relayout), js_string!("remerge"), 0)
        .build();
    ctx.register_global_property(js_string!("xfa"), xfa, rw)?;
    ctx.register_global_property(js_string!("event"), ev, rw)?;
    ctx.register_global_property(js_string!("__xfa_this"), this_node, rw)?;
    let plug_ins: JsValue = boa_engine::object::builtins::JsArray::new(ctx)?.into();
    let app = ObjectInitializer::new(ctx)
        .function(NativeFunction::from_fn_ptr(app_alert), js_string!("alert"), 4)
        .function(NativeFunction::from_fn_ptr(beep), js_string!("beep"), 1)
        .function(NativeFunction::from_fn_ptr(response), js_string!("response"), 1)
        .function(NativeFunction::from_fn_ptr(goto_url), js_string!("launchURL"), 2)
        .function(NativeFunction::from_fn_ptr(exec_menu_item), js_string!("execMenuItem"), 1)
        .function(NativeFunction::from_fn_ptr(noop), js_string!("setTimeOut"), 2)
        .function(NativeFunction::from_fn_ptr(noop), js_string!("setInterval"), 2)
        .function(NativeFunction::from_fn_ptr(noop), js_string!("clearTimeOut"), 1)
        .function(NativeFunction::from_fn_ptr(noop), js_string!("clearInterval"), 1)
        .property(js_string!("plugIns"), plug_ins, Attribute::READONLY)
        .property(js_string!("viewerType"), s("Exchange-Pro"), Attribute::READONLY)
        .property(js_string!("viewerVariation"), s("Full"), Attribute::READONLY)
        .property(js_string!("viewerVersion"), JsValue::from(24.0), Attribute::READONLY)
        .property(js_string!("platform"), s(platform()), Attribute::READONLY)
        .property(js_string!("language"), s("ENU"), Attribute::READONLY)
        .build();
    ctx.register_global_property(js_string!("app"), app, rw)?;
    let console = ObjectInitializer::new(ctx)
        .function(NativeFunction::from_fn_ptr(console_println), js_string!("println"), 1)
        .function(NativeFunction::from_fn_ptr(noop), js_string!("show"), 0)
        .function(NativeFunction::from_fn_ptr(noop), js_string!("hide"), 0)
        .function(NativeFunction::from_fn_ptr(noop), js_string!("clear"), 0)
        .build();
    ctx.register_global_property(js_string!("console"), console, rw)?;
    let util = ObjectInitializer::new(ctx)
        .function(NativeFunction::from_fn_ptr(printf), js_string!("printf"), 1)
        .function(NativeFunction::from_fn_ptr(printd), js_string!("printd"), 2)
        .function(NativeFunction::from_fn_ptr(printx), js_string!("printx"), 2)
        .function(NativeFunction::from_fn_ptr(scand), js_string!("scand"), 2)
        .build();
    ctx.register_global_property(js_string!("util"), util, rw)?;
    let xfa_text_fn = function(ctx, NativeFunction::from_fn_ptr(xfa_text));
    ctx.register_global_property(js_string!("__xfa_text"), xfa_text_fn, rw)?;
    let build_fn = function(ctx, NativeFunction::from_fn_ptr(super::objects::build));
    ctx.register_global_property(js_string!("__xfa_build"), build_fn, rw)?;
    // Unqualified names: the current node's children, its siblings, then each ancestor's
    // children, the nearest first. Instance managers as `_name`.
    let scopes: Vec<usize> = {
        let h = host(ctx)?.borrow();
        let mut out = vec![current];
        let mut at = h.nodes.get(current).and_then(|n| n.parent);
        let mut guard = 0;
        while let Some(p) = at
            && guard < MAX_DEPTH
        {
            out.push(p);
            at = h.nodes.get(p).and_then(|n| n.parent);
            guard += 1;
        }
        out
    };
    // Names that stay what JavaScript (or this host) means by them: a field called `Date`
    // is reached as `this.parent.Date` or by resolveNode.
    const RESERVED: &[&str] = &[
        "xfa",
        "event",
        "app",
        "console",
        "util",
        "this",
        "__xfa_this",
        "__xfa_src",
        "eval",
        "arguments",
        "globalThis",
        "window",
        "undefined",
        "NaN",
        "Infinity",
        "Object",
        "Function",
        "Array",
        "Number",
        "String",
        "Boolean",
        "Symbol",
        "Date",
        "RegExp",
        "Error",
        "TypeError",
        "RangeError",
        "SyntaxError",
        "Math",
        "JSON",
        "Map",
        "Set",
        "WeakMap",
        "WeakSet",
        "Promise",
        "Proxy",
        "Reflect",
        "parseInt",
        "parseFloat",
        "isNaN",
        "isFinite",
        "encodeURIComponent",
        "decodeURIComponent",
        "encodeURI",
        "decodeURI",
        "escape",
        "unescape",
        "BigInt",
        "ArrayBuffer",
        "DataView",
        "Intl",
    ];
    let mut defined: Vec<String> = RESERVED.iter().map(ToString::to_string).collect();
    // Every global this set-up gives, for the next script to take away.
    let mut installed: Vec<String> = [
        "xfa",
        "event",
        "__xfa_this",
        "app",
        "console",
        "util",
        "__xfa_text",
        "__xfa_src",
        "$",
        "$form",
        "$record",
        "$data",
        "$host",
        "$layout",
        "$template",
        "$event",
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    for scope in scopes {
        let kids: Vec<(usize, String, bool)> = {
            let h = host(ctx)?.borrow();
            visible_children(&h, scope).into_iter().filter_map(|c| h.nodes.get(c).map(|k| (c, k.name.clone(), k.repeatable && !k.gone))).collect()
        };
        for (c, name, repeatable) in kids {
            if !is_identifier(&name) || defined.contains(&name) {
                continue;
            }
            let obj = node_object(ctx, c)?;
            ctx.register_global_property(JsString::from(name.as_str()), obj, rw)?;
            installed.push(name.clone());
            if repeatable {
                let im = instance_manager(ctx, scope, &name)?;
                ctx.register_global_property(JsString::from(format!("_{name}").as_str()), im, rw)?;
                installed.push(format!("_{name}"));
            }
            defined.push(name);
        }
    }
    // A name the scopes do not reach but the code uses and the form holds once is that object (Acrobat finds it too).
    let words: HashSet<&str> = code.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')).filter(|w| !w.is_empty()).collect();
    let defined: HashSet<String> = defined.into_iter().collect();
    // the script objects it reaches name nodes too (`NotePage.presence` in a library), which they find the same way
    let object_words: Vec<Arc<HashSet<String>>> = host(ctx)?.borrow().codes.iter().map(|c| words_of(&c.2)).collect();
    let used = |name: &str| words.contains(name) || object_words.iter().any(|w| w.contains(name));
    let mut unique: HashMap<String, Option<usize>> = HashMap::new();
    // `_name`: the instance manager of the repeatable subforms of that name, when they are under one parent
    let mut managers: HashMap<String, Option<usize>> = HashMap::new();
    {
        let h = host(ctx)?.borrow();
        for (i, n) in h.nodes.iter().enumerate() {
            if n.gone || !is_identifier(&n.name) {
                continue;
            }
            if n.repeatable
                && let Some(p) = n.parent
            {
                let im = format!("_{}", n.name);
                if !defined.contains(&im) && used(&im) {
                    managers.entry(n.name.clone()).and_modify(|e| *e = e.filter(|q| *q == p)).or_insert(Some(p));
                }
            }
            if !used(&n.name) || defined.contains(&n.name) {
                continue;
            }
            unique.entry(n.name.clone()).and_modify(|e| *e = None).or_insert(Some(i));
        }
    }
    for (name, parent) in managers {
        if let Some(p) = parent {
            let im = instance_manager(ctx, p, &name)?;
            ctx.register_global_property(JsString::from(format!("_{name}").as_str()), im, rw)?;
            installed.push(format!("_{name}"));
        }
    }
    let mut taken: HashSet<String> = defined.clone();
    for (name, node) in unique {
        if let Some(c) = node {
            let obj = node_object(ctx, c)?;
            ctx.register_global_property(JsString::from(name.as_str()), obj, rw)?;
            taken.insert(name.clone());
            installed.push(name);
        }
    }
    // The script's node is in scope too, as in Acrobat: a member read without `this.` (`parent`, `caption`, `rawValue`) is the
    // node's, unless something nearer has the name.
    let members: Vec<&str> = NODE_MEMBERS.iter().copied().filter(|m| words.contains(m) && !taken.contains(*m)).collect();
    if !members.is_empty() {
        let list = members.iter().map(|m| format!("{m:?}")).collect::<Vec<_>>().join(", ");
        let code = format!(
            "(function (busy) {{ [{list}].forEach(function (n) {{ Object.defineProperty(globalThis, n, {{ configurable: true, \
             get: function () {{ if (busy[n]) return undefined; busy[n] = true; \
               try {{ var v = __xfa_this[n]; return /^(resolveNodes?|exec[A-Z]|[a-z]+Items?$|boundItem|get[A-Z])/.test(n) && v ? v.bind(__xfa_this) : v; }} \
               finally {{ busy[n] = false; }} }}, \
             set: function (v) {{ __xfa_this[n] = v; }} }}); }}); }})({{}});"
        );
        ctx.eval(boa_engine::Source::from_bytes(code.as_bytes()))?;
        installed.extend(members.iter().map(ToString::to_string));
    }
    Ok(installed)
}

/// The identifier-like words of a script object's code, read once per object.
fn words_of(code: &Arc<str>) -> Arc<HashSet<String>> {
    /// Word sets by where the code is.
    type Words = HashMap<(usize, usize), Arc<HashSet<String>>>;
    thread_local! {
        static WORDS: std::cell::RefCell<Words> = std::cell::RefCell::new(HashMap::new());
    }
    let at = (code.as_ptr() as usize, code.len());
    WORDS.with(|w| {
        let mut w = w.borrow_mut();
        if let Some(known) = w.get(&at) {
            return Arc::clone(known);
        }
        let set: HashSet<String> =
            code.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')).filter(|w| !w.is_empty()).map(str::to_string).collect();
        let set = Arc::new(set);
        if w.len() >= 1024 {
            w.clear();
        }
        w.insert(at, Arc::clone(&set));
        set
    })
}

/// Members of the script's node it may read without `this.`: names a script would not use for a variable of its own.
const NODE_MEMBERS: &[&str] = &[
    "parent",
    "caption",
    "rawValue",
    "formattedValue",
    "presence",
    "access",
    "instanceIndex",
    "instanceManager",
    "somExpression",
    "isNull",
    "mandatory",
    "border",
    "font",
    "ui",
    "assist",
    "occur",
    "para",
    "margin",
    "resolveNode",
    "resolveNodes",
    "execEvent",
    "execCalculate",
    "execValidate",
    "execInitialize",
    "clearItems",
    "addItem",
    "boundItem",
    "getDisplayItem",
    "getSaveItem",
    "deleteItem",
    "setItems",
    "selectedIndex",
];

pub(super) fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
        && !matches!(
            name,
            "var"
                | "let"
                | "const"
                | "function"
                | "return"
                | "if"
                | "else"
                | "for"
                | "while"
                | "new"
                | "null"
                | "true"
                | "false"
                | "undefined"
                | "typeof"
        )
}
