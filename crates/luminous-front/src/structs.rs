//! `aot type`: object types with a fixed layout.
//!
//! ```ts
//! aot type Point = { x: int; y: int; kind: "a" } | { x: int; y: int; kind: "b"; color: string };
//! const p: Point = { x: 1, y: 2, kind: "a" };
//! ```
//!
//! An `aot type` is a union of members (object types, `&` merging them),
//! whose fields are `int`, `number`, `boolean`, `string`, string literals,
//! other `aot type`s, object types written in place (each an `aot type` of
//! its own, named after where it is: `C.nested`) or arrays of objects of
//! these (`T[]`, `Array<T>`). An `aot type` can also name such an array
//! (`aot type D = C[]`). An object `aot type` becomes one layout (`ir::Layout`) with every field
//! of every member and a mask of the fields an object has: so `{ a } | { b }`
//! holds `{ a }`, `{ b }` and `{ a, b }` alike. Objects of an `aot type` are
//! made by object literals where the type is expected (`literal_sites`):
//! a literal must have the fields of some member (checked here), and no
//! others. At runtime they are objects of their own class, which keeps them
//! to their type: fields cannot be added or deleted, and a field only takes
//! values of its type (else TypeError).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use oxc::ast::ast::*;
use oxc::ast_visit::{Visit, walk};
use oxc::diagnostics::OxcDiagnostic;
use oxc::semantic::{Scoping, SymbolId};
use oxc::span::{GetSpan, Span};
use oxc::syntax::scope::ScopeFlags;

use crate::ir::{FieldType, Layout, LayoutField, LayoutMember, Storage, Ty};

/// Fields of an `aot type` (bits of a `u32` mask).
const MAX_FIELDS: usize = 32;

#[derive(Debug, Default)]
pub struct Structs {
    pub layouts: Vec<Layout>,
    /// Object literals that make an object of a layout: by span start, the
    /// span end and the layout.
    pub sites: HashMap<u32, (u32, u32)>,
    /// Array literals and `new Array(...)` that make arrays of objects of
    /// a layout (`T[]` expected): by span start, the span end and layout.
    pub array_sites: HashMap<u32, (u32, u32)>,
    /// Bindings declared `T[]` (`let` / `const`) that only ever hold such
    /// arrays: what they hold is always one (or they are in their TDZ).
    pub sure_arrays: HashMap<SymbolId, u32>,
    /// `JSON.parse(text)` where an object of an `aot type` (or an array of
    /// them) is expected: by span start, the span end and what (`desc`:
    /// layout * 2, + 1 for an array).
    pub parse_sites: HashMap<u32, (u32, i32)>,
    /// `await r.json()` there: by the `await`'s span start.
    pub json_sites: HashMap<u32, (u32, i32)>,
}

