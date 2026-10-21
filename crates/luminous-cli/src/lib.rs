//! The Luminous compiler as a library: what `luminous build` does, for
//! embedders (a runtime built on Luminous adds its modules, native C modules,
//! libraries and event loop: `EmbedOpts`).

mod glue;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Args;
pub use luminous_codegen::Backend;
use luminous_codegen::Output;
use luminous_front::Analysis;
use luminous_front::bundle::Bundle;
use luminous_front::erase::NATIVE_MODULE;

const RT_C: &str = include_str!("../../../runtime/luminous_rt.c");
const RT_H: &str = include_str!("../../../runtime/luminous_rt.h");

#[derive(Args, Clone, Default)]
pub struct BuildOpts {
    /// Compile everything with the interpreter (baseline for comparisons).
    #[arg(long)]
    pub no_native: bool,
    /// Keep intermediate files (JS, C glue, object file) in this directory.
    #[arg(long)]
    pub keep: Option<PathBuf>,
    /// Print the native/interpreted report while building.
    #[arg(short, long)]
    pub verbose: bool,
    /// QuickJS-NG source directory (headers). Default: $LUMINOUS_QUICKJS_DIR.
    #[arg(long)]
    pub quickjs_dir: Option<PathBuf>,
    /// QuickJS-NG build directory (qjsc, libqjs.a). Default: $LUMINOUS_QUICKJS_BUILD.
    #[arg(long)]
    pub quickjs_build: Option<PathBuf>,
    /// Code generator for the native functions: `cranelift` (compiles fast)
    /// or `llvm` (optimizes further; needs clang). Default: $LUMINOUS_BACKEND,
    /// else cranelift.
    #[arg(long)]
    pub backend: Option<Backend>,
    /// The clang used by the LLVM backend. Default: $LUMINOUS_CLANG, else clang.
    #[arg(long)]
    pub clang: Option<PathBuf>,
    /// The CPU the LLVM backend compiles for, as clang's `-march` (e.g.
    /// `x86-64-v3`). Default: the host's.
    #[arg(long)]
    pub march: Option<String>,
    #[command(flatten)]
    pub embed: EmbedOpts,
}

/// What an embedder (a runtime built on Luminous) adds to a program.
#[derive(Args, Clone, Default, Debug)]
pub struct EmbedOpts {
    /// A module that runs before the program's entry (repeatable).
    #[arg(long = "prelude", value_name = "FILE")]
    pub preludes: Vec<PathBuf>,
    /// Specifiers that start with PREFIX name the modules of DIR
    /// (`--module-dir beyond:=lib`: `beyond:http` is `lib/http.ts`).
    #[arg(long = "module-dir", value_name = "PREFIX=DIR", value_parser = parse_module_dir)]
    pub module_dirs: Vec<(String, PathBuf)>,
    /// A native C module: its specifier and the C function that initializes
    /// it (`JSModuleDef *FUNCTION(JSContext *, const char *)`).
    #[arg(long = "c-module", value_name = "SPECIFIER=FUNCTION", value_parser = parse_c_module)]
    pub c_modules: Vec<(String, String)>,
    /// `int FUNCTION(JSContext *ctx, int argc, char **argv)`: called before
    /// the program runs (a nonzero result is the exit status).
    #[arg(long, value_name = "FUNCTION")]
    pub init_fn: Option<String>,
    /// `int FUNCTION(JSContext *ctx)`: runs the event loop after the
    /// program's module code, instead of the engine's (`js_std_loop`); its
    /// result is the exit status (and an exception it leaves pending is
    /// printed).
    #[arg(long, value_name = "FUNCTION")]
    pub loop_fn: Option<String>,
    /// An object file, static library or linker argument (repeatable).
    #[arg(long = "link", value_name = "ARG", allow_hyphen_values = true)]
    pub link: Vec<String>,
    /// A global that is a binding of the embedder's module SPECIFIER (which
    /// exports it as NAME, and a prelude makes it the global): the
    /// program's modules refer to the binding, which native code knows
    /// (`--global Response=beyond:fetch`).
    #[arg(long = "global", value_name = "NAME=SPECIFIER", value_parser = parse_global)]
    pub globals: Vec<(String, String)>,
    /// A specifier that is another's (`--alias fs=node:fs`).
    #[arg(long = "alias", value_name = "SPECIFIER=TARGET", value_parser = parse_alias)]
    pub aliases: Vec<(String, String)>,
    /// A directory whose modules the program registers by path, for
    /// modules loaded at run time to import them (repeatable).
    #[arg(long = "shared", value_name = "DIR")]
    pub shared: Vec<PathBuf>,
}

