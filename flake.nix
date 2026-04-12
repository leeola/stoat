{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
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
              rust-bin.nightly."2025-03-05".rustfmt
              rust-toolchain

            ]
            ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
              # GUI runtime libraries
              wayland
              libxkbcommon
              libGL
              # X11 fallback libraries
              xorg.libX11
              xorg.libXcursor
              xorg.libXrandr
              xorg.libXi
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
              pkgs.xorg.libX11
              pkgs.xorg.libXcursor
              pkgs.xorg.libXrandr
              pkgs.xorg.libXi
            ]
          );

          # Silence nixpkgs cc-wrapper's target-mismatch warning emitted
          # when Rust's `cc` crate canonicalizes Apple triples before
          # invoking clang (e.g. `aarch64-apple-darwin` -> `arm64-apple-macosx`).
          NIX_CC_WRAPPER_SUPPRESS_TARGET_WARNING = "1";
        };
      }
    );
}