thread_local! {
    /// `aot type` names, for `struct_named` (type annotations are read
    /// without context).
    static NAMES: RefCell<HashMap<String, u32>> = RefCell::new(HashMap::new());
    /// The `aot type`s that are arrays (`aot type D = C[]`): their
    /// elements' layouts.
    static ARRAY_NAMES: RefCell<HashMap<String, u32>> = RefCell::new(HashMap::new());
    /// The `aot type`s that also hold `null` or `undefined`.
    static NULLABLE: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

/// Whether the `aot type` of this name also holds `null` or `undefined`.
pub fn struct_nullable(name: &str) -> bool {
    NULLABLE.with(|n| n.borrow().contains(name))
}

/// The layout of the `aot type` of this name.
pub fn struct_named(name: &str) -> Option<u32> {
    NAMES.with(|n| n.borrow().get(name).copied())
}

/// The layout of the elements of the array `aot type` of this name.
pub fn array_named(name: &str) -> Option<u32> {
    ARRAY_NAMES.with(|n| n.borrow().get(name).copied())
}

/// The element type of an array type (`T[]`, `Array<T>`, `ReadonlyArray<T>`).
fn array_element<'t, 'a>(t: &'t TSType<'a>) -> Option<&'t TSType<'a>> {
    match t {
        TSType::TSParenthesizedType(p) => array_element(&p.type_annotation),
        TSType::TSArrayType(a) => Some(&a.element_type),
        TSType::TSTypeReference(r) => match (&r.type_name, &r.type_arguments) {
            (TSTypeName::IdentifierReference(id), Some(args))
                if matches!(id.name.as_str(), "Array" | "ReadonlyArray") && args.params.len() == 1 =>
            {
                Some(&args.params[0])
            }
            _ => None,
        },
        _ => None,
    }
}

/// Whether `t` is object types written in place, combined with `|` and `&`
/// (and `null` / `undefined`).
fn inline_object(t: &TSType) -> bool {
    match t {
        TSType::TSTypeLiteral(_) => true,
        TSType::TSParenthesizedType(p) => inline_object(&p.type_annotation),
        TSType::TSUnionType(u) => {
            u.types.iter().all(|t| inline_object(t) || nullish(t).is_some()) && u.types.iter().any(inline_object)
        }
        TSType::TSIntersectionType(i) => i.types.iter().all(inline_object),
        _ => false,
    }
}

/// `null` (true) or `undefined` (false).
fn nullish(t: &TSType) -> Option<bool> {
    match t {
        TSType::TSNullKeyword(_) => Some(true),
        TSType::TSUndefinedKeyword(_) => Some(false),
        TSType::TSParenthesizedType(p) => nullish(&p.type_annotation),
        _ => None,
    }
}

/// A union with `null` or `undefined` in it: its other types, and whether
/// it has `null`, `undefined`.
fn nullish_parts<'t, 'a>(t: &'t TSType<'a>) -> Option<(Vec<&'t TSType<'a>>, bool, bool)> {
    fn leaves<'t, 'a>(t: &'t TSType<'a>, out: &mut Vec<&'t TSType<'a>>) {
        match t {
            TSType::TSParenthesizedType(p) => leaves(&p.type_annotation, out),
            TSType::TSUnionType(u) => u.types.iter().for_each(|t| leaves(t, out)),
            t => out.push(t),
        }
    }
    let mut all = Vec::new();
    leaves(t, &mut all);
    let (null, undef) = (all.iter().any(|t| nullish(t) == Some(true)), all.iter().any(|t| nullish(t) == Some(false)));
    (null || undef).then(|| (all.into_iter().filter(|t| nullish(t).is_none()).collect(), null, undef))
}

/// Resolve the `aot type`s (the type aliases starting at `marked`) and find
/// the object literals that make their objects.
pub fn analyze(program: &Program, scoping: &Scoping, marked: &HashSet<u32>) -> (Structs, Vec<OxcDiagnostic>) {
    let mut errors = Vec::new();
    let mut decls: Vec<&TSTypeAliasDeclaration> = Vec::new();
    // Plain type aliases (`type Y = A[]`), which may name `aot type`s.
    let mut plain: HashMap<String, &TSType> = HashMap::new();
    for s in &program.body {
        let d = match s {
            Statement::TSTypeAliasDeclaration(d) => d,
            Statement::ExportDeclaration(e) => match &e.declaration {
                Declaration::TSTypeAliasDeclaration(d) => d,
                _ => continue,
            },
            _ => continue,
        };
        if marked.contains(&d.span.start) {
            decls.push(d);
        } else if d.type_parameters.is_none() {
            plain.insert(d.id.name.to_string(), &d.type_annotation);
        }
    }
    for &at in marked {
        if !decls.iter().any(|d| d.span.start == at) {
            errors.push(
                OxcDiagnostic::error("an `aot type` must be declared at the top level of the module")
                    .with_label(Span::sized(at, 4)),
            );
        }
    }
    decls.sort_by_key(|d| d.span.start);
    // Object `aot type`s are layouts 0.., in order; then the object types
    // written in place, as they are found.
    let (arrays, decls): (Vec<&TSTypeAliasDeclaration>, Vec<&TSTypeAliasDeclaration>) =
        decls.into_iter().partition(|d| array_element(&d.type_annotation).is_some());
    let ids: HashMap<String, u32> = decls.iter().enumerate().map(|(i, d)| (d.id.name.to_string(), i as u32)).collect();
    NAMES.with(|n| *n.borrow_mut() = ids.clone());
    let array_decls: HashMap<String, &TSType> =
        arrays.iter().map(|d| (d.id.name.to_string(), array_element(&d.type_annotation).unwrap())).collect();

    let decl_nullish: Vec<(bool, bool)> =
        decls.iter().map(|d| nullish_parts(&d.type_annotation).map_or((false, false), |(_, n, u)| (n, u))).collect();
    NULLABLE.with(|n| {
        *n.borrow_mut() = decls
            .iter()
            .zip(&decl_nullish)
            .filter(|(_, (n, u))| *n || *u)
            .map(|(d, _)| d.id.name.to_string())
            .collect()
    });
    let mut r = Resolver {
        decls: &decls,
        ids: &ids,
        decl_nullish: &decl_nullish,
        arrays: &array_decls,
        stack: Vec::new(),
        path: Vec::new(),
        anon: Vec::new(),
        anon_at: HashMap::new(),
        errors: Vec::new(),
    };
    let mut layouts = Vec::new();
    for (i, d) in decls.iter().enumerate() {
        if let Some(p) = &d.type_parameters {
            r.err::<()>(p.span, "an `aot type` cannot have type parameters");
        }
        r.stack.push(i as u32);
        r.path = vec![d.id.name.to_string()];
        let members = r.members(&d.type_annotation);
        if members.as_ref().is_some_and(|m| m.is_empty()) {
            r.err::<()>(d.span, "an `aot type` needs an object type");
        }
        let members = members.unwrap_or_default();
        r.stack.pop();
        layouts.push(layout(d.id.name.as_str(), &members, d.span, &mut r.errors));
    }
    let mut array_ids = HashMap::new();
    for d in &arrays {
        if let Some(p) = &d.type_parameters {
            r.err::<()>(p.span, "an `aot type` cannot have type parameters");
        }
        r.path = vec![d.id.name.to_string()];
        if let Some(k) = r.element(array_decls[d.id.name.as_str()]) {
            array_ids.insert(d.id.name.to_string(), k);
        }
    }
    // Plain aliases of `aot type`s and of arrays of them are the same types
    // (for bindings, parameters and return types).
    let (mut site_ids, mut site_arrays) = (ids.clone(), array_ids.clone());
    for name in plain.keys() {
        match alias_site(name, &plain, &ids, &array_ids, 0) {
            Some(SiteTy::Struct(k)) if !ids.contains_key(name) => {
                site_ids.insert(name.clone(), k);
            }
            Some(SiteTy::Array(k)) if !array_ids.contains_key(name) => {
                site_arrays.insert(name.clone(), k);
            }
            _ => {}
        }
    }
    NAMES.with(|n| *n.borrow_mut() = site_ids.clone());
    ARRAY_NAMES.with(|n| *n.borrow_mut() = site_arrays.clone());
    for (name, members, span) in std::mem::take(&mut r.anon) {
        layouts.push(layout(&name, &members, span, &mut r.errors));
    }
    errors.append(&mut r.errors);
    if !errors.is_empty() {
        return (Structs::default(), errors);
    }

    let mut decl_types = DeclTypes {
        scoping,
        ids: &site_ids,
        arrays: &site_arrays,
        vars: HashMap::new(),
        fns: HashMap::new(),
        lexical: HashSet::new(),
        inits: HashSet::new(),
    };
    decl_types.visit_program(program);
    let mut sites = Sites {
        scoping,
        types: &decl_types,
        layouts: &layouts,
        returns: Vec::new(),
        sites: HashMap::new(),
        array_sites: HashMap::new(),
        parse_sites: HashMap::new(),
        json_sites: HashMap::new(),
        writes: HashMap::new(),
        errors: Vec::new(),
    };
    sites.visit_program(program);
    errors.append(&mut sites.errors);
    let sure_arrays = sure_arrays(scoping, &decl_types, &sites.writes);
    let (struct_sites, array_sites) = (sites.sites, sites.array_sites);
    let (parse_sites, json_sites) = (sites.parse_sites, sites.json_sites);
    (Structs { layouts, sites: struct_sites, array_sites, sure_arrays, parse_sites, json_sites }, errors)
}

/// What plain type alias `name` is, if an `aot type` (or an alias of one)
/// or an array of objects of one.
fn alias_site(
    name: &str,
    plain: &HashMap<String, &TSType>,
    ids: &HashMap<String, u32>,
    arrays: &HashMap<String, u32>,
    depth: u32,
) -> Option<SiteTy> {
    fn of(
        t: &TSType,
        plain: &HashMap<String, &TSType>,
        ids: &HashMap<String, u32>,
        arrays: &HashMap<String, u32>,
        depth: u32,
    ) -> Option<SiteTy> {
        if let Some(el) = array_element(t) {
            return match of(el, plain, ids, arrays, depth)? {
                SiteTy::Struct(k) => Some(SiteTy::Array(k)),
                SiteTy::Array(_) => None,
            };
        }
        match t {
            TSType::TSParenthesizedType(p) => of(&p.type_annotation, plain, ids, arrays, depth),
            TSType::TSTypeReference(r) if r.type_arguments.is_none() => {
                let TSTypeName::IdentifierReference(id) = &r.type_name else { return None };
                let name = id.name.as_str();
                match (ids.get(name), arrays.get(name)) {
                    (Some(&k), _) => Some(SiteTy::Struct(k)),
                    (None, Some(&k)) => Some(SiteTy::Array(k)),
                    (None, None) => alias_site(name, plain, ids, arrays, depth + 1),
                }
            }
            _ => None,
        }
    }
    if depth > 16 {
        return None;
    }
    of(plain.get(name)?, plain, ids, arrays, depth)
}

/// What a write to a binding stores, for `sure_arrays`.
#[derive(Clone, Copy)]
enum Write {
    /// An array site of this layout.
    Array(u32),
    /// The value of this binding.
    Binding(SymbolId),
    Other,
}

/// The bindings declared `T[]` all of whose writes store arrays of T's
/// layout (array sites, or bindings that are such themselves).
fn sure_arrays(scoping: &Scoping, types: &DeclTypes, writes: &HashMap<SymbolId, Vec<Write>>) -> HashMap<SymbolId, u32> {
    let mut sure: HashMap<SymbolId, u32> = types
        .vars
        .iter()
        .filter_map(|(&s, t)| match t {
            SiteTy::Array(k) if types.lexical.contains(&s) => Some((s, *k)),
            _ => None,
        })
        .collect();
    loop {
        let before = sure.len();
        let current = sure.clone();
        sure.retain(|s, k| {
            let ws = writes.get(s).map_or(&[][..], |w| &w[..]);
            // Every write is one recorded here (no `x++`, destructuring, ...).
            let all = scoping.get_resolved_references(*s).filter(|r| r.is_write()).count();
            let recorded = ws.len() - types.inits.contains(s) as usize;
            all == recorded
                && types.inits.contains(s)
                && ws.iter().all(|w| match w {
                    Write::Array(j) => j == k,
                    Write::Binding(b) => current.get(b) == Some(k),
                    Write::Other => false,
                })
        });
        if sure.len() == before {
            return sure;
        }
    }
}

/// A member being resolved: its fields.
/// (name, type, optional)
type RawMember = Vec<(String, FieldType, bool)>;

struct Resolver<'d, 'a> {
    decls: &'d [&'d TSTypeAliasDeclaration<'a>],
    ids: &'d HashMap<String, u32>,
    /// Per object `aot type`: whether it also holds `null`, `undefined`.
    decl_nullish: &'d [(bool, bool)],
    /// The array `aot type`s: their element types.
    arrays: &'d HashMap<String, &'d TSType<'a>>,
    /// The `aot type`s being resolved (references to them are cycles).
    stack: Vec<u32>,
    /// Where the type being resolved is written: its `aot type`, then the
    /// fields in it (which name the object types written in place).
    path: Vec<String>,
    /// The object types written in place (layouts `ids.len()..`): name,
    /// members and span; by span start, their layouts.
    anon: Vec<(String, Vec<RawMember>, Span)>,
    anon_at: HashMap<u32, u32>,
    errors: Vec<OxcDiagnostic>,
}

impl Resolver<'_, '_> {
    fn err<T>(&mut self, span: Span, msg: &str) -> Option<T> {
        self.errors.push(OxcDiagnostic::error(msg.to_string()).with_label(span));
        None
    }

    fn members(&mut self, t: &TSType) -> Option<Vec<RawMember>> {
        match t {
            TSType::TSTypeLiteral(l) => {
                let mut fields: RawMember = Vec::new();
                for sig in &l.members {
                    let TSSignature::TSPropertySignature(p) = sig else {
                        return self.err(sig.span(), "an `aot type` can only have fields");
                    };
                    let name = match &p.key {
                        PropertyKey::StaticIdentifier(id) if !p.computed => id.name.to_string(),
                        PropertyKey::StringLiteral(s) => s.value.to_string(),
                        _ => return self.err(p.key.span(), "the field name must be an identifier or a string"),
                    };
                    let Some(ann) = &p.type_annotation else {
                        return self.err(p.span, "a field of an `aot type` needs a type");
                    };
                    self.path.push(name.clone());
                    let ty = self.field_type(&ann.type_annotation);
                    self.path.pop();
                    let (ty, optional) = ty?;
                    if fields.iter().any(|(n, _, _)| *n == name) {
                        return self.err(p.span, "duplicate field");
                    }
                    fields.push((name, ty, optional || p.optional));
                }
                Some(vec![fields])
            }
            TSType::TSUnionType(u) => {
                // (`null` and `undefined` are the type's, not members.)
                let mut out = Vec::new();
                for t in u.types.iter().filter(|t| nullish(t).is_none()) {
                    out.extend(self.members(t)?);
                }
                Some(out)
            }
            TSType::TSIntersectionType(i) => {
                let mut acc: Vec<RawMember> = vec![Vec::new()];
                for t in &i.types {
                    let parts = self.members(t)?;
                    let mut next = Vec::new();
                    for a in &acc {
                        for b in &parts {
                            let mut m = a.clone();
                            for (name, ty, opt) in b {
                                match m.iter().position(|(n, _, _)| n == name) {
                                    Some(j) if m[j].1 != *ty => {
                                        return self.err(i.span, &format!("`{name}` has two different types"));
                                    }
                                    Some(j) => m[j].2 &= *opt,
                                    None => m.push((name.clone(), ty.clone(), *opt)),
                                }
                            }
                            next.push(m);
                        }
                    }
                    acc = next;
                }
                Some(acc)
            }
            TSType::TSParenthesizedType(p) => self.members(&p.type_annotation),
            TSType::TSTypeReference(r) => {
                let TSTypeName::IdentifierReference(id) = &r.type_name else {
                    return self.err(r.span, "not an `aot type`");
                };
                if self.arrays.contains_key(id.name.as_str()) {
                    return self.err(r.span, &format!("`{}` is an array, not an object type", id.name));
                }
                let Some(&k) = self.ids.get(id.name.as_str()) else {
                    return self.err(
                        r.span,
                        &format!("`{}` is not an `aot type`: use object types, `|`, `&` and `aot type`s", id.name),
                    );
                };
                if r.type_arguments.is_some() {
                    return self.err(r.span, "an `aot type` has no type arguments");
                }
                if self.stack.contains(&k) {
                    return self.err(r.span, "an `aot type` cannot contain itself");
                }
                self.stack.push(k);
                let path = std::mem::replace(&mut self.path, vec![id.name.to_string()]);
                let ms = self.members(&self.decls[k as usize].type_annotation);
                self.path = path;
                self.stack.pop();
                ms
            }
            _ => self.err(t.span(), "an `aot type` is made of object types, `|`, `&` and other `aot type`s"),
        }
    }

    /// A field's type, and whether it may be absent (`T | undefined`, or
    /// an `aot type` that holds `undefined`).
    fn field_type(&mut self, t: &TSType) -> Option<(FieldType, bool)> {
        let literal = |t: &TSType| match t {
            TSType::TSLiteralType(l) => match &l.literal {
                TSLiteral::StringLiteral(s) => Some(s.value.to_string()),
                _ => None,
            },
            _ => None,
        };
        let with = |ty: FieldType, null: bool, undef: bool| {
            let ty = match ty {
                FieldType::Nullable(_) => ty,
                ty if null => FieldType::Nullable(Box::new(ty)),
                ty => ty,
            };
            Some((ty, undef))
        };
        // `T | null`, `T | undefined`.
        if let Some((rest, null, undef)) = nullish_parts(t) {
            let (ty, opt) = match rest.as_slice() {
                [] => return self.unsupported(t),
                [one] => self.field_type(one)?,
                many if many.iter().all(|t| literal(t).is_some()) => {
                    (FieldType::Literals(many.iter().map(|t| literal(t).unwrap()).collect()), false)
                }
                _ if inline_object(t) => (FieldType::Struct(self.anonymous(t, "")?), false),
                _ => return self.unsupported(t),
            };
            return with(ty, null, undef || opt);
        }
        let ty = match t {
            TSType::TSNumberKeyword(_) => FieldType::Number,
            TSType::TSBooleanKeyword(_) => FieldType::Bool,
            TSType::TSStringKeyword(_) => FieldType::String,
            TSType::TSParenthesizedType(p) => return self.field_type(&p.type_annotation),
            TSType::TSLiteralType(_) if literal(t).is_some() => FieldType::Literals(vec![literal(t).unwrap()]),
            TSType::TSUnionType(u) if u.types.iter().all(|t| literal(t).is_some()) => {
                FieldType::Literals(u.types.iter().map(|t| literal(t).unwrap()).collect())
            }
            t if inline_object(t) => FieldType::Struct(self.anonymous(t, "")?),
            t if array_element(t).is_some() => FieldType::Array(self.element(array_element(t).unwrap())?),
            TSType::TSTypeReference(r) if r.type_arguments.is_none() => {
                let TSTypeName::IdentifierReference(id) = &r.type_name else { return self.unsupported(t) };
                match id.name.as_str() {
                    "int" => FieldType::Int,
                    "double" => FieldType::Number,
                    name => match (self.ids.get(name), self.arrays.get(name)) {
                        (Some(&k), _) => {
                            let (null, undef) = self.decl_nullish[k as usize];
                            return with(FieldType::Struct(k), null, undef);
                        }
                        (None, Some(&el)) => {
                            let path = std::mem::replace(&mut self.path, vec![name.to_string()]);
                            let k = self.element(el);
                            self.path = path;
                            FieldType::Array(k?)
                        }
                        (None, None) => return self.unsupported(t),
                    },
                }
            }
            _ => return self.unsupported(t),
        };
        Some((ty, false))
    }

    /// The layout of the elements of an array in an `aot type`: an object
    /// `aot type`, or object types written in place.
    fn element(&mut self, t: &TSType) -> Option<u32> {
        if nullish_parts(t).is_some() {
            return self.err(t.span(), "the elements of an array in an `aot type` cannot be `null` or `undefined`");
        }
        match t {
            TSType::TSParenthesizedType(p) => self.element(&p.type_annotation),
            t if inline_object(t) => self.anonymous(t, "[]"),
            TSType::TSTypeReference(r) if r.type_arguments.is_none() => match &r.type_name {
                TSTypeName::IdentifierReference(id) if self.ids.contains_key(id.name.as_str()) => {
                    let k = self.ids[id.name.as_str()];
                    if self.decl_nullish[k as usize] != (false, false) {
                        return self.err(
                            t.span(),
                            "the elements of an array in an `aot type` cannot be `null` or `undefined`",
                        );
                    }
                    Some(k)
                }
                _ => self.unsupported_element(t),
            },
            _ => self.unsupported_element(t),
        }
    }

    /// The layout of object types written in place (`t`), named after
    /// where they are (`C.nested`, `C.items[]`).
    fn anonymous(&mut self, t: &TSType, suffix: &str) -> Option<u32> {
        if let Some(&k) = self.anon_at.get(&t.span().start) {
            return Some(k);
        }
        let k = (self.ids.len() + self.anon.len()) as u32;
        let name = self.path.join(".") + suffix;
        self.anon_at.insert(t.span().start, k);
        self.anon.push((name.clone(), Vec::new(), t.span()));
        let path = std::mem::replace(&mut self.path, vec![name]);
        let members = self.members(t);
        self.path = path;
        self.anon[k as usize - self.ids.len()].1 = members?;
        Some(k)
    }

    fn unsupported_element<T>(&mut self, t: &TSType) -> Option<T> {
        self.errors.push(
            OxcDiagnostic::error("unsupported element type of an array in an `aot type`")
                .with_label(t.span())
                .with_help("the elements are objects of an `aot type`, or of object types written in place"),
        );
        None
    }

    fn unsupported<T>(&mut self, t: &TSType) -> Option<T> {
        self.errors.push(
            OxcDiagnostic::error("unsupported field type in an `aot type`")
                .with_label(t.span())
                .with_help(
                    "fields are `int`, `number`, `double`, `boolean`, `string`, string literals, `aot type`s, \
                     object types, or arrays of objects (`T[]`)",
                ),
        );
        None
    }
}

/// The layout of an `aot type` with these members.
fn layout(name: &str, members: &[RawMember], span: Span, errors: &mut Vec<OxcDiagnostic>) -> Layout {
    let mut names: Vec<String> = Vec::new();
    for m in members {
        for (n, _, _) in m {
            if !names.contains(n) {
                names.push(n.clone());
            }
        }
    }
    if names.len() > MAX_FIELDS {
        errors
            .push(OxcDiagnostic::error(format!("an `aot type` can have at most {MAX_FIELDS} fields")).with_label(span));
        names.truncate(MAX_FIELDS);
    }
    let members: Vec<LayoutMember> = members
        .iter()
        .map(|m| {
            let (mut mask, mut optional) = (0, 0);
            let mut types = vec![FieldType::Absent; names.len()];
            for (n, t, opt) in m {
                if let Some(i) = names.iter().position(|x| x == n) {
                    mask |= 1 << i;
                    if *opt {
                        optional |= 1 << i;
                    }
                    types[i] = t.clone();
                }
            }
            LayoutMember { mask, optional, types }
        })
        .collect();
    let mut offset = 0u32;
    let fields = names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let ts: Vec<&FieldType> =
                members.iter().map(|m| &m.types[i]).filter(|t| **t != FieldType::Absent).collect();
            let universal = ts.len() == members.len() && members.iter().all(|m| m.optional & (1 << i) == 0);
            let storage = if ts.iter().any(|t| matches!(t, FieldType::Nullable(_))) {
                // (`null` is a JS value.)
                Storage::Value
            } else if ts.iter().all(|t| **t == FieldType::Int) {
                Storage::Int32
            } else if ts.iter().all(|t| matches!(t, FieldType::Int | FieldType::Number)) {
                Storage::F64
            } else if ts.iter().all(|t| **t == FieldType::Bool) {
                Storage::Bool
            } else {
                Storage::Value
            };
            let same = ts.windows(2).all(|w| w[0] == w[1]);
            // An object of one `aot type`, maybe `null`, in every member.
            let one_struct = match ts[0].base() {
                FieldType::Struct(k) if ts.iter().all(|t| *t.base() == FieldType::Struct(*k)) => Some(*k),
                _ => None,
            };
            let ty = match (universal, storage) {
                (true, Storage::Int32) => Ty::I32,
                (true, Storage::F64) => Ty::F64,
                (true, Storage::Bool) => Ty::Bool,
                (true, Storage::Value) => match ts[0] {
                    FieldType::Struct(k) if same => Ty::Struct(*k),
                    FieldType::Array(k) if same => Ty::StructArr(*k),
                    _ => one_struct.map_or(Ty::Any, Ty::StructOrUndef),
                },
                // May be absent: `undefined` then.
                (false, Storage::Int32 | Storage::F64) => Ty::MaybeF64,
                (false, _) => one_struct.map_or(Ty::Any, Ty::StructOrUndef),
            };
            let (size, align) = match storage {
                Storage::Int32 => (4, 4),
                Storage::F64 => (8, 8),
                Storage::Bool => (1, 1),
                Storage::Value => (8, 8),
            };
            offset = offset.div_ceil(align) * align;
            let at = offset;
            offset += size;
            LayoutField { name: n.clone(), storage, offset: at, universal, ty, simple: universal && same }
        })
        .collect();
    Layout { name: name.to_string(), fields, members, size: offset.div_ceil(8) * 8 }
}