fn parse_alias(s: &str) -> Result<(String, String), String> {
    let (from, to) = s.split_once('=').ok_or("expected SPECIFIER=TARGET")?;
    Ok((from.to_string(), to.to_string()))
}

fn parse_global(s: &str) -> Result<(String, String), String> {
    let (name, spec) = s.split_once('=').ok_or("expected NAME=SPECIFIER")?;
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$') {
        return Err(format!("`{name}` is not an identifier"));
    }
    Ok((name.to_string(), spec.to_string()))
}

fn parse_module_dir(s: &str) -> Result<(String, PathBuf), String> {
    let (prefix, dir) = s.split_once('=').ok_or("expected PREFIX=DIR")?;
    Ok((prefix.to_string(), PathBuf::from(dir)))
}

fn parse_c_module(s: &str) -> Result<(String, String), String> {
    let (spec, f) = s.split_once('=').ok_or("expected SPECIFIER=FUNCTION")?;
    if f.is_empty() || !f.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!("`{f}` is not a C function name"));
    }
    Ok((spec.to_string(), f.to_string()))
}

impl EmbedOpts {
    fn embedding(&self) -> luminous_front::bundle::Embedding {
        luminous_front::bundle::Embedding {
            preludes: self.preludes.clone(),
            module_dirs: self.module_dirs.clone(),
            externals: self.c_modules.iter().map(|(s, _)| s.clone()).collect(),
            globals: self.globals.clone(),
            aliases: self.aliases.clone(),
            shared: self.shared.clone(),
            library: false,
        }
    }
}

/// The program's `main`: the engine's set-up (as `qjsc -e` writes it), the
/// C modules, the embedder's hooks, then the program (`lum_program`, the
/// bytecode `qjsc` wrote).
fn main_c(embed: &EmbedOpts) -> String {
    let mut decls = String::new();
    let mut inits = String::new();
    for (spec, f) in &embed.c_modules {
        decls.push_str(&format!("JSModuleDef *{f}(JSContext *ctx, const char *name);\n"));
        inits.push_str(&format!("  {f}(ctx, {});\n", c_string(spec)));
    }
    let mut init = String::new();
    if let Some(f) = &embed.init_fn {
        decls.push_str(&format!("int {f}(JSContext *ctx, int argc, char **argv);\n"));
        init = format!("  r = {f}(ctx, argc, argv);\n  if (r)\n    goto done;\n");
    }
    // With the embedder's loop, the module is evaluated without waiting for
    // it (a top-level `await` waits on what that loop runs): an error it
    // throws at once is printed and ends the program, one after an `await`
    // is reported by the embedder (as an unhandled rejection) or, if the
    // loop ends without it, here.
    let (run, lp, after) = match &embed.loop_fn {
        Some(f) => {
            decls.push_str(&format!("int {f}(JSContext *ctx);\n"));
            (
                "  JSValue lum_main = lum_eval_module(ctx);\n  if (JS_IsException(lum_main) || lum_rejected(ctx, lum_main)) {\n    if (JS_IsException(lum_main))\n      js_std_dump_error(ctx);\n    JS_FreeValue(ctx, lum_main);\n    r = 1;\n    goto done;\n  }\n"
                    .to_string(),
                format!("{f}(ctx)"),
                "  if (!r && lum_rejected(ctx, lum_main))\n    r = 1;\n  JS_FreeValue(ctx, lum_main);\n".to_string(),
            )
        }
        None => (
            "  js_std_eval_binary(ctx, lum_program, lum_program_size, 0);\n".to_string(),
            "js_std_loop(ctx)".to_string(),
            String::new(),
        ),
    };
    format!(
        r#"/* Generated by luminous. */
#include "quickjs-libc.h"

extern const uint32_t lum_program_size;
extern const uint8_t lum_program[];
JSModuleDef *js_init_module_{cname}(JSContext *ctx, const char *name);
{decls}
static JSContext *JS_NewCustomContext(JSRuntime *rt)
{{
  JSContext *ctx = JS_NewContext(rt);
  if (!ctx)
    return NULL;
  js_init_module_std(ctx, "qjs:std");
  js_init_module_os(ctx, "qjs:os");
  js_init_module_bjson(ctx, "qjs:bjson");
  js_init_module_{cname}(ctx, {native});
{inits}  return ctx;
}}

/* The program's module, evaluated: its promise, or an exception. */
static JSValue lum_eval_module(JSContext *ctx)
{{
  JSValue obj = JS_ReadObject(ctx, lum_program, lum_program_size, JS_READ_OBJ_BYTECODE);
  if (JS_IsException(obj))
    return obj;
  if (JS_VALUE_GET_TAG(obj) == JS_TAG_MODULE) {{
    if (JS_ResolveModule(ctx, obj) < 0) {{
      JS_FreeValue(ctx, obj);
      return JS_EXCEPTION;
    }}
    if (js_module_set_import_meta(ctx, obj, false, true) < 0)
      return JS_EXCEPTION;
  }}
  return JS_EvalFunction(ctx, obj);
}}

/* Whether the module's promise was rejected (its reason is printed). */
static int lum_rejected(JSContext *ctx, JSValueConst p)
{{
  if (!JS_IsPromise(p) || JS_PromiseState(ctx, p) != JS_PROMISE_REJECTED)
    return 0;
  JS_Throw(ctx, JS_PromiseResult(ctx, p));
  js_std_dump_error(ctx);
  return 1;
}}

int main(int argc, char **argv)
{{
  int r = 0;
  JSRuntime *rt = JS_NewRuntime();
  js_std_set_worker_new_context_func(JS_NewCustomContext);
  js_std_init_handlers(rt);
  JS_SetModuleLoaderFunc2(rt, NULL, js_module_loader, js_module_check_attributes, NULL);
  JSContext *ctx = JS_NewCustomContext(rt);
  js_std_add_helpers(ctx, argc, argv);
{init}{run}  r = {lp};
  if (r && JS_HasException(ctx))
    js_std_dump_error(ctx);
{after}done:
  js_std_free_handlers(rt);
  JS_FreeContext(ctx);
  JS_FreeRuntime(rt);
  return r;
}}
"#,
        cname = glue::MODULE_CNAME,
        native = c_string(NATIVE_MODULE),
    )
}

