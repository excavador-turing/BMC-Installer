//! Run the installer's UBI path against an arbitrary MTD device, for testing against a real kernel
//! UBI (e.g. on `nandsim`).
//!
//! This is the same scan → plan → format → write sequence as
//! [upgrade_bmc](bmc_installer::turing_pi::upgrade_bmc), through the same
//! [install](bmc_installer::install) functions, minus everything that needs the board: there is no
//! confirmation prompt, no LEDs, no boot0 purge and no bootloader (the `boot` partition is never
//! touched, and need not exist). It is not part of the firmware; Buildroot only builds the
//! `sdcard` and `sdcard_userspace` binaries.
//!
//! The decision is printed on stdout as two lines, `decision: kept` or `decision: erased`, then
//! `reason: ...` (or `detail: ...` when kept).
//!
//! `--rootfs` must be an EROFS image (e.g. made by `mkfs.erofs`), because the installer sizes the
//! rootfs volume from the EROFS superblock, exactly as it does on the SD card.

use anyhow::Context;
use clap::Parser;

use std::path::PathBuf;

/// Install a rootfs onto a UBI MTD device the way the SD-card installer does.
#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Cli {
    /// Path to the `/dev/mtdX` character device holding the UBI (must not be attached)
    #[clap(long)]
    mtd: PathBuf,

    /// Path to the EROFS image to write as the `rootfs` volume
    #[clap(long)]
    rootfs: PathBuf,

    /// Erase the settings instead of keeping them (like `factory_reset` on the kernel command
    /// line, or typing ERASE at the installer prompt)
    #[clap(long)]
    factory_reset: bool,

    /// Size of the whole flash chip in PEBs, which UBI's bad-block reserve is computed from;
    /// defaults to the size of `--mtd` (right for an unpartitioned nandsim)
    #[clap(long)]
    device_pebs: Option<u32>,

    /// Accepted for compatibility; this program never writes a bootloader
    #[clap(long)]
    bootloader_skip: bool,
}

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use bmc_installer::{
        image,
        install::{self, InstallMode, SettingsPlan},
        nand::{mtd::MtdNand, Nand},
        ubi,
    };
    use std::fs::File;

    let args = Cli::parse();
    let _ = args.bootloader_skip; // Nothing to skip: the bootloader is never written here.

    let mut nand =
        MtdNand::open(&args.mtd).with_context(|| format!("opening {}", args.mtd.display()))?;
    let mut rootfs =
        File::open(&args.rootfs).with_context(|| format!("opening {}", args.rootfs.display()))?;
    let rootfs_size = image::erofs_size(&mut rootfs)
        .with_context(|| format!("{} must be an EROFS image", args.rootfs.display()))?;

    let mode = match args.factory_reset {
        true => InstallMode::FactoryReset,
        false => InstallMode::KeepSettings,
    };
    let device_pebs = args.device_pebs.unwrap_or(nand.get_layout().blocks);

    let volumes = install::installer_volumes(&mut rootfs, rootfs_size);
    let mut ebt = ubi::scan_blocks(&mut nand)?;
    let plan = install::plan(&mut nand, &ebt, mode, &volumes, device_pebs);

    match &plan {
        SettingsPlan::Keep(_) => {
            println!("decision: kept");
            println!("detail: {}", plan.describe());
        }
        SettingsPlan::Reset => {
            println!("decision: erased");
            println!("reason: factory reset requested");
        }
        SettingsPlan::CannotKeep(reason) => {
            println!("decision: erased");
            println!("reason: {reason}");
        }
    }

    install::format_ubi(&mut nand, &mut ebt, &plan)?;
    install::write_ubi(&mut nand, &mut ebt, volumes, &plan)?;

    println!("done");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() -> anyhow::Result<()> {
    let _ = Cli::parse();
    anyhow::bail!("MTD devices are only supported on Linux")
}