/// What a declaration says a binding holds, for `literal_sites`.
#[derive(Clone, Copy)]
enum SiteTy {
    Struct(u32),
    /// An array of objects of an `aot type`.
    Array(u32),
}

/// What annotation `t` says a binding holds (`arrays`: the array `aot
/// type`s' element layouts).
fn site_ty(t: &TSType, ids: &HashMap<String, u32>, arrays: &HashMap<String, u32>) -> Option<SiteTy> {
    let named = |t: &TSType| match t {
        TSType::TSTypeReference(r) if r.type_arguments.is_none() => match &r.type_name {
            TSTypeName::IdentifierReference(id) => ids.get(id.name.as_str()).copied(),
            _ => None,
        },
        _ => None,
    };
    match t {
        TSType::TSParenthesizedType(p) => site_ty(&p.type_annotation, ids, arrays),
        TSType::TSArrayType(a) => named(&a.element_type).map(SiteTy::Array),
        TSType::TSTypeReference(r) => match (&r.type_name, &r.type_arguments) {
            (TSTypeName::IdentifierReference(id), Some(args))
                if matches!(id.name.as_str(), "Array" | "ReadonlyArray") && args.params.len() == 1 =>
            {
                named(&args.params[0]).map(SiteTy::Array)
            }
            (TSTypeName::IdentifierReference(id), None) if arrays.contains_key(id.name.as_str()) => {
                Some(SiteTy::Array(arrays[id.name.as_str()]))
            }
            _ => named(t).map(SiteTy::Struct),
        },
        _ => None,
    }
}

