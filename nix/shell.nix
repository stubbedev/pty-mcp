{
  perSystem = { pkgs, ... }: {
    devShells.default = pkgs.mkShell {
      packages = with pkgs; [
        cargo
        rustc
        clippy
        rustfmt
        rust-analyzer
        just
        git
      ];
      shellHook = ''
        echo "pty-mcp dev shell — \`just build\` to compile, \`just test\` to test"
      '';
    };

    formatter = pkgs.nixpkgs-fmt;
  };
}
