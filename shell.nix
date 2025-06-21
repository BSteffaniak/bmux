{
  pkgs ? import <nixpkgs> { },
}:

pkgs.mkShell {
  buildInputs = with pkgs; [
    # Rust toolchain
    rustc
    cargo
    rustfmt
    clippy

    # Development tools
    nodePackages.markdownlint-cli

    # System dependencies that might be needed for Rust crates
    pkg-config
    openssl
  ];

  shellHook = ''
    echo "🦀 bmux development environment loaded!"
    echo "Available tools:"
    echo "  - cargo ($(cargo --version))"
    echo "  - rustc ($(rustc --version))"
    echo "  - clippy ($(cargo clippy --version))"
    echo "  - markdownlint ($(markdownlint --version))"
    echo ""
    echo "Run 'markdownlint *.md **/*.md' to lint all markdown files"
    echo "Run 'markdownlint --fix *.md **/*.md' to auto-fix markdown issues"
    echo "Run 'cargo clippy --all-targets -- -D warnings' for strict linting"
  '';
}
