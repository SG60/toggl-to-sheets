{
  pkgs ? import <nixpkgs> { },
  devshell ?
    import
      (fetchTarball "https://github.com/numtide/devshell/archive/7c9e793ebe66bcba8292989a68c0419b737a22a0.tar.gz")
      { },
}:

with pkgs;

devshell.mkShell { packages = [ cargo-release ]; }
