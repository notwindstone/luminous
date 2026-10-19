use std::path::PathBuf;
use std::process::Command;

use anyhow::Result;
use clap::{Parser, Subcommand};
use luminous_cli::{BuildOpts, analyze, build, disable_native, print_report, tempdir};

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
    let fopts = luminous_front::Options { int_wrapping: !cli.no_int_wrapping, ..Default::default() };
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

