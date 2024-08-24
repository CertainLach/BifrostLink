{
  description = "Bifrostlink";
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };
  outputs = {
    nixpkgs,
    flake-utils,
    rust-overlay,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [rust-overlay.overlays.default];
        };
        rust =
          (pkgs.rustChannelOf {
            date = "2024-08-20";
            channel = "nightly";
          })
          .default
          .override {
            extensions = ["rust-src" "miri" "rust-analyzer"];
          };
      in {
        devShell = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            rust
            cargo-edit
            cargo-asm
            cargo-outdated
            lld
            hyperfine
            valgrind
            kcachegrind
            graphviz
            cargo-release
            rustPlatform.bindgenHook
            pam
          ];
        };
      }
    );
}
