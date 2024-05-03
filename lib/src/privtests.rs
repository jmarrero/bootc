use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use camino::Utf8Path;
use cap_std_ext::cap_std;
use cap_std_ext::cap_std::fs::Dir;
use fn_error_context::context;
use rustix::fd::AsFd;
use xshell::{cmd, Shell};
use crate::blockdev::LoopbackDevice;
use crate::install::config::InstallConfiguration;

use super::cli::TestingOpts;
use super::spec::Host;

const IMGSIZE: u64 = 20 * 1024 * 1024 * 1024;

fn init_ostree(sh: &Shell, rootfs: &Utf8Path) -> Result<()> {
    cmd!(sh, "ostree admin init-fs --modern {rootfs}").run()?;
    Ok(())
}

#[context("bootc status")]
fn run_bootc_status() -> Result<()> {
    let sh = Shell::new()?;

    let mut tmpdisk = tempfile::NamedTempFile::new_in("/var/tmp")?;
    rustix::fs::ftruncate(tmpdisk.as_file_mut().as_fd(), IMGSIZE)?;
    let loopdev = LoopbackDevice::new(tmpdisk.path())?;
    let devpath = loopdev.path();
    println!("Using {devpath:?}");

    let td = tempfile::tempdir()?;
    let td = td.path();
    let td: &Utf8Path = td.try_into()?;

    cmd!(sh, "mkfs.xfs {devpath}").run()?;
    cmd!(sh, "mount {devpath} {td}").run()?;

    init_ostree(&sh, td)?;

    // Basic sanity test of `bootc status` on an uninitialized root
    let _g = sh.push_env("OSTREE_SYSROOT", td);
    cmd!(sh, "bootc status").run()?;

    Ok(())
}

// This needs nontrivial work for loopback devices
// #[context("bootc install")]
// fn run_bootc_install() -> Result<()> {
//     let sh = Shell::new()?;
//     let loopdev = LoopbackDevice::new_temp(&sh)?;
//     let devpath = &loopdev.dev;
//     println!("Using {devpath:?}");

//     let selinux_enabled = crate::lsm::selinux_enabled()?;
//     let selinux_opt = if selinux_enabled {
//         ""
//     } else {
//         "--disable-selinux"
//     };

//     cmd!(sh, "bootc install {selinux_opt} {devpath}").run()?;

//     Ok(())
// }

/// Tests run an ostree-based host
#[context("Privileged container tests")]
pub(crate) fn impl_run_host() -> Result<()> {
    run_bootc_status()?;
    println!("ok bootc status");
    //run_bootc_install()?;
    //println!("ok bootc install");
    println!("ok host privileged testing");
    Ok(())
}

#[context("Container tests")]
pub(crate) fn impl_run_container() -> Result<()> {
    let sh = Shell::new()?;
    let host: Host = serde_yaml::from_str(&cmd!(sh, "bootc status").read()?)?;
    assert!(matches!(host.status.ty, None));
    println!("ok status");

    for c in ["upgrade", "update"] {
        let o = Command::new("bootc").arg(c).output()?;
        let st = o.status;
        assert!(!st.success());
        let stderr = String::from_utf8(o.stderr)?;
        assert!(
            stderr.contains("This command requires full root privileges"),
            "stderr: {stderr}",
        );
    }
    println!("ok upgrade/update are errors in container");

    let o = Command::new("runuser")
        .args(["-u", "bin", "bootc", "upgrade"])
        .output()?;
    assert!(!o.status.success());
    let stderr = String::from_utf8(o.stderr)?;
    assert!(
        stderr.contains("requires root privileges"),
        "stderr: {stderr}"
    );

    let config = cmd!(sh, "bootc install print-configuration").read()?;
    let mut config: InstallConfiguration =
        serde_json::from_str(&config).context("Parsing install config")?;
    config.canonicalize();
    assert_eq!(
        config.root_fs_type.unwrap(),
        crate::install::baseline::Filesystem::Xfs
    );

    println!("ok container integration testing");
    Ok(())
}

#[context("Prep test install filesystem")]
fn prep_test_install_filesystem(blockdev: &Utf8Path) -> Result<tempfile::TempDir> {
    let sh = Shell::new()?;
    // Arbitrarily larger partition offsets
    let efipn = "5";
    let bootpn = "6";
    let rootpn = "7";
    let mountpoint_dir = tempfile::tempdir()?;
    let mountpoint: &Utf8Path = mountpoint_dir.path().try_into().unwrap();
    // Create the partition setup; we add some random empty partitions for 2,3,4 just to exercise things
    cmd!(
        sh,
        "sgdisk -Z {blockdev} -n 1:0:+1M -c 1:BIOS-BOOT -t 1:21686148-6449-6E6F-744E-656564454649 -n 2:0:+3M -n 3:0:+2M -n 4:0:+5M -n {efipn}:0:+127M -c {efipn}:EFI-SYSTEM -t ${efipn}:C12A7328-F81F-11D2-BA4B-00A0C93EC93B -n {bootpn}:0:+510M -c {bootpn}:boot -n {rootpn}:0:0 -c {rootpn}:root -t {rootpn}:0FC63DAF-8483-4772-8E79-3D69D8477DE4"
    )
    .run()?;
    // Create filesystems and mount
    cmd!(sh, "mkfs.ext4 {blockdev}{bootpn}").run()?;
    cmd!(sh, "mkfs.ext4 {blockdev}{rootpn}").run()?;
    cmd!(sh, "mkfs.fat {blockdev}{efipn}").run()?;
    cmd!(sh, "mount {blockdev}{rootpn} {mountpoint}").run()?;
    cmd!(sh, "mkdir {mountpoint}/boot").run()?;
    cmd!(sh, "mount {blockdev}{bootpn} {mountpoint}/boot").run()?;
    let efidir = crate::bootloader::EFI_DIR;
    cmd!(sh, "mkdir {mountpoint}/boot/{efidir}").run()?;
    cmd!(sh, "mount {blockdev}{efipn} {mountpoint}/boot/{efidir}").run()?;

    Ok(mountpoint_dir)
}

