//! Script objects: the named JavaScripts a form keeps under a subform's `<variables>` (XFA 3.3 chapter 10). The prelude
//! defines each one a script reaches as a global whose getter calls [`build`] the first time it is read: the object's code
//! is compiled then, once, and the global becomes the object, kept with the engine for the session's later scripts.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use boa_engine::interner::Interner;
use boa_engine::parser::Parser;
use boa_engine::property::PropertyDescriptor;
use boa_engine::{Context, JsNativeError, JsResult, JsString, JsValue, Source};

use super::host::host;
use super::model::XfaKind;
use super::nodes::node_object;
use crate::{arg, text};

/// The most script object code one script may have built: each object is at most 2 MiB, and a script reaching many large
/// ones is stopped here rather than compiling them all.
pub(crate) const MAX_BUILT_BYTES: usize = 8 << 20;

thread_local! {
    /// The members read from each object's code, by a hash of the code: a form's scripts reach the same objects again and again.
    static MEMBERS: RefCell<HashMap<u64, Result<Arc<str>, String>>> = RefCell::new(HashMap::new());
}

/// `__xfa_build(key, name)`: compiles code object `key` and runs its code once, `this` its subform, then makes the global
/// `name` the object it comes to: every function and variable its code declares at its top level.
pub(super) fn build(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let key = text(&arg(args, 0), ctx)?;
    let name = text(&arg(args, 1), ctx)?;
    let value = build_value(ctx, &key, &name)?;
    if value.is_undefined() {
        return Ok(value);
    }
    let desc = PropertyDescriptor::builder().value(value.clone()).writable(true).configurable(true).enumerable(false);
    ctx.global_object().define_property_or_throw(JsString::from(name.as_str()), desc, ctx)?;
    Ok(value)
}

/// Code object `key` built for a script, its global left as it is: one named like a built-in (`form1.Number`).
pub(super) fn build_detached(ctx: &mut Context, key: &str, name: &str) -> JsResult<JsValue> {
    build_value(ctx, key, name)
}

fn build_value(ctx: &mut Context, key: &str, name: &str) -> JsResult<JsValue> {
    let key = key.to_string();
    let (code, this) = {
        let mut h = host(ctx)?.borrow_mut();
        // an object reached again while it is being built (two that use each other at their top level) is not there yet
        if h.building.contains(&key) {
            return Ok(JsValue::undefined());
        }
        let Some((_, som, code)) = h.codes.iter().find(|(k, _, _)| *k == key).cloned() else { return Ok(JsValue::undefined()) };
        h.built_bytes = h.built_bytes.saturating_add(code.len());
        if h.built_bytes > MAX_BUILT_BYTES {
            let why = format!("the script object {name} was not loaded: the script's objects pass {} MiB", MAX_BUILT_BYTES >> 20);
            return Err(JsNativeError::range().with_message(why).into());
        }
        let this = som.as_deref().and_then(|som| h.nodes.iter().position(|n| &*n.som == som && n.kind != XfaKind::Form));
        h.building.push(key.clone());
        (code, this)
    };
    let built = compile(ctx, &code, this);
    if let Ok(mut h) = host(ctx).map(RefCell::borrow_mut) {
        h.building.retain(|k| *k != key);
    }
    built.map_err(|why| JsNativeError::error().with_message(format!("the script object {name} did not load: {why}")).into())
}

fn compile(ctx: &mut Context, code: &str, this: Option<usize>) -> Result<JsValue, String> {
    if let Some(why) = crate::refuse_nesting(code) {
        return Err(why);
    }
    let members = members_of(code)?;
    let wrapped = format!("(function () {{\n{code}\n;return {{ {members} }};\n}})");
    let f = ctx.eval(Source::from_bytes(wrapped.as_bytes())).map_err(|e| e.to_string())?;
    let this = match this {
        Some(i) => node_object(ctx, i).map_err(|e| e.to_string())?,
        None => JsValue::undefined(),
    };
    match f.as_callable() {
        Some(f) => f.call(&this, &[], ctx).map_err(|e| e.to_string()),
        None => Ok(JsValue::undefined()),
    }
}

/// `name: name, …` for every function, variable and lexical name `text` declares at its top level (`var`s in its blocks
/// too), read by the parser so regular expressions, templates and strings are read as they run.
fn members_of(text: &str) -> Result<Arc<str>, String> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    let key = hasher.finish();
    if let Some(known) = MEMBERS.with(|m| m.borrow().get(&key).cloned()) {
        return known;
    }
    let read = read_members(text);
    MEMBERS.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() >= 256 {
            m.clear();
        }
        m.insert(key, read.clone());
    });
    read
}

fn read_members(text: &str) -> Result<Arc<str>, String> {
    use boa_engine::ast::StatementListItem;
    use boa_engine::ast::operations::{bound_names, var_declared_names};
    let mut interner = Interner::default();
    let body = Parser::new(Source::from_bytes(text.as_bytes())).parse_function_body(&mut interner, false, false).map_err(|e| e.to_string())?;
    let mut names: Vec<String> = Vec::new();
    for item in body.statements() {
        let syms: Vec<_> = match item {
            StatementListItem::Statement(s) => var_declared_names(&**s).into_iter().collect(),
            StatementListItem::Declaration(d) => bound_names(&**d),
        };
        for sym in syms {
            let name = interner.resolve_expect(sym).to_string();
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names.sort();
    Ok(names.iter().map(|n| format!("{n}: {n}")).collect::<Vec<_>>().join(", ").into())
}
