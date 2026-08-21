{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/master";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
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

        rustPlatform = pkgs.makeRustPlatform {
          cargo = rust-toolchain;
          rustc = rust-toolchain;
        };

        # winit and wgpu dlopen these at runtime, so nothing links them at
        # build time and an RPATH on the installed binary is what finds them.
        runtimeLibs = with pkgs; [
          wayland
          libxkbcommon
          libGL
          vulkan-loader
          xorg.libX11
          xorg.libXcursor
          xorg.libXrandr
          xorg.libXi
        ];

        # pkg-config and zlib serve libgit2-sys and libz-sys, the workspace's
        # only crates that link native code.
        commonPackage = {
          version = "0.1.0";
          # A git flake's source holds only tracked and staged files, so
          # `target/` and the untracked `.cargo/config.toml` (whose job limit
          # would throttle a Nix build) never reach the store.
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.zlib ];
          # The four project commands own testing, and the workspace tests need
          # the devshell's software-rasterizer setup.
          doCheck = false;
          # Silence nixpkgs cc-wrapper's target-mismatch warning emitted
          # when Rust's `cc` crate canonicalizes Apple triples before
          # invoking clang (e.g. `aarch64-apple-darwin` -> `arm64-apple-macosx`).
          NIX_CC_WRAPPER_SUPPRESS_TARGET_WARNING = "1";
        };

        # The macOS icon, rendered from the same SVG the Linux desktop entry
        # names so the two platforms never drift apart. A separate derivation
        # keeps an icon edit from rebuilding the Rust workspace.
        stoattyIcns =
          pkgs.runCommand "stoatty.icns"
            {
              nativeBuildInputs = [
                pkgs.resvg
                pkgs.libicns
              ];
            }
            ''
              for size in 16 32 128 256 512; do
                resvg -w $size -h $size ${./assets/stoatty.svg} icon_$size.png
              done
              png2icns $out icon_16.png icon_32.png icon_128.png icon_256.png icon_512.png
            '';

        packages = rec {
          stoat = rustPlatform.buildRustPackage (
            commonPackage
            // {
              pname = "stoat";
              cargoBuildFlags = [
                "-p"
                "stoat_bin"
              ];
              meta = {
                description = "The stoat editor CLI";
                mainProgram = "stoat";
              };
            }
          );

          # Both binaries in one derivation. stoatty resolves `stoat` as a
          # sibling of its own executable before consulting PATH, so shipping
          # them together needs no wrapper and cannot skew versions.
          stoatty = rustPlatform.buildRustPackage (
            commonPackage
            // {
              pname = "stoatty";
              cargoBuildFlags = [
                "-p"
                "stoatty"
                "-p"
                "stoat_bin"
              ];

              # Linux gets a desktop entry, macOS an app bundle. The bundle's
              # MacOS directory links to bin/ rather than holding a copy, so a
              # Finder launch runs the same file as a shell launch and the
              # sibling `stoat` lookup still lands next to it.
              postInstall =
                if pkgs.stdenv.hostPlatform.isDarwin then
                  ''
                    app=$out/Applications/Stoatty.app/Contents
                    install -Dm444 assets/Info.plist $app/Info.plist
                    install -Dm444 ${stoattyIcns} $app/Resources/stoatty.icns
                    ln -s $out/bin $app/MacOS
                  ''
                else
                  ''
                    install -Dm444 assets/stoatty.desktop $out/share/applications/stoatty.desktop
                    install -Dm444 assets/stoatty.svg $out/share/icons/hicolor/scalable/apps/stoatty.svg
                  '';

              # An RPATH rather than a wrapper that sets a library path.
              # stoatty spawns shells, and a wrapper's environment leaks into
              # every child process the user starts from one.
              postFixup = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
                patchelf --add-rpath "${pkgs.lib.makeLibraryPath runtimeLibs}" $out/bin/stoatty
              '';

              meta = {
                description = "GPU terminal hosting the stoat editor";
                mainProgram = "stoatty";
              };
            }
          );

          default = stoatty;
        };
      in
      {
        inherit packages;

        apps = {
          stoat = {
            type = "app";
            program = "${packages.stoat}/bin/stoat";
          };
          stoatty = {
            type = "app";
            program = "${packages.stoatty}/bin/stoatty";
          };
        };

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
                # Drives the feature-powerset compile check in scripts/check-features.sh.
                cargo-hack
                # A color emoji face for the terminal to fall back to. The
                # bundled faces cover text and symbols but carry no emoji, and
                # the fallback only finds an installed one.
                noto-fonts-color-emoji

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

            # Point fontconfig at the emoji face above. It adds to the standard
            # font directories rather than replacing them, so everything else
            # the machine has stays visible.
            FONTCONFIG_FILE = pkgs.makeFontsConf {
              fontDirectories = [ pkgs.noto-fonts-color-emoji ];
            };

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