#[context("Test install filesystem")]
fn test_install_filesystem(image: &str, blockdev: &Utf8Path) -> Result<()> {
    let sh = Shell::new()?;

    let mountpoint_dir = prep_test_install_filesystem(blockdev)?;
    let mountpoint: &Utf8Path = mountpoint_dir.path().try_into().unwrap();

    // And run the install
    cmd!(sh, "podman run --rm --privileged --pid=host --env=RUST_LOG -v /usr/bin/bootc:/usr/bin/bootc -v {mountpoint}:/target-root {image} bootc install to-filesystem /target-root").run()?;

    cmd!(sh, "umount -R {mountpoint}").run()?;

    Ok(())
}

fn verify_selinux_label_exists(root: &Dir, path: &Path, warn: bool) -> Result<()> {
    let mut buf = [0u8; 1024];
    let fdpath = format!("/proc/self/fd/{}/", root.as_raw_fd());
    let fdpath = &Path::new(&fdpath).join(path);
    match rustix::fs::lgetxattr(fdpath, "security.selinux", &mut buf) {
        // Ignore EOPNOTSUPPORTED
        Ok(_) | Err(rustix::io::Errno::OPNOTSUPP) => Ok(()),
        Err(rustix::io::Errno::NODATA) if warn => {
            eprintln!("No SELinux label found for: {path:?}");
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("Failed to look up context for {path:?}")),
    }
}

fn verify_selinux_recurse(root: &Dir, path: &mut PathBuf, warn: bool) -> Result<()> {
    for ent in root.read_dir(&path)? {
        let ent = ent?;
        let name = ent.file_name();
        path.push(name);
        verify_selinux_label_exists(root, &path, warn)?;
        let file_type = ent.file_type()?;
        if file_type.is_dir() {
            verify_selinux_recurse(root, path, warn)?;
        }
        path.pop();
    }
    Ok(())
}

#[context("Container tests")]
fn test_container_lint(image: &str) -> Result<()> {

    let sh = Shell::new()?;

    // Smoke test of container --lint 
    let _test_1_result = cmd!(sh, "podman run --rm --privileged --pid=host --env=RUST_LOG -v /usr/bin/bootc:/usr/bin/bootc {image} bootc container --lint").run();

    // Setup for multiple kernels lint test
    cmd!(sh, "podman run -dt --name test --privileged --pid=host --env=RUST_LOG -v /usr/bin/bootc:/usr/bin/bootc {image} bash").run()?;
    let kernel_name = cmd!(sh, "podman exec test bash -c 'ls /usr/lib/modules | tail -n -1'" ).read()?;
    Command::new("podman")
    .arg("exec")
    .arg("test")
    .arg("bash")
    .arg("-c")
    .arg(format!("sudo cp -r /usr/lib/modules/{} /usr/lib/modules/delete-me", kernel_name))
    .output()?;
    let more_then_one_kernel_result = cmd!(sh, "podman exec test bash -c 'bootc container --lint'").read_stderr();
    // Container Cleanup 
    cmd!(sh, "podman rm -f test").run()?;

    _test_1_result?;    
    if let Err(e) = more_then_one_kernel_result {                                                                        
        assert!(e.to_string().contains("bootc container --lint"));                                                  
    } else {
        assert!(false, "Expected error, got none");     
    }
    Ok(())
}

pub(crate) async fn run(opts: TestingOpts) -> Result<()> {
    match opts {
        TestingOpts::RunPrivilegedIntegration {} => {
            crate::cli::ensure_self_unshared_mount_namespace().await?;
            tokio::task::spawn_blocking(impl_run_host).await?
        }
        TestingOpts::RunContainerIntegration {} => {
            tokio::task::spawn_blocking(impl_run_container).await?
        }
        TestingOpts::PrepTestInstallFilesystem { blockdev } => {
            tokio::task::spawn_blocking(move || prep_test_install_filesystem(&blockdev).map(|_| ()))
                .await?
        }
        TestingOpts::TestInstallFilesystem { image, blockdev } => {
            crate::cli::ensure_self_unshared_mount_namespace().await?;
            tokio::task::spawn_blocking(move || test_install_filesystem(&image, &blockdev)).await?
        }
        // This one is currently executed mainly from Github Actions
        TestingOpts::VerifySELinux { root, warn } => {
            let rootfs = cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())
                .context("Opening dir")?;
            let mut path = PathBuf::from(".");
            tokio::task::spawn_blocking(move || verify_selinux_recurse(&rootfs, &mut path, warn))
                .await?
        }
        TestingOpts::TestBuildLint { image } => {
            tokio::task::spawn_blocking(move || test_build_lint(&image)).await?
        }
        TestingOpts::TestContainerLint { image } => {
            tokio::task::spawn_blocking(move || test_container_lint(&image)).await?
        }
    }
}
