use super::globals::{empty_list, false_value, is_identifier, node_box, recalculate, relayout, reset_data};
use super::host::{HNode, XHost, host};
use super::instances::{instance_manager, list_object};
use super::model::{MAX_DEPTH, XfaEffect, XfaKind};
use super::{
    Attribute, Context, JsObject, JsProxy, JsResult, JsString, JsValue, NativeFunction, ObjectInitializer, arg, as_number, error, function,
    js_string, s, text,
};

// ── node objects ────────────────────────────────────────────────────────────────────────────

const IDX: &str = "__xfaNode";

pub(super) fn node_index(target: &JsValue, ctx: &mut Context) -> JsResult<usize> {
    let o = target.as_object().ok_or_else(|| error("not an XFA node"))?;
    let v = o.get(JsString::from(IDX), ctx)?;
    let n = v.to_number(ctx)?;
    if !n.is_finite() || n < 0.0 {
        return Err(error("not an XFA node"));
    }
    Ok(n as usize)
}

/// A Proxy standing for node `idx`.
pub(super) fn node_object(ctx: &mut Context, idx: usize) -> JsResult<JsValue> {
    let target = ObjectInitializer::new(ctx).property(JsString::from(IDX), JsValue::from(idx as f64), Attribute::READONLY).build();
    let proxy = JsProxy::builder(target).get(node_get).set(node_set).has(node_has).build(ctx)?;
    Ok(proxy.into())
}

/// An object that takes any property read or write without complaint (`this.border.fill…`,
/// `this.fillColor = …`): what a script sets there changes nothing here.
pub(super) fn sink(ctx: &mut Context) -> JsResult<JsValue> {
    // A function underneath, so a call on it (`this.ui.resolveNode("#text")`) works too and gives another one.
    let target: JsObject = function(ctx, NativeFunction::from_fn_ptr(noop)).into();
    let proxy = JsProxy::builder(target).get(sink_get).set(sink_set).apply(sink_apply).build(ctx)?;
    Ok(proxy.into())
}

pub(super) fn sink_apply(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    sink(ctx)
}

fn sink_get(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let key = arg(args, 1);
    if key.as_symbol().is_some() {
        return Ok(JsValue::undefined());
    }
    match text(&key, ctx)?.as_str() {
        "length" => Ok(JsValue::from(0)),
        "toString" | "valueOf" => Ok(function(ctx, NativeFunction::from_fn_ptr(empty_text)).into()),
        _ => sink(ctx),
    }
}

pub(super) fn empty_text(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(s(""))
}

fn sink_set(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(true))
}

fn node_has(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(true))
}

fn value_js(h: &HNode) -> JsValue {
    if h.value.is_empty() {
        return JsValue::null();
    }
    if h.numeric
        && let Some(n) = as_number(&h.value)
    {
        return JsValue::from(n);
    }
    s(&h.value)
}

/// The live children of `parent`, an unnamed container's children (and its own, which a script can reach by its
/// `#subform` name) counted as `parent`'s: a name does not stop at it.
pub(crate) fn visible_children(h: &XHost, parent: usize) -> Vec<usize> {
    fn walk(h: &XHost, parent: usize, out: &mut Vec<usize>, depth: usize) {
        let Some(p) = h.nodes.get(parent) else { return };
        for &c in &p.children {
            let Some(n) = h.nodes.get(c) else { continue };
            if n.gone {
                continue;
            }
            out.push(c);
            if n.name.starts_with('#') && depth < MAX_DEPTH {
                walk(h, c, out, depth + 1);
            }
        }
    }
    let mut out = Vec::new();
    walk(h, parent, &mut out, 0);
    out
}

/// `(idx, name)` of the child of `parent` named `name`, first instance.
pub(crate) fn child_named(h: &XHost, parent: usize, name: &str) -> Option<usize> {
    visible_children(h, parent).into_iter().find(|&c| h.nodes.get(c).is_some_and(|n| n.name == name))
}

/// Every live instance named `name` under `parent`, in order.
pub(crate) fn instances(h: &XHost, parent: usize, name: &str) -> Vec<usize> {
    visible_children(h, parent).into_iter().filter(|&c| h.nodes.get(c).is_some_and(|n| n.name == name)).collect()
}