/// The declared types of bindings (variables, parameters) and the
/// parameter and return types of functions.
struct DeclTypes<'s> {
    scoping: &'s Scoping,
    ids: &'s HashMap<String, u32>,
    arrays: &'s HashMap<String, u32>,
    vars: HashMap<SymbolId, SiteTy>,
    fns: HashMap<SymbolId, (Vec<Option<SiteTy>>, Option<SiteTy>)>,
    /// `let` / `const` bindings, and the declared bindings with an initializer.
    lexical: HashSet<SymbolId>,
    inits: HashSet<SymbolId>,
}

impl DeclTypes<'_> {
    fn signature(
        &self,
        params: &FormalParameters,
        ret: Option<&TSTypeAnnotation>,
    ) -> (Vec<Option<SiteTy>>, Option<SiteTy>) {
        let ps = params
            .items
            .iter()
            .map(|p| p.type_annotation.as_ref().and_then(|a| site_ty(&a.type_annotation, self.ids, self.arrays)))
            .collect();
        (ps, ret.and_then(|a| site_ty(&a.type_annotation, self.ids, self.arrays)))
    }
}

impl<'a> Visit<'a> for DeclTypes<'_> {
    fn visit_variable_declarator(&mut self, d: &VariableDeclarator<'a>) {
        if let BindingPattern::BindingIdentifier(id) = &d.id
            && let Some(sym) = id.symbol_id.get()
        {
            if self.scoping.symbol_flags(sym).is_block_scoped() {
                self.lexical.insert(sym);
            }
            if d.init.is_some() {
                self.inits.insert(sym);
            }
            if let Some(t) = d.type_annotation.as_ref().and_then(|a| site_ty(&a.type_annotation, self.ids, self.arrays)) {
                self.vars.insert(sym, t);
            }
            match d.init.as_ref().map(|e| e.without_parentheses()) {
                Some(Expression::ArrowFunctionExpression(f)) => {
                    let s = self.signature(&f.params, f.return_type.as_deref());
                    self.fns.insert(sym, s);
                }
                Some(Expression::FunctionExpression(f)) => {
                    let s = self.signature(&f.params, f.return_type.as_deref());
                    self.fns.insert(sym, s);
                }
                _ => {}
            }
        }
        walk::walk_variable_declarator(self, d);
    }

    fn visit_formal_parameter(&mut self, p: &FormalParameter<'a>) {
        if let BindingPattern::BindingIdentifier(id) = &p.pattern
            && let Some(sym) = id.symbol_id.get()
            && let Some(t) = p.type_annotation.as_ref().and_then(|a| site_ty(&a.type_annotation, self.ids, self.arrays))
        {
            self.vars.insert(sym, t);
        }
        walk::walk_formal_parameter(self, p);
    }

    fn visit_function(&mut self, f: &Function<'a>, flags: ScopeFlags) {
        if let Some(sym) = f.id.as_ref().and_then(|id| id.symbol_id.get()) {
            let s = self.signature(&f.params, f.return_type.as_deref());
            self.fns.insert(sym, s);
        }

        walk::walk_function(self, f, flags);
    }
}

