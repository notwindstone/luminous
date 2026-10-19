//! Differential tests: every case in `tests/cases` is built with native
//! compilation, once per backend (Cranelift, and LLVM when clang is
//! available), and fully interpreted; all executables must print exactly the
//! same output. `// @native a b` and `// @interpreted c` header lines pin down
//! which functions the checker is expected to compile natively. `// @flags
//! ...` passes options to the compiler, `// @env NAME=value ...` sets
//! variables for the runs (after the collector's own: see `run_case`).
//! `LUMINOUS_BACKENDS=cranelift` (or `llvm`) restricts the native builds to
//! one backend.

use std::path::{Path, PathBuf};
use std::process::Command;

fn luminous() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_luminous"));
    // Cranelift checks the IR it is given.
    cmd.env("LUMINOUS_VERIFY", "1");
    // Link against the engine built with assertions, so that leaked objects
    // abort the program at exit (see scripts/build-quickjs.sh).
    let check = repo().join("build/quickjs-check");
    if check.join("libqjs.a").exists() {
        cmd.env("LUMINOUS_QUICKJS_BUILD", check);
    }
    cmd
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn directive(src: &str, tag: &str) -> Vec<String> {
    src.lines()
        .filter_map(|l| l.strip_prefix(&format!("// @{tag} ")))
        .flat_map(|l| l.split_whitespace().map(str::to_string))
        .collect()
}

/// The backends to test: `LUMINOUS_BACKENDS` (comma-separated), else Cranelift
/// and, if clang runs, LLVM.
fn backends() -> Vec<String> {
    if let Ok(list) = std::env::var("LUMINOUS_BACKENDS") {
        return list.split(',').map(str::trim).filter(|b| !b.is_empty()).map(str::to_string).collect();
    }
    let clang = std::env::var("LUMINOUS_CLANG").unwrap_or_else(|_| "clang".into());
    let has_clang = Command::new(&clang).arg("--version").output().is_ok_and(|o| o.status.success());
    if !has_clang {
        eprintln!("note: {clang} not found, skipping the LLVM backend");
    }
    let mut list = vec!["cranelift".to_string()];
    list.extend(has_clang.then(|| "llvm".to_string()));
    list
}

fn run_case(case: &Path, out_dir: &Path, backends: &[String]) -> Result<(), String> {
    let src = std::fs::read_to_string(case).unwrap();
    let flags = directive(&src, "flags");
    let report = luminous().arg("report").args(&flags).arg(case).output().unwrap();
    if !report.status.success() {
        return Err(format!("report failed: {}", String::from_utf8_lossy(&report.stderr)));
    }
    let report = String::from_utf8(report.stdout).unwrap();
    let status_of = |name: &str| {
        report.lines().find_map(|l| {
            let mut it = l.split_whitespace().skip(1);
            let status = it.next()?;
            (it.next()? == name).then(|| status.to_string())
        })
    };
    for (tag, want) in [("native", "native"), ("interpreted", "interpreted")] {
        for name in directive(&src, tag) {
            let got = status_of(&name);
            if got.as_deref() != Some(want) {
                return Err(format!("expected `{name}` to be {want}, got {got:?}\n{report}"));
            }
        }
    }

    let stem = case.file_stem().unwrap().to_string_lossy();
    let flags_all = flags;
    let mut builds: Vec<(String, Vec<String>)> =
        backends.iter().map(|b| (b.clone(), vec!["--backend".to_string(), b.clone()])).collect();
    builds.push(("interp".into(), vec!["--no-native".into()]));
    let mut outputs = Vec::new();
    for (suffix, flags) in &builds {
        let exe = out_dir.join(format!("{stem}-{suffix}"));
        let b = luminous().arg("build").args(flags).args(&flags_all).arg(case).arg("-o").arg(&exe).output().unwrap();
        if !b.status.success() {
            return Err(format!("build ({suffix}) failed: {}", String::from_utf8_lossy(&b.stderr)));
        }
        let mut run = Command::new(&exe);
        if repo().join("build/quickjs-check/libqjs.a").exists() {
            // With the engine built with assertions: collect often, and check
            // that every object something holds is reached from the
            // collector's roots (see QJS_GC_VERIFY in the engine).
            run.env("QJS_GC_VERIFY", "1").env("QJS_GC_STRESS", "50");
        }
        for var in directive(&src, "env") {
            let (name, value) = var.split_once('=').unwrap_or((&var, ""));
            run.env(name, value);
        }
        let r = run.output().unwrap();
        outputs.push((
            r.status.code(),
            String::from_utf8_lossy(&r.stdout).into_owned(),
            String::from_utf8_lossy(&r.stderr).into_owned(),
        ));
    }
    let interp = outputs.last().unwrap();
    for ((backend, _), native) in builds.iter().zip(&outputs) {
        if native == interp {
            continue;
        }
        let diff: Vec<String> = native
            .1
            .lines()
            .zip(interp.1.lines())
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .take(5)
            .map(|(i, (a, b))| format!("line {}:\n  native: {a:.300}\n  interp: {b:.300}", i + 1))
            .collect();
        return Err(format!(
            "outputs differ with {backend} (exit {:?} vs {:?})\n{}\nnative stderr: {}\ninterp stderr: {}",
            native.0,
            interp.0,
            diff.join("\n"),
            native.2,
            interp.2
        ));
    }
    if interp.0 != Some(0) {
        return Err(format!("program failed in both modes: {}", interp.2));
    }
    Ok(())
}

#[test]
fn differential() {
    let out_dir = repo().join("build/tests");
    std::fs::create_dir_all(&out_dir).unwrap();
    let mut cases: Vec<PathBuf> = std::fs::read_dir(repo().join("tests/cases"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "ts"))
        .collect();
    cases.sort();
    let filter = std::env::var("LUMINOUS_CASE").ok();
    let backends = backends();
    let mut failures = Vec::new();
    for case in &cases {
        let name = case.file_name().unwrap().to_string_lossy().into_owned();
        if filter.as_ref().is_some_and(|f| !name.contains(f.as_str())) {
            continue;
        }
        match run_case(case, &out_dir, &backends) {
            Ok(()) => eprintln!("ok   {name}"),
            Err(e) => {
                eprintln!("FAIL {name}: {e}");
                failures.push(name);
            }
        }
    }
    assert!(failures.is_empty(), "failed cases: {failures:?}");
}

/// Every program in `tests/errors` must fail to compile, with diagnostics
/// containing each `// @error <text>` line.
#[test]
fn compile_errors() {
    let mut cases: Vec<PathBuf> = std::fs::read_dir(repo().join("tests/errors"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "ts"))
        .collect();
    cases.sort();
    let mut failures = Vec::new();
    for case in &cases {
        let name = case.file_name().unwrap().to_string_lossy().into_owned();
        let src = std::fs::read_to_string(case).unwrap();
        let expected = src.lines().filter_map(|l| l.strip_prefix("// @error ")).collect::<Vec<_>>();
        assert!(!expected.is_empty(), "{name}: no `// @error` lines");
        let flags = directive(&src, "flags");
        let out = luminous().arg("report").args(&flags).arg(case).output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        let missing: Vec<_> = expected.iter().filter(|e| !stderr.contains(*e)).collect();
        if out.status.success() || !missing.is_empty() {
            eprintln!("FAIL {name}: success={} missing={missing:?}\n{stderr}", out.status.success());
            failures.push(name);
        } else {
            eprintln!("ok   {name}");
        }
    }
    assert!(failures.is_empty(), "failed cases: {failures:?}");
}
