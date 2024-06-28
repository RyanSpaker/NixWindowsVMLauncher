{
  description = "Windows VM launcher for NixOS system";
  inputs = {
    flake-utils.url = "github:numtide/flake-utils";
    fenix.url = "github:nix-community/fenix";
  };
  outputs = { self, nixpkgs, flake-utils, fenix, ...}:
    flake-utils.lib.eachDefaultSystem (
      system: 
      let
        pkgs = import nixpkgs {inherit system; config.allowUnfree = true; };
        rust-toolchain = fenix.packages.${system}.latest;
      in
      {
        devShells.default = with pkgs; mkShell rec {
          nativeBuildInputs = [
            pkg-config
          ];
          buildInputs = [
            (rust-toolchain.withComponents [
              "cargo"
              "clippy"
              "rust-src"
              "rustc"
              "rustfmt"
            ])
            pkg-config
            cargo-udeps
            git
            dbus
            libinput
            rustc.llvmPackages.clang
            rustc.llvmPackages.bintools
            (wrapBintoolsWith { bintools = mold; })
          ];
          LIBCLANG_PATH = lib.makeLibraryPath [ rustc.llvmPackages.libclang.lib ];
          LD_LIBRARY_PATH = lib.makeLibraryPath buildInputs;
          RUST_SRC_PATH = "${rust-toolchain.rust-src}/lib/rustlib/src/rust/library";
          PATH = "${rust-toolchain.cargo}/bin";
          RUSTFLAGS = "-C link-arg=-fuse-ld=mold -C linker=clang -Zshare-generics=y";
        };
        packages.default = (pkgs.makeRustPlatform {
          cargo = rust-toolchain.toolchain;
          rustc = rust-toolchain.toolchain;
        }).buildRustPackage rec {
          pname = "nixos-windows-launcher";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = with pkgs; [
            pkg-config
            makeWrapper
          ];
          buildInputs = with pkgs; [
            pkg-config
            cargo-udeps
            git
            dbus
            libinput
            rustc.llvmPackages.clang
            rustc.llvmPackages.bintools
            (wrapBintoolsWith { bintools = mold; })
          ];
          libraries = pkgs.lib.makeLibraryPath [pkgs.libinput pkgs.dbus];
          postInstall = ''
            mv $out/bin/nixos-windows-launcher $out/bin/.nixos-windows-launcher
            makeWrapper $out/bin/.nixos-windows-launcher $out/bin/nixos-windows-launcher --set LD_LIBRARY_PATH ${libraries}
            mkdir -p $out/share/dbus-1/system.d
            mkdir -p $out/share/dbus-1/session.d
            cp ${src}/dbus.conf $out/share/dbus-1/system.d/org.cowsociety.vmlauncher.conf
            cp ${src}/dbus_session.conf $out/share/dbus-1/session.d/org.cowsociety.vmlauncher.conf
          '';
        };
      }
    );
}