/// Finds the object literals made where an `aot type` is expected: the
/// initializer of a binding declared with it, a value assigned to one, a
/// returned value, an argument for such a parameter of a function called
/// by name, an element of an array of them (in an array literal, stored
/// with `a[i] = ...` or `a.push(...)`), the value of such a field, and the
/// branches of a conditional in these places.
struct Sites<'s> {
    scoping: &'s Scoping,
    types: &'s DeclTypes<'s>,
    layouts: &'s [Layout],
    /// The return types of the enclosing functions.
    returns: Vec<Option<SiteTy>>,
    sites: HashMap<u32, (u32, u32)>,
    array_sites: HashMap<u32, (u32, u32)>,
    parse_sites: HashMap<u32, (u32, i32)>,
    json_sites: HashMap<u32, (u32, i32)>,
    /// Writes of the bindings declared `T[]`.
    writes: HashMap<SymbolId, Vec<Write>>,
    errors: Vec<OxcDiagnostic>,
}

impl Sites<'_> {
    fn symbol(&self, id: &IdentifierReference) -> Option<SymbolId> {
        self.scoping.get_reference(id.reference_id.get()?).symbol_id()
    }

    /// What the declarations say `e` holds: a binding, a field of an
    /// `aot type` that holds one or an array of them, or an element of such
    /// an array.
    fn declared(&self, e: &Expression) -> Option<SiteTy> {
        match e.without_parentheses() {
            Expression::Identifier(id) => self.symbol(id).and_then(|s| self.types.vars.get(&s)).copied(),
            Expression::StaticMemberExpression(m) => match self.declared(&m.object)? {
                SiteTy::Struct(k) => self.field_site(k, m.property.name.as_str()),
                SiteTy::Array(_) => None,
            },
            Expression::ComputedMemberExpression(m) => match self.declared(&m.object)? {
                SiteTy::Array(k) => Some(SiteTy::Struct(k)),
                SiteTy::Struct(_) => None,
            },
            _ => None,
        }
    }

    /// What field `name` of layout `k` holds in every member that has it,
    /// if an object of an `aot type` or an array of them.
    fn field_site(&self, k: u32, name: &str) -> Option<SiteTy> {
        let l = &self.layouts[k as usize];
        let i = l.fields.iter().position(|f| f.name == name)?;
        field_site_at(l, i)
    }

    /// The global `Array` (not shadowed).
    fn is_array_ctor(&self, e: &Expression) -> bool {
        matches!(e.without_parentheses(), Expression::Identifier(id) if id.name == "Array" && self.symbol(id).is_none())
    }

    /// `JSON.parse(text)` (the global `JSON`, one argument).
    fn is_json_parse(&self, c: &CallExpression) -> bool {
        matches!(c.callee.without_parentheses(), Expression::StaticMemberExpression(m)
            if m.property.name == "parse" && !m.optional
                && matches!(m.object.without_parentheses(), Expression::Identifier(id)
                    if id.name == "JSON" && self.symbol(id).is_none()))
            && !c.optional
            && c.arguments.len() == 1
            && c.arguments[0].as_expression().is_some()
    }

    /// `r.json()` (any `r`).
    fn is_json_call(e: &Expression) -> bool {
        matches!(e.without_parentheses(), Expression::CallExpression(c)
            if !c.optional && c.arguments.is_empty()
                && matches!(c.callee.without_parentheses(), Expression::StaticMemberExpression(m)
                    if m.property.name == "json" && !m.optional))
    }

    /// What storing `e` in a binding declared `T[]` stores.
    fn write_of(&self, e: &Expression) -> Write {
        let e = e.without_parentheses();
        let e = match e {
            Expression::TSAsExpression(a) => a.expression.without_parentheses(),
            Expression::TSSatisfiesExpression(a) => a.expression.without_parentheses(),
            Expression::TSNonNullExpression(a) => a.expression.without_parentheses(),
            e => e,
        };
        if let Some(&(_, k)) = self.array_sites.get(&e.span().start) {
            return Write::Array(k);
        }
        match e {
            Expression::Identifier(id) => self.symbol(id).map_or(Write::Other, Write::Binding),
            _ => Write::Other,
        }
    }

    fn record_write(&mut self, sym: SymbolId, e: &Expression) {
        if matches!(self.types.vars.get(&sym), Some(SiteTy::Array(_))) {
            let w = self.write_of(e);
            self.writes.entry(sym).or_default().push(w);
        }
    }

    fn mark(&mut self, e: &Expression, ty: SiteTy) {
        match (e, ty) {
            (Expression::ParenthesizedExpression(p), _) => self.mark(&p.expression, ty),
            (Expression::TSAsExpression(a), _) => self.mark(&a.expression, ty),
            (Expression::TSSatisfiesExpression(a), _) => self.mark(&a.expression, ty),
            (Expression::TSNonNullExpression(a), _) => self.mark(&a.expression, ty),
            (Expression::ConditionalExpression(c), _) => {
                self.mark(&c.consequent, ty);
                self.mark(&c.alternate, ty);
            }
            (Expression::ObjectExpression(o), SiteTy::Struct(k)) => self.literal(o, k),
            (Expression::CallExpression(c), _) if self.is_json_parse(c) => {
                self.parse_sites.insert(c.span.start, (c.span.end, desc(ty)));
            }
            (Expression::AwaitExpression(a), _) if Self::is_json_call(&a.argument) => {
                self.json_sites.insert(a.span.start, (a.span.end, desc(ty)));
            }
            (Expression::NewExpression(n), SiteTy::Array(k)) if self.is_array_ctor(&n.callee) => {
                self.array_sites.insert(n.span.start, (n.span.end, k));
            }
            (Expression::CallExpression(c), SiteTy::Array(k)) if self.is_array_ctor(&c.callee) => {
                self.array_sites.insert(c.span.start, (c.span.end, k));
            }
            (Expression::ArrayExpression(a), SiteTy::Array(k)) => {
                self.array_sites.insert(a.span.start, (a.span.end, k));
                for el in &a.elements {
                    if let Some(e) = el.as_expression() {
                        self.mark(e, SiteTy::Struct(k));
                    }
                }
            }
            _ => {}
        }
    }

    fn err(&mut self, span: Span, msg: String) {
        self.errors.push(OxcDiagnostic::error(msg).with_label(span));
    }

    /// An object literal of layout `k`: its fields must make a valid object.
    fn literal(&mut self, o: &ObjectExpression, k: u32) {
        let layout = &self.layouts[k as usize];
        let name = layout.name.clone();
        let mut mask = 0u32;
        // Per field: the literal value, if a literal (number, string, bool).
        let mut values: Vec<Option<&Expression>> = vec![None; layout.fields.len()];
        let mut nested = Vec::new();
        for p in &o.properties {
            let ObjectPropertyKind::ObjectProperty(p) = p else {
                self.err(p.span(), format!("a spread is not allowed in an object of `aot type` `{name}`"));
                continue;
            };
            if p.kind != PropertyKind::Init || p.method || p.computed {
                self.err(p.span, format!("an object of `aot type` `{name}` can only have plain fields"));
                continue;
            }
            let key = match &p.key {
                PropertyKey::StaticIdentifier(id) => id.name.to_string(),
                PropertyKey::StringLiteral(s) => s.value.to_string(),
                _ => {
                    self.err(p.key.span(), "the field name must be an identifier or a string".into());
                    continue;
                }
            };
            let Some(i) = layout.fields.iter().position(|f| f.name == key) else {
                self.err(p.key.span(), format!("`{key}` is not a field of `aot type` `{name}`"));
                continue;
            };
            if mask & (1 << i) != 0 {
                self.err(p.key.span(), format!("`{key}` is set twice"));
                continue;
            }
            mask |= 1 << i;
            values[i] = Some(&p.value);
            // A nested literal: of the field's `aot type` (or an array of
            // them), if one.
            if let Some(t) = field_site_at(layout, i) {
                nested.push((&p.value, t));
            }
        }
        // Some member whose fields it has, with literal values of its types.
        let values_fit = |m: &LayoutMember| {
            (0..layout.fields.len()).all(|i| match (&m.types[i], values[i]) {
                (FieldType::Absent, _) | (_, None) => true,
                (t, Some(v)) => literal_fits(t, m.optional & (1 << i) != 0, v) != Some(false),
            })
        };
        let required = |m: &LayoutMember| m.mask & !m.optional;
        if !layout.members.iter().any(|m| required(m) & !mask == 0 && values_fit(m)) {
            // The fields missing for the closest member its values fit.
            let missing = layout
                .members
                .iter()
                .filter(|m| values_fit(m))
                .map(|m| required(m) & !mask)
                .min_by_key(|b| b.count_ones())
                .filter(|b| *b != 0)
                .map(|b| {
                    let names: Vec<String> = (0..layout.fields.len())
                        .filter(|i| b & (1 << i) != 0)
                        .map(|i| format!("`{}`", layout.fields[i].name))
                        .collect();
                    format!("missing {}", names.join(", "))
                })
                .unwrap_or_else(|| "a field has a value of another type".into());
            self.err(o.span, format!("not an object of `aot type` `{name}`: {missing}"));
        }
        // Other fields: of a type some member allows.
        for (i, v) in values.iter().enumerate() {
            if let Some(v) = v
                && !layout.members.iter().any(|m| {
                    m.types[i] != FieldType::Absent
                        && literal_fits(&m.types[i], m.optional & (1 << i) != 0, v) != Some(false)
                })
            {
                self.err(v.span(), format!("`{}` of `{name}` cannot hold this value", layout.fields[i].name));
            }
        }
        self.sites.insert(o.span.start, (o.span.end, k));
        for (v, t) in nested {
            self.mark(v, t);
        }
    }
}

