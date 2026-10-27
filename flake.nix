{
  description = "GPUI offscreen display for the Soeyi CH1L USB screen, fed by Spotify/Spicetify";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
    in
    {
      packages = nixpkgs.lib.genAttrs systems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          dida-box = pkgs.callPackage ./package.nix { };
        in
        {
          inherit dida-box;
          default = dida-box;
        }
      );
    };
}
