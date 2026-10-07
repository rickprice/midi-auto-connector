let
  pkgs = import <nixpkgs> { };
in
pkgs.mkShell {
  buildInputs = [
    pkgs.alsa-lib
    pkgs.pipewire
    pkgs.pkg-config
    pkgs.llvmPackages.libclang
    pkgs.alsa-utils # aconnect, aseqdump -- handy for seeing/testing connections
  ];

  LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

  # bindgen (used to build the pipewire/alsa FFI bindings) invokes libclang
  # directly and doesn't pick up the glibc include paths the wrapped `cc`
  # normally injects, so they have to be passed explicitly.
  BINDGEN_EXTRA_CLANG_ARGS =
    builtins.readFile "${pkgs.stdenv.cc}/nix-support/libc-crt1-cflags"
    + " " + builtins.readFile "${pkgs.stdenv.cc}/nix-support/libc-cflags"
    + " " + builtins.readFile "${pkgs.stdenv.cc}/nix-support/cc-cflags";
}
