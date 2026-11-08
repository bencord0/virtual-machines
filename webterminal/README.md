# Web Terminal VM

Inspired by [SadServers](https://github.com/SadServers/sadservers) Scenario VMs.

This example is otherwise the same as ../gentoo, but an agent is running in
the guest that serves a web terminal for clients to access the instance.

- **`agent`** is a standalone rust binary, run inside the guest:

  - `GET /` &rarr; an xterm.js browser terminal ("webssh")
  - `GET /ws` &rarr; WebSocket bridged to a login shell in a PTY

The boot/run tooling is shared with the other VMs: it lives in the `vm`
crate at the repo root (`../src`) and is invoked through the thin wrapper scripts
here. The per-VM differences are just CLI flags (see `run.sh`): the virtiofs
share (`--shared-dir ./share:agent`) and the host&rarr;guest port forwards for
the web (8080) and ssh (2222&rarr;22) ports.

## Usage

1. **Fetch and snapshot.** This VM boots the same image as ../gentoo.

        ./fetch.sh

2. **Compile the agent** into the virtiofs staging dir (`share/`):

       cargo +nightly -Z script -Z unstable-options build --release \
           --artifact-dir ./share --manifest-path ./agent/agent.rs

3. **Boot the VM.** `run.sh` supervises virtiofsd + qemu as one process tree
   (the agent share is passed to qemu)

       ./run.sh

4a. **Use the web console**:

        xdg-open http://localhost:8080/

4b. **Use the serial console**:

        minicom -D serial.pty
