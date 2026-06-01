{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/master";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      nixpkgs,
      rust-overlay,
      flake-utils,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        rust-toolchain = (pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml).override {
          extensions = [ "rust-analysis" ];
        };
      in
      {
        devShell = pkgs.mkShell rec {
          buildInputs =
            with pkgs;
            [
              pkg-config
              binutils
              gcc
              rust-analyzer
              # using a hardcoded rustfmt version to support nightly rustfmt features.
              rust-bin.nightly."2026-04-16".rustfmt
              rust-toolchain

            ]
            ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
              # GUI runtime libraries
              wayland
              libxkbcommon
              libGL
              # X11 fallback libraries
              libx11
              libxcb
              libxcursor
              libxrandr
              libxi
            ];

          # Library path for GUI applications
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (
            buildInputs
            ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
              # Wayland runtime libraries
              pkgs.wayland
              pkgs.libxkbcommon
              pkgs.libGL
              pkgs.vulkan-loader
              # X11 fallback libraries
              pkgs.libx11
              pkgs.libxcb
              pkgs.libxcursor
              pkgs.libxrandr
              pkgs.libxi
            ]
          );

          # Silence nixpkgs cc-wrapper's target-mismatch warning emitted
          # when Rust's `cc` crate canonicalizes Apple triples before
          # invoking clang (e.g. `aarch64-apple-darwin` -> `arm64-apple-macosx`).
          NIX_CC_WRAPPER_SUPPRESS_TARGET_WARNING = "1";

          # difftastic line-based diffing for TUI snapshots.
          DFT_OVERRIDE = "stoat/src/snapshots/tui/*.snap:text";
        };
      }
    );
}
