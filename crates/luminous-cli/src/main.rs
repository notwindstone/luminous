mod glue;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use luminous_codegen::{Backend, Output};
use luminous_front::Analysis;
use luminous_front::bundle::Bundle;
use luminous_front::erase::NATIVE_MODULE;

const RT_C: &str = include_str!("../../../runtime/luminous_rt.c");
const RT_H: &str = include_str!("../../../runtime/luminous_rt.h");

#[derive(Parser)]
#[command(name = "luminous", about = "Ahead-of-time compiler for a strict TypeScript subset, on QuickJS-NG")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// Make `++`, `--`, `+=`, `-=`, `*=` on `int` variables compile errors
    /// unless they provably stay int32s, instead of wrapping like `| 0`.
    #[arg(long, global = true)]
    no_int_wrapping: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Compile a .ts file to a native executable.
    Build {
        input: PathBuf,
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[command(flatten)]
        opts: BuildOpts,
    },
    /// Compile and run a .ts file.
    Run {
        input: PathBuf,
        #[command(flatten)]
        opts: BuildOpts,
        /// Arguments passed to the program.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Show which functions compile natively, and why the others do not.
    Report { input: PathBuf },
    /// Print the type-erased JavaScript run by the interpreter.
    EmitJs {
        input: PathBuf,
        #[arg(long)]
        no_native: bool,
    },
    /// Print the typed IR of the natively compiled functions.
    Ir { input: PathBuf },
}

#[derive(Args, Clone)]
struct BuildOpts {
    /// Compile everything with the interpreter (baseline for comparisons).
    #[arg(long)]
    no_native: bool,
    /// Keep intermediate files (JS, C glue, object file) in this directory.
    #[arg(long)]
    keep: Option<PathBuf>,
    /// Print the native/interpreted report while building.
    #[arg(short, long)]
    verbose: bool,
    /// QuickJS-NG source directory (headers). Default: $LUMINOUS_QUICKJS_DIR.
    #[arg(long)]
    quickjs_dir: Option<PathBuf>,
    /// QuickJS-NG build directory (qjsc, libqjs.a). Default: $LUMINOUS_QUICKJS_BUILD.
    #[arg(long)]
    quickjs_build: Option<PathBuf>,
    /// Code generator for the native functions: `cranelift` (compiles fast)
    /// or `llvm` (optimizes further; needs clang). Default: $LUMINOUS_BACKEND,
    /// else cranelift.
    #[arg(long)]
    backend: Option<Backend>,
    /// The clang used by the LLVM backend. Default: $LUMINOUS_CLANG, else clang.
    #[arg(long)]
    clang: Option<PathBuf>,
    /// The CPU the LLVM backend compiles for, as clang's `-march` (e.g.
    /// `x86-64-v3`). Default: the host's.
    #[arg(long)]
    march: Option<String>,
}

impl BuildOpts {
    fn backend(&self) -> Result<Backend> {
        match (&self.backend, std::env::var("LUMINOUS_BACKEND")) {
            (Some(b), _) => Ok(*b),
            (None, Ok(b)) if !b.is_empty() => b.parse().map_err(anyhow::Error::msg),
            _ => Ok(Backend::Cranelift),
        }
    }

