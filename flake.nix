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
        devShell = pkgs.mkShell (
          rec {
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

            # difftastic line-based diffing for TUI snapshots.
            DFT_OVERRIDE = "stoat/src/snapshots/tui/*.snap:text";
          }
          // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
            # Lavapipe, Mesa's software rasterizer. The renderer's headless
            # tests ask Vulkan for an adapter and skip themselves when none
            # answers, so on a machine with no GPU driver they report green
            # having built no pipeline and drawn no pixel.
            #
            # Registered through the loader's additive variable rather than
            # VK_ICD_FILENAMES, which replaces the driver list outright. This
            # shell also runs the real application, and pinning it to a
            # software rasterizer would make that render in software on a
            # machine that has a GPU.
            VK_ADD_DRIVER_FILES = "${pkgs.mesa}/share/vulkan/icd.d/lvp_icd.${pkgs.stdenv.hostPlatform.parsed.cpu.name}.json";
          }
        );
      }
    );
}
