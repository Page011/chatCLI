{
  lib,
  rustPlatform,
  pkg-config,
  sqlite,
}:

rustPlatform.buildRustPackage {
  pname = "chatcli";
  version = (lib.importTOML ../Cargo.toml).workspace.package.version;
  src = lib.cleanSource ../.;
  cargoLock = {
    lockFile = ../Cargo.lock;
    # Ratatui comes from git until its next release (see [patch.crates-io] in Cargo.toml). One
    # hash covers every crate from that repo and rev; update it when the rev changes.
    outputHashes."ratatui-0.30.2" = "sha256-sgfS2o17cT+Vr7wXSI0sZaPld2D96CyOcGVThDNyPHA=";
  };

  # nixpkgs' cargo setup hook sets LIBSQLITE3_SYS_USE_PKG_CONFIG=1, which links the system
  # sqlite instead of rusqlite's bundled copy. libdbus is vendored by keyring and needs nothing.
  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ sqlite ];
}