fn c_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_ascii_graphic() || c == ' ' => out.push(c),
            c => out.push_str(&format!("\\u{:04x}", c as u32)),
        }
    }
    out.push('"');
    out
}

impl BuildOpts {
    pub fn backend(&self) -> Result<Backend> {
        match (&self.backend, std::env::var("LUMINOUS_BACKEND")) {
            (Some(b), _) => Ok(*b),
            (None, Ok(b)) if !b.is_empty() => b.parse().map_err(anyhow::Error::msg),
            _ => Ok(Backend::Cranelift),
        }
    }

    pub fn codegen(&self) -> Result<luminous_codegen::Options> {
        let mut o = luminous_codegen::Options { backend: self.backend()?, ..Default::default() };
        if let Some(c) = self.clang.clone().or_else(|| std::env::var_os("LUMINOUS_CLANG").map(PathBuf::from)) {
            o.llvm.clang = c;
        }
        if let Some(m) = &self.march {
            o.llvm.march = m.clone();
        }
        Ok(o)
    }
}

/// The program's modules (merged into one; see `luminous_front::bundle`),
/// analyzed.
pub fn analyze(input: &Path, fopts: &luminous_front::Options) -> Result<(Bundle, Analysis)> {
    analyze_with(input, fopts, &EmbedOpts::default())
}

/// `analyze`, with what an embedder adds.
pub fn analyze_with(input: &Path, fopts: &luminous_front::Options, embed: &EmbedOpts) -> Result<(Bundle, Analysis)> {
    let bundle = luminous_front::bundle::bundle_with(input, &embed.embedding()).map_err(anyhow::Error::msg)?;
    for w in &bundle.warnings {
        eprintln!("warning: {w}");
    }
    let analysis = luminous_front::analyze_bundle(&bundle, fopts).map_err(anyhow::Error::msg)?;
    Ok((bundle, analysis))
}

pub fn disable_native(a: &mut Analysis) {
    a.natives.clear();
    // `aot type` objects are made by the runtime either way.
    let layouts = std::mem::take(&mut a.module.layouts);
    a.module = Default::default();
    a.module.layouts = layouts;
    a.cells.clear();
    a.cell_refs.clear();
    a.classes.clear();
}

/// The cells interpreted code reaches through accessors, and whether each
/// is a constant (assigning to it throws).
fn mutable_cells(a: &Analysis) -> Vec<(usize, bool)> {
    a.cells.iter().enumerate().filter(|(_, c)| c.mutable).map(|(i, c)| (i, c.constant)).collect()
}