/// Resolve a SOM expression from node `from`: `$`, `$form`, `$record`, `$host`, `xfa.form…`,
/// `parent`, `..name` (descendant search), `name[n]`, `name[*]`, `_name`.
pub(crate) fn resolve_som(h: &XHost, from: usize, expr: &str) -> Vec<usize> {
    let expr = expr.trim();
    if expr.is_empty() {
        return vec![from];
    }
    let root = 0usize;
    let mut cur: Vec<usize> = vec![from];
    let mut rest = expr;
    let starts =
        [("$form", root), ("xfa.form", root), ("$record", root), ("xfa.datasets.data", root), ("xfa.record", root), ("$", from), ("this", from)];
    for (prefix, at) in starts {
        if let Some(r) = rest.strip_prefix(prefix)
            && (r.is_empty() || r.starts_with('.') || r.starts_with('['))
        {
            cur = vec![at];
            rest = r;
            break;
        }
    }
    let mut depth = 0;
    while !rest.is_empty() && depth < MAX_DEPTH {
        depth += 1;
        let descend = rest.starts_with("..");
        rest = rest.trim_start_matches('.');
        let end = rest.find(['.', '[']).unwrap_or(rest.len());
        let (name, after) = rest.split_at(end);
        let name = name.trim();
        let (index, after) = match after.strip_prefix('[') {
            Some(a) => {
                let close = a.find(']').unwrap_or(a.len());
                let idx = a.get(..close).map(str::trim).unwrap_or("");
                (Some(idx.to_string()), a.get(close + 1..).unwrap_or(""))
            }
            None => (None, after),
        };
        rest = after;
        let mut next = Vec::new();
        for &c in &cur {
            match name {
                "" => {}
                "parent" => {
                    if let Some(p) = h.nodes.get(c).and_then(|n| n.parent) {
                        next.push(p);
                    }
                }
                _ => {
                    // `#subform`: the children of that class, whatever their names
                    let class = name.strip_prefix('#').filter(|k| !k.is_empty());
                    let mut found: Vec<usize> = match class {
                        Some(k) => visible_children(h, c).into_iter().filter(|&i| h.nodes.get(i).is_some_and(|n| n.kind.class_name() == k)).collect(),
                        None if descend => descendants_named(h, c, name, 0),
                        None => instances(h, c, name),
                    };
                    // A relative name a node has no child for is looked for among its siblings
                    // and in each enclosing container, nearest first (so a field's script can
                    // say `resolveNode("otherField")`).
                    if found.is_empty() && depth == 1 && !descend {
                        let mut at = h.nodes.get(c).and_then(|n| n.parent);
                        let mut guard = 0;
                        while let Some(p) = at
                            && guard < MAX_DEPTH
                        {
                            found = instances(h, p, name);
                            if !found.is_empty() {
                                break;
                            }
                            at = h.nodes.get(p).and_then(|n| n.parent);
                            guard += 1;
                        }
                    }
                    // `xfa.form` also reaches what its root subform holds (`xfa.form.resolveNode("FirstPage")`)
                    if found.is_empty() && depth == 1 && !descend && class.is_none() && h.nodes.get(c).is_some_and(|n| n.kind == XfaKind::Form) {
                        found = h.nodes.get(c).and_then(|n| n.children.first().copied()).map(|r| instances(h, r, name)).unwrap_or_default();
                    }
                    match index.as_deref() {
                        Some("*") => next.extend(found),
                        Some(i) => {
                            if let Some(k) = i.parse::<usize>().ok().and_then(|i| found.get(i)) {
                                next.push(*k);
                            }
                        }
                        None => next.extend(found.first()),
                    }
                }
            }
        }
        // Each node once: `row[*].parent.row[*]` would otherwise multiply the list by the
        // number of rows at every step.
        let mut seen = std::collections::HashSet::with_capacity(next.len());
        next.retain(|i| seen.insert(*i));
        cur = next;
        if cur.is_empty() {
            break;
        }
    }
    cur
}

/// The live descendants of node `from` named `name` (`from..name` in a SOM path), in document
/// order, to [`MAX_DEPTH`].
pub(crate) fn descendants_named(h: &XHost, from: usize, name: &str, depth: usize) -> Vec<usize> {
    let mut out = Vec::new();
    if depth > MAX_DEPTH {
        return out;
    }
    let Some(n) = h.nodes.get(from) else { return out };
    for &c in &n.children {
        if let Some(k) = h.nodes.get(c)
            && !k.gone
        {
            if k.name == name {
                out.push(c);
            } else {
                out.extend(descendants_named(h, c, name, depth + 1));
            }
        }
    }
    out
}

