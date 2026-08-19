
# Stoat

**Development Status**: Exploration / Prototyping

A Stoat-ally different editor.

## Stoatty

Stoat ships with `stoatty`, a GPU-accelerated terminal that renders more than
cells: a program inside it can ask for panels, scaled text, minimaps, and
smooth-scrolling page pools, and the same program still runs anywhere else.

[docs/stoatty-protocol.md](docs/stoatty-protocol.md) is the reference for
speaking that protocol from your own program.

## Install (NixOS)

Add the flake as an input:

```nix
inputs.stoat.url = "github:leeola/stoat";
```

Then install the terminal:

```nix
environment.systemPackages = [ inputs.stoat.packages.${pkgs.system}.stoatty ];
```

The `stoatty` package carries the `stoat` binary too, and installs a desktop
entry, so the terminal appears in the Plasma launcher under its own icon.

For a headless machine that wants the editor alone, install
`inputs.stoat.packages.${pkgs.system}.stoat` instead.
