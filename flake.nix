{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/master";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      nixpkgs,
      rust-overlay,
      flake-utils,
      crane,
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

        lib = pkgs.lib;

        craneLib = (crane.mkLib pkgs).overrideToolchain rust-toolchain;

        # Link-time native dependencies for the GUI binary.
        nativeLibs = lib.optionals pkgs.stdenv.isLinux (
          with pkgs;
          [
            fontconfig
            freetype
            wayland
            libxkbcommon
            libGL
            libx11
            libxcb
            libxcursor
            libxrandr
            libxi
          ]
        );

        # Libraries gpui loads via `dlopen` at runtime; they are not recorded in
        # the binary, so the wrapper must place them on `LD_LIBRARY_PATH`.
        runtimeLibs = with pkgs; [
          wayland
          libxkbcommon
          libGL
          vulkan-loader
          libx11
          libxcb
          libxcursor
          libxrandr
          libxi
        ];

        # `cleanCargoSource` would drop the tree-sitter grammar C sources, the
        # `.scm` query files embedded via `include_str!`, and `config.stcfg`.
        # The unused `zed`/`iced-graph-editor` trees and the asset-only `vendor`
        # `.rs` files are excluded to keep the source closure small.
        src =
          let
            keepAsset =
              path:
              lib.hasSuffix ".c" path
              || lib.hasSuffix ".h" path
              || lib.hasSuffix ".scm" path
              || lib.hasSuffix ".stcfg" path
              || lib.hasSuffix "stoatignore" path;
          in
          lib.cleanSourceWith {
            name = "stoat-source";
            src = ./.;
            filter =
              path: type:
              let
                rel = lib.removePrefix (toString ./. + "/") path;
              in
              if
                lib.hasPrefix "target/" rel || lib.hasPrefix "zed/" rel || lib.hasPrefix "iced-graph-editor/" rel
              then
                false
              else if lib.hasPrefix "vendor/" rel then
                type == "directory" || keepAsset path
              else
                craneLib.filterCargoSources path type || keepAsset path;
          };

        commonArgs = {
          inherit src;
          pname = "stoat";
          version = "0.1.0";
          strictDeps = true;
          doCheck = false;
          cargoExtraArgs = "-p stoat_bin";
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = nativeLibs;
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        stoat = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
            nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ pkgs.makeWrapper ];
            postInstall = lib.optionalString pkgs.stdenv.isLinux ''
              wrapProgram $out/bin/stoat \
                --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath runtimeLibs}
            '';
          }
        );
      in
      {
        packages.default = stoat;

        devShell = pkgs.mkShell rec {
          buildInputs =
            with pkgs;
            [
              pkg-config
              binutils
              gcc
              rust-analyzer
              # using a hardcoded rustfmt version to support nightly rustfmt features.
              rust-bin.nightly."2026-05-28".rustfmt
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