/// A property read on a form node's Proxy: its value (and `formattedValue` as its picture shows
/// it), attributes, relatives, methods, instance managers and children by name.
fn node_get(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let idx = node_index(&arg(args, 0), ctx)?;
    let key = arg(args, 1);
    if key.as_symbol().is_some() {
        return Ok(JsValue::undefined());
    }
    let key = text(&key, ctx)?;
    let snapshot = {
        let h = host(ctx)?.borrow();
        let Some(n) = h.nodes.get(idx) else { return Ok(JsValue::undefined()) };
        (n.name.clone(), n.som.clone(), n.kind, n.value.clone(), n.presence.clone(), n.access.clone(), n.index, n.parent, n.children.clone())
    };
    let (name, som, kind, value, presence, access, index, parent, children) = snapshot;
    // A script object the subform declares comes first, as a named child does (`form1.value` when it declares `value`).
    let declared = {
        let h = host(ctx)?.borrow();
        let k = format!("{som}|{key}");
        key != IDX && h.codes.iter().any(|c| c.0 == k)
    };
    if declared {
        return script_object_named(ctx, idx, &key);
    }
    Ok(match key.as_str() {
        IDX => JsValue::from(idx as f64),
        // where the object was laid out, as a measurement
        "x" | "y" | "w" | "h" if node_box(ctx, &som).is_some() => {
            let b = node_box(ctx, &som).unwrap_or_default();
            let points = match key.as_str() {
                "x" => b.x,
                "y" => b.y,
                "w" => b.w,
                _ => b.h,
            };
            s(&format!("{}in", (points / 72.0 * 10_000.0).round() / 10_000.0))
        }
        // On a draw `value` is the <value> element (`Rectangle3.value.rectangle.fill…`), not the text
        "value" if kind == XfaKind::Draw => sink(ctx)?,
        "rawValue" | "value" => host(ctx)?.borrow().nodes.get(idx).map_or(JsValue::null(), value_js),
        "formattedValue" => s(&host(ctx)?.borrow().nodes.get(idx).and_then(|n| n.formatted.clone()).unwrap_or(value)),
        "name" => s(&name),
        "somExpression" => s(&som),
        "className" => s(kind.class_name()),
        "index" | "instanceIndex" => JsValue::from(index as f64),
        "presence" => s(&presence),
        "access" => s(&access),
        "isNull" => JsValue::from(value.is_empty()),
        "mandatory" => s("disabled"),
        "parent" => match parent {
            Some(p) => node_object(ctx, p)?,
            None => JsValue::null(),
        },
        "nodes" => {
            let live: Vec<usize> = {
                let h = host(ctx)?.borrow();
                children.iter().copied().filter(|&c| h.nodes.get(c).is_some_and(|n| !n.gone)).collect()
            };
            list_object(ctx, &live)?
        }
        "all" => {
            let live = match parent {
                Some(p) => instances(&host(ctx)?.borrow(), p, &name),
                None => vec![idx],
            };
            list_object(ctx, &live)?
        }
        "instanceManager" => match parent {
            Some(p) => instance_manager(ctx, p, &name)?,
            None => JsValue::undefined(),
        },
        "resolveNode" => resolver(ctx, idx, false)?,
        "resolveNodes" => resolver(ctx, idx, true)?,
        "recalculate" => function(ctx, NativeFunction::from_fn_ptr(recalculate)).into(),
        "remerge" => function(ctx, NativeFunction::from_fn_ptr(relayout)).into(),
        "getAttribute"
        | "setAttribute"
        | "execEvent"
        | "execInitialize"
        | "execCalculate"
        | "execValidate"
        | "getElement"
        | "setElement"
        | "isPropertySpecified"
        | "loadXML"
        | "saveXML"
        | "clearItems"
        | "addItem"
        | "deleteItem"
        | "setItemState"
        | "clone" => function(ctx, NativeFunction::from_fn_ptr(noop)).into(),
        "createNode" => function(ctx, NativeFunction::from_fn_ptr(sink_apply)).into(),
        "getItemState" => function(ctx, NativeFunction::from_fn_ptr(false_value)).into(),
        "getDisplayItem" | "getSaveItem" => function(ctx, NativeFunction::from_fn_ptr(empty_text)).into(),
        "getDeltas" => function(ctx, NativeFunction::from_fn_ptr(empty_list)).into(),
        "toString" | "valueOf" | "toJSON" => function(ctx, NativeFunction::from_fn_ptr(node_to_string)).into(),
        "length" => JsValue::undefined(),
        "border" | "ui" | "font" | "caption" | "fillColor" | "borderColor" | "fontColor" | "margin" | "para" | "assist" | "validate" | "format"
        | "items" | "bind" | "keep" | "h" | "w" | "x" | "y" | "minH" | "maxH" | "minW" | "maxW" | "layout" | "model" | "oneOfChild" | "locale"
        | "relevant" | "colSpan" | "dataNode" | "defaultValue" | "editValue" | "selectedIndex" | "event" | "calculate" | "traversal" | "extras"
        | "desc" => {
            // a script object the subform declares comes before a member the host only stands in for (an object called `layout`)
            let declared = {
                let h = host(ctx)?.borrow();
                let key = h.nodes.get(idx).map(|n| format!("{}|{key}", n.som));
                key.is_some_and(|k| h.codes.iter().any(|c| c.0 == k))
            };
            if declared { script_object_named(ctx, idx, &key)? } else { sink(ctx)? }
        }
        other => {
            if let Some(child) = other.strip_prefix('_') {
                let exists = child_named(&host(ctx)?.borrow(), idx, child).is_some();
                return if exists { instance_manager(ctx, idx, child) } else { Ok(JsValue::undefined()) };
            }
            let found = child_named(&host(ctx)?.borrow(), idx, other);
            match found {
                Some(c) => node_object(ctx, c)?,
                // a script object or variable of the form is a child of its subform too (`xfa.form.nodes.item(0).LOV`)
                None => match document_property(ctx, other)? {
                    Some(v) => v,
                    None => script_object_named(ctx, idx, other)?,
                },
            }
        }
    })
}

