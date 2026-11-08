//! Renders the qemu argv for a microvm, kept apart from the supervisor that
//! spawns it so the command can be asserted in tests and printed by `--dry-run`.
//!
//! The vhost-user sockets reach qemu as inherited fds rather than paths, so the
//! fd numbering here and in `run`'s spawn loop must agree: share `i` is always
//! [`share_fd`]`(i)`.

use std::os::fd::RawFd;
use std::process::Command;
use std::str::FromStr;

/// First fd a child may be handed; 0-2 are std{in,out,err}.
pub const FIRST_CHILD_FD: RawFd = 3;

/// The fd number qemu sees for share `i`.
pub fn share_fd(i: usize) -> RawFd {
    FIRST_CHILD_FD + i as RawFd
}

/// A host directory shared into the guest over virtio-fs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedDir {
    pub dir: String,
    /// What the guest mounts: `mount -t virtiofs <tag> <mountpoint>`.
    pub tag: String,
    pub readonly: bool,
}

/// `<dir>:<tag>` or `<dir>:<tag>:ro`. Split from the right so a directory path
/// containing a colon still parses; `:ro` is only read as a modifier when a tag
/// would survive stripping it.
impl FromStr for SharedDir {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (rest, readonly) = match s.strip_suffix(":ro") {
            Some(rest) if rest.contains(':') => (rest, true),
            _ => (s, false),
        };
        let (dir, tag) = rest
            .rsplit_once(':')
            .ok_or_else(|| format!("expected <dir>:<tag>[:ro], got {s:?}"))?;
        if dir.is_empty() || tag.is_empty() {
            return Err(format!("neither <dir> nor <tag> may be empty, got {s:?}"));
        }
        Ok(SharedDir {
            dir: dir.to_string(),
            tag: tag.to_string(),
            readonly,
        })
    }
}

/// Everything the qemu argv is rendered from.
#[derive(Debug, Clone)]
pub struct QemuConfig {
    pub shared_dirs: Vec<SharedDir>,
    pub serial: String,
    pub memory: String,
    pub smp: usize,
    pub hostfwd: Vec<String>,
    pub image: String,
    pub kernel: String,
    pub initrd: String,
    pub append: String,
    pub seed: String,
}

impl QemuConfig {
    pub fn argv(&self) -> Vec<String> {
        let mut argv: Vec<String> = vec!["qemu-system-x86_64".into()];
        let mut push = |args: &[&str]| argv.extend(args.iter().map(|s| s.to_string()));

        // Machine type: as native as possible.
        push(&["-M", "microvm"]);

        // CPU: as native as possible.
        push(&["--enable-kvm", "-cpu", "host"]);
        push(&["-smp", &self.smp.to_string(), "-m", &self.memory]);

        // virtio-fs needs guest memory qemu can share with virtiofsd.
        push(&[
            "-object",
            &format!(
                "memory-backend-file,id=mem0,size={},mem-path=/dev/shm,share=on",
                self.memory
            ),
            "-numa",
            "node,memdev=mem0",
        ]);

        // A pty clients can attach to and detach from, symlinked at --serial.
        push(&["-nodefaults", "-no-user-config", "-nographic"]);
        push(&["-serial", &format!("pty:{}", self.serial)]);

        // microvm has no firmware, so the kernel and initrd are external.
        push(&["-kernel", &self.kernel]);
        push(&["-append", &self.append]);
        push(&["-initrd", &self.initrd]);

        // microvm has no PCI: virtio-mmio for every device.
        // snapshot=on keeps the verified image pristine.
        push(&["-device", "virtio-blk-device,drive=img0"]);
        push(&[
            "-drive",
            &format!(
                "file={},if=none,format=qcow2,id=img0,snapshot=on",
                self.image
            ),
        ]);
        push(&["-device", "virtio-blk-device,drive=img1"]);
        push(&[
            "-drive",
            &format!(
                "file={},if=none,format=raw,id=img1,readonly=on",
                self.seed
            ),
        ]);

        // One chardev + one device per filesystem share.
        for (i, share) in self.shared_dirs.iter().enumerate() {
            push(&[
                "-chardev",
                &format!("socket,id=vfs{i},fd={}", share_fd(i)),
            ]);
            push(&[
                "-device",
                &format!(
                    "vhost-user-fs-device,queue-size=1024,chardev=vfs{i},tag={}",
                    share.tag
                ),
            ]);
        }

        // Networking: `user` NAT, plus one hostfwd= clause per --hostfwd.
        let mut netdev = String::from("user,id=qemu0");
        for fwd in &self.hostfwd {
            netdev.push_str(",hostfwd=");
            netdev.push_str(fwd);
        }
        push(&["-device", "virtio-net-device,netdev=qemu0"]);
        push(&["-netdev", &netdev]);

        argv
    }

    pub fn command(&self) -> Command {
        let argv = self.argv();
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd
    }
}

