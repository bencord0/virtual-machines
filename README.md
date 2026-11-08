# Virtual Machines


Here are some experiments to run virtual machines with gentoo.
See the individual README.md files in each directory for usage.

The boot/run tooling is a shared Rust crate (`vm`) at the repo root:

- `src/bin/fetch.rs`  -- download + PGP-verify a Gentoo cloud image and extract its boot, in one step
- `src/bin/run.rs`  -- supervise virtiofsd + qemu to boot a microvm
