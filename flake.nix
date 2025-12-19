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
