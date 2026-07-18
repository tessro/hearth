{
  description = "Hearth VM manager";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/b3da656039dc7a6240f27b2ef8cc6a3ef3bccae7";

  outputs =
    { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
      overlay = final: prev: {
        hearth-guest-kernel = final.callPackage ./nix/guest-kernel.nix { };
        hearth = final.callPackage ./nix/package.nix { };
      };
      packageSet = import nixpkgs {
        inherit system;
        overlays = [ overlay ];
      };
      moduleChecks = import ./nix/tests/module.nix {
        pkgs = packageSet;
        hearthModule = self.nixosModules.default;
      };
    in
    {
      overlays.default = overlay;

      packages.${system} = {
        hearth = packageSet.hearth;
        guest-kernel = packageSet.hearth-guest-kernel;
        default = packageSet.hearth;
      };

      nixosModules.default = { pkgs, ... }: {
        imports = [ ./nix/module.nix ];
        _module.args.hearthPackage = self.packages.${pkgs.stdenv.hostPlatform.system}.hearth;
        _module.args.hearthGuestKernel = self.packages.${pkgs.stdenv.hostPlatform.system}.guest-kernel;
      };

      checks.${system} = moduleChecks // {
        package = packageSet.hearth;
        guest-kernel = packageSet.hearth-guest-kernel;
      };
    };
}