/// Render an argv as a line that can be pasted into a shell.
pub fn shell_quote(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if !a.is_empty()
                && a.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,+@".contains(&b))
            {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(shares: &[&str]) -> QemuConfig {
        QemuConfig {
            shared_dirs: shares.iter().map(|s| s.parse().unwrap()).collect(),
            serial: "./serial.pty".into(),
            memory: "4G".into(),
            smp: 2,
            hostfwd: vec!["tcp::8080-:8080".into()],
            image: "./image.qcow2".into(),
            kernel: "./boot/kernel".into(),
            initrd: "./boot/initramfs.img".into(),
            append: "console=ttyS0 root=/dev/vda2".into(),
            seed: "./cloud-init/seed.iso".into(),
        }
    }

    /// Values of every occurrence of `flag`, e.g. all `-device` arguments.
    fn values_of(argv: &[String], flag: &str) -> Vec<String> {
        argv.windows(2)
            .filter(|w| w[0] == flag)
            .map(|w| w[1].clone())
            .collect()
    }

    fn vhost_fs_devices(argv: &[String]) -> Vec<String> {
        values_of(argv, "-device")
            .into_iter()
            .filter(|d| d.starts_with("vhost-user-fs-device"))
            .collect()
    }

    /// Properties of a comma-separated qemu argument, minus the leading type.
    fn props(arg: &str) -> Vec<(String, String)> {
        arg.split(',')
            .filter_map(|p| p.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn prop(arg: &str, key: &str) -> Option<String> {
        props(arg).into_iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    #[test]
    fn chardev_ids_resolve() {
        let argv = config(&["/var/db/repos:repos", "./share:agent"]).argv();
        let ids: Vec<String> = values_of(&argv, "-chardev")
            .iter()
            .filter_map(|c| prop(c, "id"))
            .collect();
        assert_eq!(ids, ["vfs0", "vfs1"]);
        for device in vhost_fs_devices(&argv) {
            let referenced = prop(&device, "chardev").expect("device has no chardev=");
            assert!(
                ids.contains(&referenced),
                "{device} references chardev {referenced}, which is never defined: {ids:?}"
            );
        }
    }

    /// `vhost-user-fs-device` has no `fd` property; the fd lives on the chardev.
    #[test]
    fn devices_carry_no_fd_property() {
        let argv = config(&["./share:agent"]).argv();
        for device in vhost_fs_devices(&argv) {
            assert!(prop(&device, "fd").is_none(), "{device} has an fd= property");
        }
    }

    /// qemu ignores virtiofsd's --tag, so the tag has to be on the device or the
    /// guest's `mount -t virtiofs <tag>` can never match.
    #[test]
    fn devices_carry_the_tag() {
        let argv = config(&["/var/db/repos:repos:ro", "./share:agent"]).argv();
        let tags: Vec<Option<String>> = vhost_fs_devices(&argv)
            .iter()
            .map(|d| prop(d, "tag"))
            .collect();
        assert_eq!(
            tags,
            [Some("repos".to_string()), Some("agent".to_string())]
        );
    }

    #[test]
    fn share_fds_are_distinct_and_inheritable() {
        let argv = config(&["a:one", "b:two", "c:three"]).argv();
        let fds: Vec<RawFd> = values_of(&argv, "-chardev")
            .iter()
            .filter_map(|c| prop(c, "fd"))
            .map(|f| f.parse().unwrap())
            .collect();
        assert_eq!(fds, [3, 4, 5]);
        assert_eq!(fds.len(), vhost_fs_devices(&argv).len());
    }

    /// qemu rejects `-nodefault`: it is `-nodefaults`.
    #[test]
    fn nodefaults_is_spelled_correctly() {
        let argv = config(&[]).argv();
        assert!(argv.contains(&"-nodefaults".to_string()));
        assert!(!argv.contains(&"-nodefault".to_string()));
    }

    /// Every id qemu is handed has to satisfy its identifier grammar.
    #[test]
    fn generated_ids_match_qemu_grammar() {
        let argv = config(&["./share:my/odd.tag"]).argv();
        let ids: Vec<String> = ["-chardev", "-object", "-netdev", "-drive"]
            .iter()
            .flat_map(|flag| values_of(&argv, flag))
            .filter_map(|a| prop(&a, "id"))
            .collect();
        assert!(!ids.is_empty());
        for id in ids {
            let mut chars = id.chars();
            assert!(
                chars.next().is_some_and(|c| c.is_ascii_alphabetic())
                    && chars.all(|c| c.is_ascii_alphanumeric() || "-._".contains(c)),
                "{id:?} is not a qemu identifier"
            );
        }
    }

    #[test]
    fn hostfwd_clauses_land_on_the_netdev() {
        let mut cfg = config(&[]);
        cfg.hostfwd = vec!["tcp::8080-:8080".into(), "tcp::2222-:22".into()];
        let argv = cfg.argv();
        assert_eq!(
            values_of(&argv, "-netdev"),
            ["user,id=qemu0,hostfwd=tcp::8080-:8080,hostfwd=tcp::2222-:22"]
        );
    }

    #[test]
    fn shared_dir_parsing() {
        assert_eq!(
            "./share:agent".parse::<SharedDir>().unwrap(),
            SharedDir { dir: "./share".into(), tag: "agent".into(), readonly: false }
        );
        assert_eq!(
            "/var/db/repos:repos:ro".parse::<SharedDir>().unwrap(),
            SharedDir { dir: "/var/db/repos".into(), tag: "repos".into(), readonly: true }
        );
        // A colon in the path is fine; the tag is taken from the right.
        assert_eq!(
            "/srv/a:b:tag".parse::<SharedDir>().unwrap(),
            SharedDir { dir: "/srv/a:b".into(), tag: "tag".into(), readonly: false }
        );
        // Stripping ":ro" here would leave no tag, so it is the tag.
        assert_eq!(
            "./share:ro".parse::<SharedDir>().unwrap(),
            SharedDir { dir: "./share".into(), tag: "ro".into(), readonly: false }
        );
        assert!("./share".parse::<SharedDir>().is_err());
        assert!("./share:".parse::<SharedDir>().is_err());
    }

    #[test]
    fn shell_quote_round_trips_qemu_args() {
        let quoted = shell_quote(&config(&[]).argv());
        // -append holds spaces and must come back as one shell word.
        assert!(quoted.contains("-append 'console=ttyS0 root=/dev/vda2'"));
        // Plain values stay bare.
        assert!(quoted.contains("-M microvm"));
    }
}