    fn codegen(&self) -> Result<luminous_codegen::Options> {
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

fn main() {
    if let Err(e) = real_main() {
        let msg = format!("{e:#}");
        // Rendered diagnostics are self-describing; prefix plain messages only.
        if msg.contains('\n') {
            eprintln!("{msg}")
        } else {
            eprintln!("luminous: {msg}")
        }
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    let fopts = luminous_front::Options { int_wrapping: !cli.no_int_wrapping };
    match cli.cmd {
        Cmd::Build { input, output, opts } => {
            let output = output.unwrap_or_else(|| input.with_extension(""));
            build(&input, &output, &opts, &fopts)
        }
        Cmd::Run { input, opts, args } => {
            let dir = tempdir()?;
            let exe = dir.join("a.out");
            let r = build(&input, &exe, &opts, &fopts).and_then(|()| {
                let status = Command::new(&exe).args(&args).status()?;
                Ok(status.code().unwrap_or(1))
            });
            let _ = std::fs::remove_dir_all(&dir);
            std::process::exit(r?);
        }
        Cmd::Report { input } => {
            let (bundle, analysis) = analyze(&input, &fopts)?;
            print_report(&bundle, &analysis);
            Ok(())
        }
        Cmd::EmitJs { input, no_native } => {
            let (_, mut analysis) = analyze(&input, &fopts)?;
            if no_native {
                disable_native(&mut analysis);
            }
            print!("{}", luminous_front::erased_js(&input, &analysis).map_err(anyhow::Error::msg)?);
            Ok(())
        }
        Cmd::Ir { input } => {
            let (_, analysis) = analyze(&input, &fopts)?;
            for (i, f) in analysis.module.funcs.iter().enumerate() {
                println!("// lum_fn_{i}\n{f:#?}\n");
            }
            for (i, f) in analysis.module.closures.iter().enumerate() {
                println!("// closure {i}\n{f:#?}\n");
            }
            Ok(())
        }
    }
}

/// The program's modules (merged into one; see `luminous_front::bundle`),
/// analyzed.
fn analyze(input: &Path, fopts: &luminous_front::Options) -> Result<(Bundle, Analysis)> {
    let bundle = luminous_front::bundle::bundle(input).map_err(anyhow::Error::msg)?;
    let analysis = luminous_front::analyze_bundle(&bundle, fopts).map_err(anyhow::Error::msg)?;
    Ok((bundle, analysis))
}

fn disable_native(a: &mut Analysis) {
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

fn print_report(bundle: &Bundle, a: &Analysis) {
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

fn tempdir() -> Result<PathBuf> {
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

fn build(input: &Path, output: &Path, opts: &BuildOpts, fopts: &luminous_front::Options) -> Result<()> {
    let q = quickjs(opts)?;
    let (bundle, mut analysis) = analyze(input, fopts)?;
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
    let result = (|| -> Result<()> {
        let script_name = input.file_name().unwrap().to_string_lossy().into_owned();
        let js_path = tmp.join(Path::new(&script_name).with_extension("js"));
        std::fs::write(&js_path, &js)?;

        let has_native = !analysis.module.funcs.is_empty();
        // The luminous:native module is always linked: its initialization locks
        // the Math and Number builtins, in native and --no-native builds alike.
        let mut qjsc = Command::new(q.build.join("qjsc"));
        qjsc.arg("-e").arg("-m").arg("-n").arg(&script_name).arg("-o").arg(tmp.join("main.c"));
        qjsc.arg("-M").arg(format!("{NATIVE_MODULE},{}", glue::MODULE_CNAME));
        run(qjsc.arg(&js_path))?;

        std::fs::write(
            tmp.join("glue.c"),
            glue::generate(&analysis.module, &mutable_cells(&analysis), &analysis.classes, fopts.int_wrapping),
        )?;
        std::fs::write(tmp.join("luminous_rt.c"), RT_C)?;
        std::fs::write(tmp.join("luminous_rt.h"), RT_H)?;
        let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
        let mut link = Command::new(cc);
        link.args(["-O2", "-w"]).arg("-I").arg(&q.src).arg("-I").arg(&tmp);
        link.arg(tmp.join("main.c")).arg(tmp.join("glue.c")).arg(tmp.join("luminous_rt.c"));
        if has_native {
            let cg = opts.codegen()?;
            let obj = tmp.join("native.o");
            match luminous_codegen::compile(&analysis.module, &cg)? {
                Output::Object(bytes) => std::fs::write(&obj, bytes)?,
                Output::LlvmIr(ir) => {
                    let ll = tmp.join("native.ll");
                    std::fs::write(&ll, ir)?;
                    luminous_codegen::llvm::compile(&ll, &obj, &cg.llvm)?;
                }
            }
            link.arg(&obj);
        }
        link.arg(q.build.join("libqjs-libc.a")).arg(q.build.join("libqjs.a"));
        link.args(["-lm", "-lpthread", "-ldl", "-o"]).arg(output);
        run(&mut link)
    })();
    if opts.keep.is_none() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    result
}