/// What a parse site makes (`Structs::parse_sites`).
fn desc(ty: SiteTy) -> i32 {
    match ty {
        SiteTy::Struct(k) => k as i32 * 2,
        SiteTy::Array(k) => k as i32 * 2 + 1,
    }
}

/// What field `i` of layout `l` holds in every member that has it, if an
/// object of an `aot type` or an array of them.
fn field_site_at(l: &Layout, i: usize) -> Option<SiteTy> {
    let mut ts = l.members.iter().map(|m| m.types[i].base()).filter(|t| **t != FieldType::Absent);
    let first = ts.next()?;
    let site = match first {
        FieldType::Struct(k) => SiteTy::Struct(*k),
        FieldType::Array(k) => SiteTy::Array(*k),
        _ => return None,
    };
    ts.all(|t| t == first).then_some(site)
}

/// Whether a literal value fits a field type (of a field that may be
/// absent: `optional`; `None`: not a literal).
fn literal_fits(t: &FieldType, optional: bool, v: &Expression) -> Option<bool> {
    match v.without_parentheses() {
        Expression::NullLiteral(_) => return Some(matches!(t, FieldType::Nullable(_))),
        Expression::Identifier(id) if id.name == "undefined" => return Some(optional),
        _ => {}
    }
    let t = t.base();
    let num = match v.without_parentheses() {
        Expression::NumericLiteral(n) => Some(n.value),
        Expression::UnaryExpression(u) if u.operator == UnaryOperator::UnaryNegation => match &u.argument {
            Expression::NumericLiteral(n) => Some(-n.value),
            _ => None,
        },
        _ => None,
    };
    if let Some(n) = num {
        return Some(match t {
            FieldType::Int => n.fract() == 0.0,
            FieldType::Number => true,
            _ => false,
        });
    }
    match v.without_parentheses() {
        Expression::StringLiteral(s) => Some(match t {
            FieldType::String => true,
            FieldType::Literals(ls) => ls.iter().any(|l| l == s.value.as_str()),
            _ => false,
        }),
        Expression::BooleanLiteral(_) => Some(*t == FieldType::Bool),
        Expression::ObjectExpression(_) => Some(matches!(t, FieldType::Struct(_))),
        Expression::ArrayExpression(_) => Some(matches!(t, FieldType::Array(_))),
        _ => None,
    }
}

