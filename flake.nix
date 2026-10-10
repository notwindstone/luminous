{
  description = "typescript-aot: strict TypeScript subset AOT-compiled with Cranelift, QuickJS-NG fallback";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        # clang as the C compiler/linker driver for QuickJS and the generated glue
        mkShell = pkgs.mkShell.override { stdenv = pkgs.clangStdenv; };
      in
      {
        devShells.default = mkShell {
          packages = with pkgs; [
            # Rust
            rustc
            cargo
            clippy
            rustfmt
            rust-analyzer
            # C side (QuickJS-NG build + linking compiled programs)
            cmake
            ninja
            gnumake
            # benchmarking / optional full strict type-check with tsc
            hyperfine
            typescript
          ];

          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";

          shellHook = ''
            export TSAOT_ROOT="$PWD"
            export TSAOT_QUICKJS_DIR="$PWD/build/quickjs-src"
            export TSAOT_QUICKJS_BUILD="$PWD/build/quickjs"
          '';
        };
      });
}
