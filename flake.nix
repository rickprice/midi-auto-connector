{
  description = "Dev shell for midi-auto-connector (provides ALSA + PipeWire + clang headers for native bindings)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs, ... }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ];
    in
    {
      devShells = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.clang
            ];

            buildInputs = [
              pkgs.alsa-lib
              pkgs.pipewire
            ];

            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

            # bindgen (used to build the pipewire/alsa FFI bindings) invokes
            # libclang directly and doesn't pick up the glibc include paths
            # the wrapped `cc` normally injects, so they have to be passed
            # explicitly -- without this, builds fail with "'inttypes.h'
            # file not found" or similar from inside bindgen.
            BINDGEN_EXTRA_CLANG_ARGS =
              builtins.readFile "${pkgs.stdenv.cc}/nix-support/libc-crt1-cflags"
              + " " + builtins.readFile "${pkgs.stdenv.cc}/nix-support/libc-cflags"
              + " " + builtins.readFile "${pkgs.stdenv.cc}/nix-support/cc-cflags";

            shellHook = ''
              echo "midi-auto-connector dev shell: alsa-lib + pipewire + pkg-config on PATH."
            '';
          };
        });
    };
}
