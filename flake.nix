{
  description = "Rust implementation of headless Silicon Heaven applications";

  outputs = {
    self,
    systems,
    nixpkgs,
  }: let
    inherit (builtins) head match readFile;
    inherit (nixpkgs.lib) genAttrs;
    forSystems = genAttrs (import systems);
    withPkgs = func: forSystems (system: func self.legacyPackages.${system});

    package = {rustPlatform}:
      rustPlatform.buildRustPackage {
        pname = "shvcall-rs";
        version = head (
          match ".*\nversion[ ]*=[ ]*\"([^\"]+)\"\n.*" (readFile ./Cargo.toml)
        );

        src = ./.;
        cargoLock = {
          lockFile = ./Cargo.lock;
          outputHashes = {
            "libshvproto-macros-0.2.0" = "sha256-A66OGwzA5Uc8p0P2Bv51jW2dAxMZsA+nXhreV0NCIxQ=";
            "shvrpc-3.11.3" = "sha256-v6flcHHGL/cpmQoWWuHj/DQSiD/A+QlNpCBz0Pi0VJk=";
          };
        };
        doCheck = true;

        meta = with nixpkgs.lib; {
          description = "CLI utility to invoke remote SHV RPC calls";
          homepage = "https://github.com/silicon-heaven/shvcall-rs/";
          license = licenses.mit;
          mainProgram = "shvcall";
        };
      };
  in {
    overlays.default = final: _: {
      shvcall-rs = final.callPackage package {};
    };

    packages = withPkgs (pkgs: {default = pkgs.shvcall-rs;});

    legacyPackages =
      forSystems (system:
        nixpkgs.legacyPackages.${system}.extend self.overlays.default);

    formatter = withPkgs (pkgs: pkgs.alejandra);
  };
}