/// `event.target` is the Acrobat document when an event is the form's own (`myDoc.resetForm([...])`, `myDoc.URL`): what a
/// form's scripts ask of it, for a name no child of the node has.
fn document_property(ctx: &mut Context, name: &str) -> JsResult<Option<JsValue>> {
    Ok(Some(match name {
        "resetForm" => function(ctx, NativeFunction::from_fn_ptr(reset_data)).into(),
        "URL" | "path" | "documentFileName" => s(&host(ctx)?.borrow().doc.file_name),
        "numPages" => JsValue::from(host(ctx)?.borrow().doc.page_count as f64),
        "getField" | "info" => function(ctx, NativeFunction::from_fn_ptr(sink_apply)).into(),
        "mailDoc" | "submitForm" | "saveAs" | "closeDoc" | "importDataObject" => function(ctx, NativeFunction::from_fn_ptr(noop)).into(),
        "dirty" => JsValue::from(false),
        _ => return Ok(None),
    }))
}

/// The global object named `name`, when it is one a form defined (not what the host or JavaScript provides), or the script
/// object of that name the subform at node `idx` declares when its name is a built-in's (`form1.Number`).
fn script_object_named(ctx: &mut Context, idx: usize, name: &str) -> JsResult<JsValue> {
    if !is_identifier(name) || ["xfa", "app", "console", "event", "Math", "JSON", "Intl", "Reflect", "globalThis", "window"].contains(&name) {
        return Ok(JsValue::undefined());
    }
    // the code object the subform at `idx` declares: its global when it has one, else built for the script (an outer object an
    // inner one hides, or one named like a built-in)
    let declared = {
        let h = host(ctx)?.borrow();
        let Some(som) = h.nodes.get(idx).map(|n| n.som.to_string()) else { return Ok(JsValue::undefined()) };
        let key = format!("{som}|{name}");
        h.codes.iter().find(|c| c.0 == key).map(|c| (key, c.3))
    };
    match declared {
        Some((_, true)) => ctx.global_object().get(JsString::from(name), ctx),
        Some((key, false)) => super::objects::build_detached(ctx, &key, name),
        None => {
            let v = ctx.global_object().get(JsString::from(name), ctx)?;
            Ok(if v.as_object().is_some_and(|o| !o.is_callable()) { v } else { JsValue::undefined() })
        }
    }
}

/// `resolveNode("form1.calcs")` or `resolveNode("calcs")` naming a script object rather than a node: the object, or `null`.
fn object_by_som(ctx: &mut Context, from: usize, expr: &str) -> JsResult<JsValue> {
    let (parent, name) = match expr.rsplit_once('.') {
        Some((p, n)) => (Some(p), n),
        None => (None, expr),
    };
    let name = name.split('[').next().unwrap_or(name);
    let at = match parent {
        Some(p) => resolve_som(&host(ctx)?.borrow(), from, p).first().copied(),
        None => Some(from),
    };
    let found = match at {
        Some(i) => script_object_named(ctx, i, name)?,
        None => JsValue::undefined(),
    };
    Ok(if found.is_undefined() { JsValue::null() } else { found })
}

