//! The embedding API (`EmbedOpts`): a prelude module, a module directory
//! (`demo:`), a native C module (`demo:native`), and the init and event loop
//! hooks, as a runtime built on Luminous uses them.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn embed() {
    let dir = repo().join("tests/embed");
    let out = std::env::temp_dir().join(format!("luminous-embed-{}", std::process::id()));
    std::fs::create_dir_all(&out).unwrap();
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let obj = out.join("native.o");
    let status = Command::new(cc)
        .args(["-O2", "-c", "-I"])
        .arg(repo().join("build/quickjs-src"))
        .arg(dir.join("native.c"))
        .arg("-o")
        .arg(&obj)
        .status()
        .unwrap();
    assert!(status.success(), "compiling native.c");
    let exe = out.join("app");
    let build = Command::new(env!("CARGO_BIN_EXE_luminous"))
        .arg("build")
        .arg(dir.join("app.ts"))
        .arg("-o")
        .arg(&exe)
        .arg("--prelude")
        .arg(dir.join("lib/prelude.ts"))
        .arg("--module-dir")
        .arg(format!("demo:={}", dir.join("lib").display()))
        .args(["--c-module", "demo:native=js_init_module_demo", "--init-fn", "demo_init", "--loop-fn", "demo_loop"])
        .arg("--link")
        .arg(&obj)
        .output()
        .unwrap();
    assert!(build.status.success(), "build: {}", String::from_utf8_lossy(&build.stderr));
    let run = Command::new(&exe).args(["a", "b"]).output().unwrap();
    let _ = std::fs::remove_dir_all(&out);
    assert!(run.status.success());
    assert_eq!(
        String::from_utf8_lossy(&run.stdout),
        "demo_init: 3 args\nprelude ran first\nhello, beyond (42)\nafter await\ndemo_loop: ran 0 jobs\n"
    );
}
