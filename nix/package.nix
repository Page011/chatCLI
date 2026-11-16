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
  cargoLock.lockFile = ../Cargo.lock;

  # nixpkgs' cargo setup hook sets LIBSQLITE3_SYS_USE_PKG_CONFIG=1, which links the system
  # sqlite instead of rusqlite's bundled copy. libdbus is vendored by keyring and needs nothing.
  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ sqlite ];
}