fn node_set(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let idx = node_index(&arg(args, 0), ctx)?;
    let key = arg(args, 1);
    if key.as_symbol().is_some() {
        return Ok(JsValue::from(true));
    }
    let key = text(&key, ctx)?;
    let value = arg(args, 2);
    let value = if value.is_null() || value.is_undefined() { String::new() } else { text(&value, ctx)? };
    let mut h = host(ctx)?.borrow_mut();
    let Some(n) = h.nodes.get(idx) else { return Ok(JsValue::from(true)) };
    let som = n.som.clone();
    match key.as_str() {
        "rawValue" | "value" | "formattedValue" => {
            // Not a field: the property doesn't exist, as in Acrobat, and nothing is recorded.
            h.set_value(idx, value);
        }
        "presence" => {
            let p = match value.as_str() {
                "hidden" | "invisible" | "inactive" | "visible" => value.clone(),
                _ => return Ok(JsValue::from(true)),
            };
            h.set_state(idx, XfaEffect::SetPresence { som, presence: p });
        }
        "access" => {
            let a = match value.as_str() {
                "open" | "readOnly" | "protected" | "nonInteractive" => value.clone(),
                _ => return Ok(JsValue::from(true)),
            };
            h.set_state(idx, XfaEffect::SetAccess { som, access: a });
        }
        // a size or position the layout then takes
        "x" | "y" | "w" | "h" | "minH" | "maxH" | "minW" | "maxW" if is_measurement(&value) => {
            h.set_state(idx, XfaEffect::SetGeometry { som, prop: key.clone(), value: value.trim().to_string() });
        }
        _ => {}
    }
    Ok(JsValue::from(true))
}

/// Whether `v` reads as a measurement (`2in`, `12.5mm`, `-3pt`, `10`): a number and, perhaps, a unit.
fn is_measurement(v: &str) -> bool {
    let v = v.trim();
    let split = v.find(|c: char| c.is_ascii_alphabetic() || c == '%').unwrap_or(v.len());
    let (num, unit) = v.split_at(split);
    num.trim().parse::<f64>().is_ok_and(f64::is_finite) && matches!(unit.trim(), "" | "pt" | "mm" | "cm" | "in" | "mp")
}

fn node_to_string(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let idx = node_index(this, ctx)?;
    Ok(s(&host(ctx)?.borrow().nodes.get(idx).map(|n| n.value.clone()).unwrap_or_default()))
}

pub(super) fn noop(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

/// `resolveNode(som)` / `resolveNodes(som)` bound to node `from`.
pub(super) fn resolver(ctx: &mut Context, from: usize, many: bool) -> JsResult<JsValue> {
    let key = ObjectInitializer::new(ctx)
        .property(js_string!("from"), JsValue::from(from as f64), Attribute::READONLY)
        .property(js_string!("many"), JsValue::from(many), Attribute::READONLY)
        .build();
    fn resolve(_: &JsValue, args: &[JsValue], key: &JsObject, ctx: &mut Context) -> JsResult<JsValue> {
        let from = key.get(js_string!("from"), ctx)?.to_number(ctx)? as usize;
        let many = key.get(js_string!("many"), ctx)?.to_boolean();
        let expr = text(&arg(args, 0), ctx)?;
        let found = resolve_som(&host(ctx)?.borrow(), from, &expr);
        if many {
            return list_object(ctx, &found);
        }
        match found.first() {
            Some(i) => node_object(ctx, *i),
            // a part of the template the host does not model (`field.caption.value.#text`) stands in as an object that takes anything
            None if template_part_of_known_node(&host(ctx)?.borrow(), from, &expr) => sink(ctx),
            None => object_by_som(ctx, from, &expr),
        }
    }
    Ok(function(ctx, NativeFunction::from_copy_closure_with_captures(resolve, key)).into())
}

/// Whether `expr` names a part of the template (its caption, font, border…) below a node the form does have.
fn template_part_of_known_node(h: &XHost, from: usize, expr: &str) -> bool {
    const PARTS: &[&str] = &[
        "caption", "ui", "font", "border", "assist", "para", "margin", "items", "validate", "format", "bind", "keep", "extras", "desc", "edge",
        "corner", "fill",
    ];
    let segments: Vec<&str> = expr.split('.').collect();
    let Some(k) = segments.iter().skip(1).position(|seg| {
        let name = seg.split('[').next().unwrap_or(seg);
        name.starts_with('#') || PARTS.contains(&name)
    }) else {
        return false;
    };
    !resolve_som(h, from, &segments[..=k].join(".")).is_empty()
}
