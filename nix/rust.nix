{ inputs, ... }:
{
  debug = true;
  perSystem =
    { inputs', self', ... }:
    let
      rust = inputs'.fenix.packages.fromManifestFile inputs.rust-manifest;
      toolchain = inputs'.fenix.packages.combine [
        rust.defaultToolchain
        (inputs'.fenix.packages.targets."aarch64-unknown-linux-gnu".fromManifestFile
          inputs.rust-manifest
        ).rust-std
        rust."clippy"
        rust."rust-docs"
        rust."rustfmt-preview"
        rust."clippy-preview"
      ];
    in
    {
      rust-project.crates."ec2autostop" = {
        imports = [ ];
        crane = {
          args = { };
        };
      };
      rust-project = {
        inherit toolchain;
      };
      packages.default = self'.packages.ec2autostop;
    };
}
