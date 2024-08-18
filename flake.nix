{
  inputs = {
    nixpkgs.url      = "github:nixos/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url  = "github:numtide/flake-utils";
  };

  outputs = { nixpkgs, rust-overlay, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        rust-toolchain = (pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain).override {
          extensions = [ "rust-analysis" ];
        };
      in
      {
        devShell = pkgs.mkShell rec {
          buildInputs = with pkgs; [
            pkg-config
            binutils
            gcc
            rust-analyzer
            # using a hardcoded rustfmt version to support nightly rustfmt features.
            rust-bin.nightly."2024-08-08".rustfmt
            rust-toolchain

          ] ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
            # Some dependencies needed for GPUI examples. Found through dumb trial and error.
            clang
            mold
            cmake
            openssl
            xorg.libxcb
            libxkbcommon
            vulkan-headers
            vulkan-loader
            vulkan-tools
            vulkan-validation-layers

          ] ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
            darwin.apple_sdk.frameworks.Security
            darwin.apple_sdk.frameworks.Foundation
            darwin.apple_sdk.frameworks.Cocoa
            darwin.apple_sdk.frameworks.Carbon
            darwin.apple_sdk.frameworks.WebKit
            darwin.apple_sdk.frameworks.CoreVideo
            darwin.apple_sdk.frameworks.IOKit
            darwin.apple_sdk.frameworks.Metal
            darwin.apple_sdk.frameworks.MetalKit
          ];

          # Seems necessary to make libxcb found. Again for GPUI.
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath buildInputs;
        };
      }
    );
}
