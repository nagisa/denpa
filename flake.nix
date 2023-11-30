{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }: flake-utils.lib.eachDefaultSystem (system:
    let pkgs = nixpkgs.legacyPackages.${system};
    in {
      devShell = pkgs.mkShell {
        buildInputs = (with pkgs; [
          rustup pkg-config gdb rustfmt openssl glib
        ]) ++ (with pkgs.gst_all_1; [
          gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad
        ]);
      };
    });
}
