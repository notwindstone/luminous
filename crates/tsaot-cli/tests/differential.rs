//! Differential tests: every case in `tests/cases` is built twice, with native
//! compilation and fully interpreted, and both executables must print exactly
//! the same output. `// @native a b` and `// @interpreted c` header lines pin
//! down which functions the checker is expected to compile natively.

use std::path::{Path, PathBuf};
use std::process::Command;

fn tsaot() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tsaot"))
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

fn run_case(case: &Path, out_dir: &Path) -> Result<(), String> {
    let src = std::fs::read_to_string(case).unwrap();
    let report = tsaot().arg("report").arg(case).output().unwrap();
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
    let mut outputs = Vec::new();
    for (suffix, flags) in [("native", &[][..]), ("interp", &["--no-native"][..])] {
        let exe = out_dir.join(format!("{stem}-{suffix}"));
        let b = tsaot().arg("build").args(flags).arg(case).arg("-o").arg(&exe).output().unwrap();
        if !b.status.success() {
            return Err(format!("build ({suffix}) failed: {}", String::from_utf8_lossy(&b.stderr)));
        }
        let r = Command::new(&exe).output().unwrap();
        outputs.push((
            r.status.code(),
            String::from_utf8_lossy(&r.stdout).into_owned(),
            String::from_utf8_lossy(&r.stderr).into_owned(),
        ));
    }
    let (native, interp) = (&outputs[0], &outputs[1]);
    if native != interp {
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
            "outputs differ (exit {:?} vs {:?})\n{}\nnative stderr: {}\ninterp stderr: {}",
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
    let filter = std::env::var("TSAOT_CASE").ok();
    let mut failures = Vec::new();
    for case in &cases {
        let name = case.file_name().unwrap().to_string_lossy().into_owned();
        if filter.as_ref().is_some_and(|f| !name.contains(f.as_str())) {
            continue;
        }
        match run_case(case, &out_dir) {
            Ok(()) => eprintln!("ok   {name}"),
            Err(e) => {
                eprintln!("FAIL {name}: {e}");
                failures.push(name);
            }
        }
    }
    assert!(failures.is_empty(), "failed cases: {failures:?}");
}
