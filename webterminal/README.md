# Web Terminal VM

Inspired by [SadServers](https://github.com/SadServers/sadservers) Scenario VMs.

This example is otherwise the same as ../gentoo, but an agent is running in
the guest that serves a web terminal for clients to access the instance.

- **`agent`** is a standalone rust binary, run inside the guest:

  - `GET /` &rarr; an xterm.js browser terminal ("webssh")
  - `GET /ws` &rarr; WebSocket bridged to a login shell in a PTY

## Usage

1. **Extract the kernel + initramfs** (microvm boots them directly):

       ./extract-boot.sh

2. **Compile the agent** into the virtiofs staging dir (`share/`):

       cargo +nightly -Z script -Z unstable-options build --release \
           --artifact-dir ./share --manifest-path ./agent/agent.rs

3. **Build the cloud-init seed**: setup cloud-init:

       ( cd cloud-init && ./build.sh )

4. **Share the agent over virtiofs**:

       ./run-virtiofsd.sh &

    You can also use this directory to copy files out of the guest.

5. **Boot the VM** (another shell):

       ./run-microvm.sh

6a. **Use the web console**:

        xdg-open http://localhost:8080/

6b. **Use the serial console**:

        minicom -D serial.pty
