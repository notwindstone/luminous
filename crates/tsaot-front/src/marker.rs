//! The `aot` and `interpreted` contextual keywords: `aot function f() {}`,
//! `aot (x) => x`, `interpreted async function g() {}`, ...
//!
//! Like `async`, `aot` is only a keyword directly before a function: no line
//! break may follow it, and everywhere else it is an ordinary identifier. The
//! TypeScript parser does not know it, so markers are found here and replaced
//! with spaces, which keeps every byte offset of the source valid.
//!
//! Instead of re-implementing a JS lexer (strings, comments, templates and
//! regexes all contain arbitrary text), candidates are confirmed with the real
//! parser: an occurrence is a marker exactly when, with it blanked, the source
//! parses and a function or arrow function starts right after it. `aot (x)` as
//! a call blanks to a parenthesized expression, not a function, so it stays an
//! identifier; an `aot` inside a string or comment never precedes a function
//! node.

use std::collections::HashSet;

use oxc::allocator::Allocator;
use oxc::ast::ast::{ArrowFunctionExpression, Function, Program};
use oxc::ast_visit::{Visit, walk};
use oxc::parser::Parser;
use oxc::span::SourceType;
use oxc::syntax::scope::ScopeFlags;

pub const KEYWORD: &str = "aot";
/// Keeps a function interpreted.
pub const INTERPRETED: &str = "interpreted";
const KEYWORDS: [&str; 2] = [KEYWORD, INTERPRETED];

pub struct Stripped {
    /// The source with every marker replaced by spaces.
    pub source: String,
    /// Start offsets of the functions marked `aot` (the token after it).
    pub marked: HashSet<u32>,
    /// Start offsets of the functions marked `interpreted`.
    pub interpreted: HashSet<u32>,
}

struct Candidate {
    keyword: &'static str,
    at: usize,
    /// Offset of the first token after the keyword.
    next: usize,
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

fn candidates(src: &str) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = KEYWORDS.iter().flat_map(|k| keyword_candidates(src, k)).collect();
    out.sort_by_key(|c| c.at);
    out
}

fn keyword_candidates(src: &str, keyword: &'static str) -> Vec<Candidate> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = src[from..].find(keyword).map(|i| i + from) {
        from = i + keyword.len();
        let end = i + keyword.len();
        if i > 0 && (is_ident_byte(bytes[i - 1]) || bytes[i - 1] == b'.') {
            continue;
        }
        if bytes.get(end).is_some_and(|&b| is_ident_byte(b)) {
            continue;
        }
        let mut next = end;
        while next < bytes.len() && matches!(bytes[next], b' ' | b'\t') {
            next += 1;
        }
        let Some(&b) = bytes.get(next) else { continue };
        let ok = match b {
            b'(' | b'<' => true,
            _ => next > end && is_ident_byte(b),
        };
        if ok {
            out.push(Candidate { keyword, at: i, next });
        }
    }
    out
}

fn blank(src: &str, cands: &[&Candidate]) -> String {
    let mut out = src.to_string();
    for c in cands {
        out.replace_range(c.at..c.at + c.keyword.len(), &" ".repeat(c.keyword.len()));
    }
    out
}

#[derive(Default)]
struct FunctionStarts(HashSet<u32>);

impl<'a> Visit<'a> for FunctionStarts {
    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        self.0.insert(it.span.start);
        walk::walk_function(self, it, flags);
    }
    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        self.0.insert(it.span.start);
        walk::walk_arrow_function_expression(self, it);
    }
}

/// Parse `src`; on success, the start offsets of all functions.
fn function_starts(src: &str) -> Option<HashSet<u32>> {
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, src, SourceType::ts().with_module(true)).parse();
    if !ret.diagnostics.is_empty() {
        return None;
    }
    let program: &Program = &ret.program;
    let mut v = FunctionStarts::default();
    v.visit_program(program);
    Some(v.0)
}

pub fn strip(src: &str) -> Stripped {
    let all = candidates(src);
    let mut active: Vec<&Candidate> = all.iter().collect();
    'outer: while !active.is_empty() {
        match function_starts(&blank(src, &active)) {
            Some(starts) => {
                let before = active.len();
                active.retain(|c| starts.contains(&(c.next as u32)));
                if active.len() == before {
                    break;
                }
            }
            None => {
                // Blanking an identifier use (e.g. `function aot(x)`) broke the
                // parse: drop the first candidate whose removal fixes it.
                for i in 0..active.len() {
                    let mut without = active.clone();
                    without.remove(i);
                    if function_starts(&blank(src, &without)).is_some() {
                        active = without;
                        continue 'outer;
                    }
                }
                // A genuine syntax error; let the main parse report it.
                break;
            }
        }
    }
    let of = |k: &str| active.iter().filter(|c| c.keyword == k).map(|c| c.next as u32).collect();
    Stripped { source: blank(src, &active), marked: of(KEYWORD), interpreted: of(INTERPRETED) }
}

/// Markers directly followed by a keyword that cannot start a function
/// (`aot class`, `interpreted const`, ...): offset and keyword. Used to
/// explain parse errors.
pub fn misplaced(src: &str) -> Vec<(u32, &'static str)> {
    const NOT_FUNCTIONS: &[&str] = &[
        "class",
        "const",
        "let",
        "var",
        "interface",
        "type",
        "enum",
        "namespace",
        "import",
        "export",
        "get",
        "set",
        "static",
        "abstract",
        "declare",
    ];
    candidates(src)
        .into_iter()
        .filter(|c| {
            let rest = &src[c.next..];
            let word: String = rest.bytes().take_while(|&b| is_ident_byte(b)).map(char::from).collect();
            NOT_FUNCTIONS.contains(&word.as_str())
        })
        .map(|c| (c.at as u32, c.keyword))
        .collect()
}