impl<'a> Visit<'a> for Sites<'_> {
    fn visit_variable_declarator(&mut self, d: &VariableDeclarator<'a>) {
        if let BindingPattern::BindingIdentifier(id) = &d.id
            && let Some(sym) = id.symbol_id.get()
            && let Some(&t) = self.types.vars.get(&sym)
            && let Some(init) = &d.init
        {
            self.mark(init, t);
            self.record_write(sym, init);
        }
        walk::walk_variable_declarator(self, d);
    }

    fn visit_assignment_expression(&mut self, a: &AssignmentExpression<'a>) {
        if a.operator == AssignmentOperator::Assign {
            match &a.left {
                AssignmentTarget::AssignmentTargetIdentifier(id) => {
                    if let Some(sym) = self.symbol(id)
                        && let Some(&t) = self.types.vars.get(&sym)
                    {
                        self.mark(&a.right, t);
                        self.record_write(sym, &a.right);
                    }
                }
                AssignmentTarget::ComputedMemberExpression(m) => {
                    if let Some(SiteTy::Array(k)) = self.declared(&m.object) {
                        self.mark(&a.right, SiteTy::Struct(k));
                    }
                }
                AssignmentTarget::StaticMemberExpression(m) => {
                    if let Some(SiteTy::Struct(k)) = self.declared(&m.object)
                        && let Some(t) = self.field_site(k, m.property.name.as_str())
                    {
                        self.mark(&a.right, t);
                    }
                }
                _ => {}
            }
        }
        walk::walk_assignment_expression(self, a);
    }

    fn visit_call_expression(&mut self, c: &CallExpression<'a>) {
        match c.callee.without_parentheses() {
            Expression::Identifier(id) => {
                if let Some((params, _)) = self.symbol(id).and_then(|s| self.types.fns.get(&s)).cloned() {
                    for (arg, p) in c.arguments.iter().zip(params) {
                        if let (Some(e), Some(t)) = (arg.as_expression(), p) {
                            self.mark(e, t);
                        }
                    }
                }
            }
            // Elements stored by array methods.
            Expression::StaticMemberExpression(m) => {
                let first = match m.property.name.as_str() {
                    "push" | "unshift" => Some(0),
                    "splice" => Some(2),
                    "fill" => Some(0),
                    _ => None,
                };
                if let Some(first) = first
                    && let Some(SiteTy::Array(k)) = self.declared(&m.object)
                {
                    let last = if m.property.name == "fill" { 1 } else { c.arguments.len() };
                    for arg in c.arguments.iter().take(last).skip(first) {
                        if let Some(e) = arg.as_expression() {
                            self.mark(e, SiteTy::Struct(k));
                        }
                    }
                }
            }
            _ => {}
        }
        walk::walk_call_expression(self, c);
    }

    fn visit_function(&mut self, f: &Function<'a>, flags: ScopeFlags) {
        let ret = f.return_type.as_ref().and_then(|a| site_ty(&a.type_annotation, self.types.ids, self.types.arrays));
        self.returns.push(ret);
        walk::walk_function(self, f, flags);
        self.returns.pop();
    }

    fn visit_arrow_function_expression(&mut self, f: &ArrowFunctionExpression<'a>) {
        let ret = f.return_type.as_ref().and_then(|a| site_ty(&a.type_annotation, self.types.ids, self.types.arrays));
        if let Some(t) = ret
            && let Some(e) = f.body.as_expression()
        {
            self.mark(e, t);
        }
        self.returns.push(ret);
        walk::walk_arrow_function_expression(self, f);
        self.returns.pop();
    }

    fn visit_ts_as_expression(&mut self, a: &TSAsExpression<'a>) {
        // `JSON.parse(text) as T`, `(await r.json()) as T`: parse sites.
        let parse = match a.expression.without_parentheses() {
            Expression::CallExpression(c) => self.is_json_parse(c),
            Expression::AwaitExpression(aw) => Self::is_json_call(&aw.argument),
            _ => false,
        };
        if parse && let Some(t) = site_ty(&a.type_annotation, self.types.ids, self.types.arrays) {
            self.mark(&a.expression, t);
        }
        walk::walk_ts_as_expression(self, a);
    }

    fn visit_return_statement(&mut self, r: &ReturnStatement<'a>) {
        if let (Some(Some(t)), Some(e)) = (self.returns.last(), &r.argument) {
            self.mark(e, *t);
        }
        walk::walk_return_statement(self, r);
    }
}
