{
  description = "BodhiSpace AI SRE development environment";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];

      forAllSystems = function:
        nixpkgs.lib.genAttrs systems (system: function nixpkgs.legacyPackages.${system});
    in
    {
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShellNoCC {
          packages = with pkgs; [
            cargo-deny
            cargo-nextest
            gnumake
            rustup
          ];

          shellHook = ''
            export RUST_BACKTRACE="1"
            export CARGO_INCREMENTAL="0"
            export CARGO_TERM_COLOR="always"
          '';
        };
      });
    };
}
