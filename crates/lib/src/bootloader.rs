use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use bootc_utils::CommandRunExt;
use camino::Utf8Path;
use cap_std_ext::cap_std::fs::Dir;
use cap_std_ext::dirext::CapStdExtDirExt;
use fn_error_context::context;

use bootc_blockdev::PartitionTable;
use bootc_mount as mount;

/// The name of the mountpoint for efi (as a subdirectory of /boot, or at the toplevel)
pub(crate) const EFI_DIR: &str = "efi";

/// Get the backing device for a filesystem at the given path
fn get_backing_device(root_path: &Utf8Path) -> Result<String> {
    let fsinfo = mount::inspect_filesystem(root_path)?;
    let mut dev = fsinfo.source;
    loop {
        let mut parents = bootc_blockdev::find_parent_devices(&dev)?.into_iter();
        let Some(parent) = parents.next() else {
            break;
        };
        dev = parent;
    }
    Ok(dev)
}

/// Get ESP partition node based on the root path
pub(crate) fn get_esp_partition_node(root_path: &Utf8Path) -> Result<Option<String>> {
    let device = get_backing_device(root_path)?;
    let base_partitions = bootc_blockdev::partitions_of(Utf8Path::new(&device))?;
    let esp = base_partitions.find_partition_of_esp()?;
    Ok(esp.map(|v| v.node.clone()))
}

/// Mount ESP partition at /boot/efi if not already mounted
pub(crate) fn mount_esp_part(root: &Dir, root_path: &Utf8Path, is_ostree: bool) -> Result<()> {
    let efi_path = Utf8Path::new("boot").join(EFI_DIR);
    let Some(esp_fd) = root
        .open_dir_optional(&efi_path)
        .context("Opening /boot/efi")?
    else {
        return Ok(());
    };

    let Some(false) = esp_fd.is_mountpoint(".")? else {
        return Ok(());
    };

    tracing::debug!("Not a mountpoint: /boot/efi");
    // On ostree env, the physical root is at /target/sysroot
    let physical_root_path = if is_ostree {
        root_path.join("sysroot")
    } else {
        root_path.to_owned()
    };

    if let Some(esp_part) = get_esp_partition_node(&physical_root_path)? {
        mount::mount(&esp_part, &root_path.join(&efi_path))?;
        tracing::debug!("Mounted {esp_part} at /boot/efi");
    }
    Ok(())
}

#[context("Installing bootloader")]
pub(crate) fn install_via_bootupd(
    device: &PartitionTable,
    rootfs: &Utf8Path,
    configopts: &crate::install::InstallConfigOpts,
    deployment_path: &str,
) -> Result<()> {
    let verbose = std::env::var_os("BOOTC_BOOTLOADER_DEBUG").map(|_| "-vvvv");
    // bootc defaults to only targeting the platform boot method.
    let bootupd_opts = (!configopts.generic_image).then_some(["--update-firmware", "--auto"]);

    let srcroot = rootfs.join(deployment_path);
    let devpath = device.path();
    println!("Installing bootloader via bootupd");
    Command::new("bootupctl")
        .args(["backend", "install", "--write-uuid"])
        .args(verbose)
        .args(bootupd_opts.iter().copied().flatten())
        .args(["--src-root", srcroot.as_str()])
        .args(["--device", devpath.as_str(), rootfs.as_str()])
        .log_debug()
        .run_inherited_with_cmd_context()
}

#[context("Installing bootloader using zipl")]
pub(crate) fn install_via_zipl(device: &PartitionTable, boot_uuid: &str) -> Result<()> {
    // Identify the target boot partition from UUID
    let fs = mount::inspect_filesystem_by_uuid(boot_uuid)?;
    let boot_dir = Utf8Path::new(&fs.target);
    let maj_min = fs.maj_min;

    // Ensure that the found partition is a part of the target device
    let device_path = device.path();

    let partitions = bootc_blockdev::list_dev(device_path)?
        .children
        .with_context(|| format!("no partition found on {device_path}"))?;
    let boot_part = partitions
        .iter()
        .find(|part| part.maj_min.as_deref() == Some(maj_min.as_str()))
        .with_context(|| format!("partition device {maj_min} is not on {device_path}"))?;
    let boot_part_offset = boot_part.start.unwrap_or(0);

    // Find exactly one BLS configuration under /boot/loader/entries
    // TODO: utilize the BLS parser in ostree
    let bls_dir = boot_dir.join("boot/loader/entries");
    let bls_entry = bls_dir
        .read_dir_utf8()?
        .try_fold(None, |acc, e| -> Result<_> {
            let e = e?;
            let name = Utf8Path::new(e.file_name());
            if let Some("conf") = name.extension() {
                if acc.is_some() {
                    bail!("more than one BLS configurations under {bls_dir}");
                }
                Ok(Some(e.path().to_owned()))
            } else {
                Ok(None)
            }
        })?
        .with_context(|| format!("no BLS configuration under {bls_dir}"))?;

    let bls_path = bls_dir.join(bls_entry);
    let bls_conf =
        std::fs::read_to_string(&bls_path).with_context(|| format!("reading {bls_path}"))?;

    let mut kernel = None;
    let mut initrd = None;
    let mut options = None;

    for line in bls_conf.lines() {
        match line.split_once(char::is_whitespace) {
            Some(("linux", val)) => kernel = Some(val.trim().trim_start_matches('/')),
            Some(("initrd", val)) => initrd = Some(val.trim().trim_start_matches('/')),
            Some(("options", val)) => options = Some(val.trim()),
            _ => (),
        }
    }

    let kernel = kernel.ok_or_else(|| anyhow!("missing 'linux' key in default BLS config"))?;
    let initrd = initrd.ok_or_else(|| anyhow!("missing 'initrd' key in default BLS config"))?;
    let options = options.ok_or_else(|| anyhow!("missing 'options' key in default BLS config"))?;

    let image = boot_dir.join(kernel).canonicalize_utf8()?;
    let ramdisk = boot_dir.join(initrd).canonicalize_utf8()?;

    // Execute the zipl command to install bootloader
    println!("Running zipl on {device_path}");
    Command::new("zipl")
        .args(["--target", boot_dir.as_str()])
        .args(["--image", image.as_str()])
        .args(["--ramdisk", ramdisk.as_str()])
        .args(["--parameters", options])
        .args(["--targetbase", device_path.as_str()])
        .args(["--targettype", "SCSI"])
        .args(["--targetblocksize", "512"])
        .args(["--targetoffset", &boot_part_offset.to_string()])
        .args(["--add-files", "--verbose"])
        .log_debug()
        .run_inherited_with_cmd_context()
}
