{
  mkShell,
  rustc,
  cargo,
  rustfmt,
  clippy,
  taplo,
  cargo-nextest,
}:
mkShell {
  name = "rust";

  strictDeps = true;
  nativeBuildInputs = [
    rustc
    cargo

    # Tools
    (rustfmt.override {asNightly = true;})
    clippy
    taplo

    # Additional Cargo Tooling
    cargo-nextest
  ];
}
