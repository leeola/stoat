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
              rust-bin.nightly."2025-06-26".rustfmt
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
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
              darwin.apple_sdk.frameworks.Security
              darwin.apple_sdk.frameworks.Foundation
              darwin.apple_sdk.frameworks.Cocoa
              darwin.apple_sdk.frameworks.Carbon
              darwin.apple_sdk.frameworks.WebKit
            ];

          # Library path for GUI applications (especially Wayland/iced)
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
        };
      }
    );
}