pub fn print_report(bundle: &Bundle, a: &Analysis) {
    let mut lines: Vec<(u32, String)> =
        a.natives.iter().map(|n| (n.line, format!("native       {}", n.name))).collect();
    lines.extend(a.rejected.iter().map(|r| {
        let (file, line) = bundle.line(r.reason_line);
        let at = if bundle.line(r.line).0 == file { format!("line {line}") } else { format!("{}:{line}", file.display()) };
        (r.line, format!("interpreted  {} — {} ({at})", r.name, r.reason))
    }));
    lines.sort_by_key(|l| l.0);
    for (line, text) in lines {
        let (file, line) = bundle.line(line);
        println!("{}:{line}: {text}", file.display());
    }
}

pub fn tempdir() -> Result<PathBuf> {
    let base = std::env::var_os("TMPDIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.subsec_nanos();
    let dir = base.join(format!("luminous-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

struct QuickJs {
    src: PathBuf,
    build: PathBuf,
}

fn quickjs(opts: &BuildOpts) -> Result<QuickJs> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let pick = |arg: &Option<PathBuf>, var: &str, default: PathBuf| {
        arg.clone().or_else(|| std::env::var_os(var).map(PathBuf::from)).unwrap_or(default)
    };
    let q = QuickJs {
        src: pick(&opts.quickjs_dir, "LUMINOUS_QUICKJS_DIR", repo.join("build/quickjs-src")),
        build: pick(&opts.quickjs_build, "LUMINOUS_QUICKJS_BUILD", repo.join("build/quickjs")),
    };
    if !q.src.join("quickjs.h").exists() {
        bail!(
            "QuickJS-NG sources not found at {} (run scripts/build-quickjs.sh, or set LUMINOUS_QUICKJS_DIR)",
            q.src.display()
        );
    }
    for f in ["qjsc", "libqjs.a", "libqjs-libc.a"] {
        if !q.build.join(f).exists() {
            bail!("{} not found in {} (build QuickJS-NG, or set LUMINOUS_QUICKJS_BUILD)", f, q.build.display());
        }
    }
    Ok(q)
}

/// Returns the memory the allocator holds free to the system (glibc keeps
/// it otherwise, for the process's next allocations).
fn release_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        unsafe {
            malloc_trim(0);
        }
    }
}

fn run(cmd: &mut Command) -> Result<()> {
    let out = cmd.output().with_context(|| format!("running {:?}", cmd.get_program()))?;
    if !out.status.success() {
        bail!(
            "{:?} failed:\n{}{}",
            cmd.get_program(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// By native function: what `toString` gives for its closures, their
/// source without types.
fn closure_sources(analysis: &luminous_front::Analysis) -> Vec<Option<String>> {
    let m = &analysis.module;
    m.funcs
        .iter()
        .chain(&m.closures)
        .map(|f| {
            let (s, e) = f.source?;
            luminous_front::erase::function_source(analysis.source.get(s as usize..e as usize)?)
        })
        .collect()
}

/// The program's bytecode as C (`lum_program`, `lum_program_size`): one
/// string literal, which compilers read many times faster than a list of
/// numbers.
fn bytecode_c(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut c = String::with_capacity(bytes.len() * 2 + 256);
    let _ = write!(
        c,
        "#include <inttypes.h>\n\nconst uint32_t lum_program_size = {n};\n\nconst uint8_t lum_program[{n}] =\n\"",
        n = bytes.len()
    );
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && i % 100 == 0 {
            c.push_str("\"\n\"");
        }
        // (Octal escapes have at most three digits: a digit after one is
        // its own character. `?` could start a trigraph.)
        if (0x20..0x7f).contains(&b) && !matches!(b, b'"' | b'\\' | b'?') {
            c.push(b as char);
        } else {
            let _ = write!(c, "\\{b:03o}");
        }
    }
    c.push_str("\";\n");
    c
}

pub fn build(input: &Path, output: &Path, opts: &BuildOpts, fopts: &luminous_front::Options) -> Result<()> {
    let q = quickjs(opts)?;
    let fopts = &luminous_front::Options { native: fopts.native && !opts.no_native, ..*fopts };
    let t0 = std::time::Instant::now();
    let (bundle, mut analysis) = analyze_with(input, fopts, &opts.embed)?;
    luminous_codegen::time("analysis", t0);
    if opts.no_native {
        disable_native(&mut analysis);
    }
    if opts.verbose {
        print_report(&bundle, &analysis);
    }
    let js = luminous_front::erased_js(input, &analysis).map_err(anyhow::Error::msg)?;

    let tmp = match &opts.keep {
        Some(d) => {
            std::fs::create_dir_all(d)?;
            d.clone()
        }
        None => tempdir()?,
    };
    // (Ok: the link command, and the LLVM IR files to compile first to the
    // object files it links.)
    let result = (|| -> Result<(Command, Vec<PathBuf>, Vec<PathBuf>)> {
        let script_name = input.file_name().unwrap().to_string_lossy().into_owned();
        let js_path = tmp.join(Path::new(&script_name).with_extension("js"));
        std::fs::write(&js_path, &js)?;

        let has_native = !analysis.module.funcs.is_empty();
        // The luminous:native module is always linked: its initialization locks
        // the Math and Number builtins, in native and --no-native builds alike.
        let mut qjsc = Command::new(q.build.join("qjsc"));
        qjsc.arg("-b").arg("-m").arg("-n").arg(&script_name).arg("-o").arg(tmp.join("program.bin"));
        qjsc.arg("-M").arg(format!("{NATIVE_MODULE},{}", glue::MODULE_CNAME));
        for (spec, _) in &opts.embed.c_modules {
            qjsc.arg("-M").arg(spec);
        }
        run(qjsc.arg(&js_path))?;
        std::fs::write(tmp.join("program.c"), bytecode_c(&std::fs::read(tmp.join("program.bin"))?))?;
        std::fs::write(tmp.join("main.c"), main_c(&opts.embed))?;

        std::fs::write(
            tmp.join("glue.c"),
            glue::generate(
                &analysis.module,
                &mutable_cells(&analysis),
                &analysis.classes,
                fopts.int_wrapping,
                &closure_sources(&analysis),
            ),
        )?;
        std::fs::write(tmp.join("luminous_rt.c"), RT_C)?;
        std::fs::write(tmp.join("luminous_rt.h"), RT_H)?;
        let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
        let mut link = Command::new(cc);
        // (Without a stack protector or CET markers, as the engine: see
        // scripts/build-quickjs.sh.)
        link.args(["-O2", "-w", "-fno-stack-protector", "-fcf-protection=none"]);
        link.arg("-I").arg(&q.src).arg("-I").arg(&tmp);
        link.arg(tmp.join("main.c")).arg(tmp.join("program.c")).arg(tmp.join("glue.c")).arg(tmp.join("luminous_rt.c"));
        let (mut lls, mut objs) = (Vec::new(), Vec::new());
        if has_native {
            let cg = opts.codegen()?;
            match luminous_codegen::compile(&analysis.module, &cg)? {
                Output::Objects(objects) => {
                    for (i, bytes) in objects.iter().enumerate() {
                        let obj = tmp.join(if objects.len() == 1 { "native.o".into() } else { format!("native{i}.o") });
                        std::fs::write(&obj, bytes)?;
                        link.arg(&obj);
                    }
                }
                Output::LlvmIr(parts) => {
                    for (i, ir) in parts.iter().enumerate() {
                        let name = if parts.len() == 1 { "native".to_string() } else { format!("native{i}") };
                        let ll = tmp.join(format!("{name}.ll"));
                        std::fs::write(&ll, ir)?;
                        lls.push(ll);
                        objs.push(tmp.join(format!("{name}.o")));
                    }
                }
            }
        }
        Ok((link, lls, objs))
    })();
    // The clangs of a large program take gigabytes each: the program's
    // analysis and IR (as many) are freed, and their memory returned to the
    // system, before they start.
    drop((bundle, analysis, js));
    release_memory();
    let result = result.and_then(|(mut link, lls, objs)| {
        if !lls.is_empty() {
            let t0 = std::time::Instant::now();
            luminous_codegen::llvm::compile_parts(&lls, &objs, &opts.codegen()?.llvm)?;
            luminous_codegen::time("clang", t0);
            link.args(&objs);
        }
        // (The embedder's libraries use the engine's: before it.)
        link.args(&opts.embed.link);
        link.arg(q.build.join("libqjs-libc.a")).arg(q.build.join("libqjs.a"));
        link.args(["-lm", "-lpthread", "-ldl", "-o"]).arg(output);
        let t0 = std::time::Instant::now();
        run(&mut link)?;
        luminous_codegen::time("link", t0);
        Ok(())
    });
    if opts.keep.is_none() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    result
}
