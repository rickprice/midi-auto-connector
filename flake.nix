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

            shellHook = ''
              echo "midi-auto-connector dev shell: alsa-lib + pipewire + pkg-config on PATH."
            '';
          };
        });
    };
}
