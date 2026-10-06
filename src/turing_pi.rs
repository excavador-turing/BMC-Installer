pub mod led;

use anyhow::Context;
use nix::errno::Errno;
use nix::mount::{mount, MsFlags};
use retry::{delay::Fixed, retry};
use std::fmt::Debug;
use std::io::{self, Read, Seek};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::{fs, path::Path};

use crate::{
    format, image,
    install::{self, InstallMode, SettingsPlan},
    nand::{mtd::MtdNand, Nand},
    ubi::{self, ubinize::Volume},
};

use self::led::LedState;
const BANNER: &str = r"
 _____ _   _ ____  ___ _   _  ____
|_   _| | | |  _ \|_ _| \ | |/ ___|
  | | | | | | |_) || ||  \| | |  _
  | | | |_| |  _ < | || |\  | |_| |
  |_|  \___/|_| \_\___|_| \_|\____|
";

/// Set up the basic environment (e.g. mount points).
pub fn setup_initramfs() -> anyhow::Result<()> {
    // Handle mounts
    for (mount_dev, mount_path, mount_type) in [
        (None, "/dev", "devtmpfs"),
        (None, "/proc", "proc"),
        (None, "/sys", "sysfs"),
    ] {
        let path = Path::new(mount_path);

        if !path.is_dir() {
            fs::create_dir(path)?;
        }

        let result = mount(
            mount_dev.or(Some(path)),
            path,
            Some(mount_type),
            MsFlags::empty(),
            None::<&str>,
        );

        match result {
            // Ignore EBUSY, which indicates that the mountpoint is already mounted.
            Err(Errno::EBUSY) => (),
            r => r?,
        };
    }

    Ok(())
}

/// Sleep until the user cuts power.
pub fn wait_forever() -> ! {
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

/// The kernel command-line word that asks for a factory reset. U-Boot adds it when the microSD
/// card carries a `factory-reset.txt`.
const FACTORY_RESET_CMDLINE: &str = "factory_reset";

/// Determine the [InstallMode] asked for on the kernel command line: [InstallMode::FactoryReset]
/// if it contains the word `factory_reset`, otherwise [InstallMode::KeepSettings].
///
/// `/proc` must be mounted (see [setup_initramfs]). If the command line cannot be read, settings
/// are kept when possible, as they would be with no request at all.
pub fn install_mode_from_cmdline() -> InstallMode {
    let requested = fs::read_to_string("/proc/cmdline")
        .map(|cmdline| {
            cmdline
                .split_whitespace()
                .any(|word| word == FACTORY_RESET_CMDLINE)
        })
        .unwrap_or(false);

    match requested {
        true => InstallMode::FactoryReset,
        false => InstallMode::KeepSettings,
    }
}

/// How the user answered the confirmation prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirmation {
    /// Go ahead with what the prompt said would happen.
    Proceed,

    /// Go ahead, but erase the settings even if they could have been kept.
    FactoryReset,
}

/// The size of the whole NAND chip, in PEBs of the `ubi` partition.
///
/// UBI sizes its bad-block reserve from the whole chip, not the partition. On the Turing Pi 2 the
/// chip is exactly `boot` followed by `ubi` (see the board's device tree).
fn device_pebs(nand_boot: &impl Nand, nand_ubi: &impl Nand) -> u32 {
    let block_bytes = |layout: crate::nand::NandLayout| {
        u64::from(layout.pages_per_block) * layout.bytes_per_page as u64
    };
    let boot = nand_boot.get_layout();
    let ubi = nand_ubi.get_layout();
    let boot_bytes = u64::from(boot.blocks) * block_bytes(boot);
    let boot_pebs = boot_bytes.div_ceil(block_bytes(ubi)) as u32;
    boot_pebs + ubi.blocks
}

/// This is the core function of the installer. Several tasks are executed to
/// upgrade from v1.x firmware or to install onto new flash.
///
/// The `ubi` partition is analyzed first, read-only, to decide whether the board's settings can be
/// kept (only ever with [InstallMode::KeepSettings]). `confirm` is then shown that decision and
/// blocks until the user agrees; it may still ask for a factory reset instead. Nothing is written
/// before `confirm` returns.
///
/// [InstallMode::KeepSettings] must only be used when the settings volume is not in use, i.e.
/// from the initramfs; a mounted UBIFS changes under our feet.
pub fn upgrade_bmc(
    mut rootfs: impl Read + Seek,
    bootloader: impl Read,
    mode: InstallMode,
    confirm: impl FnOnce(&SettingsPlan) -> Confirmation,
    led_tx: mpsc::Sender<&'static [LedState]>,
) -> anyhow::Result<()> {
    eprintln!("{}", BANNER);

    // Open the NAND flash partitions
    let nand_boot = MtdNand::open_named("boot")?;
    let mut nand_ubi = MtdNand::open_named("ubi")?;

    // Locate the rootfs and bootloader to be written
    let rootfs_size = image::erofs_size(&mut rootfs)?;

    // Define the UBI image
    let ubi_volumes = install::installer_volumes(&mut rootfs, rootfs_size);

    // Find out what can be kept, before asking, so that the prompt can say what will happen.
    eprintln!("Analyzing UBI partition...");
    let ebt = ubi::scan_blocks(&mut nand_ubi)?;
    let device_pebs = device_pebs(&nand_boot, &nand_ubi);
    let mut plan = install::plan(&mut nand_ubi, &ebt, mode, &ubi_volumes, device_pebs);

    // These are the tasks to be run once the user confirms the operation:
    struct TaskCtx<'a, N: Nand, R: Read> {
        rpt: howudoin::Tx,
        nand_boot: N,
        nand_ubi: N,
        ebt: ubi::Ebt,
        plan: SettingsPlan,
        ubi_volumes: Vec<Box<dyn Volume + 'a>>,
        bootloader: R,
    }
    type TaskFn<Ctx> = fn(&mut Ctx) -> anyhow::Result<()>;

    // Ready...
    let _ = led_tx.send(led::LED_READY);

    if confirm(&plan) == Confirmation::FactoryReset && plan != SettingsPlan::Reset {
        eprintln!("Factory reset selected: settings will be erased.");
        plan = SettingsPlan::Reset;
    }
    eprintln!("Settings: {}", plan.describe());

    let format_desc = match plan.keeps_settings() {
        true => "Formatting UBI partition (keeping settings)",
        false => "Formatting UBI partition",
    };
    let tasks: [(&str, TaskFn<TaskCtx<'_, _, _>>); 4] = [
        ("Purging boot0 code", |ctx| {
            let purged = format::purge_boot0(&mut ctx.nand_boot)?;
            if purged {
                ctx.rpt
                    .add_info("Legacy Allwinner boot code has been found and erased");
            }
            Ok(())
        }),
        (format_desc, |ctx| {
            install::format_ubi(&mut ctx.nand_ubi, &mut ctx.ebt, &ctx.plan)?;
            Ok(())
        }),
        ("Writing rootfs", |ctx| {
            install::write_ubi(
                &mut ctx.nand_ubi,
                &mut ctx.ebt,
                ctx.ubi_volumes.split_off(0),
                &ctx.plan,
            )?;
            Ok(())
        }),
        ("Updating bootloader", |ctx| {
            format::raw::write_raw_image(&mut ctx.nand_boot, &mut ctx.bootloader, false)?;
            Ok(())
        }),
    ];

    // ...go!
    howudoin::init(howudoin::consumers::TermLine::default());
    let rpt = howudoin::new()
        .label("Installing BMC firmware")
        .set_len(u64::try_from(tasks.len()).ok());
    let mut ctx = TaskCtx {
        rpt,
        nand_boot,
        nand_ubi,
        ebt,
        plan,
        ubi_volumes,
        bootloader,
    };
    let _ = led_tx.send(led::LED_BUSY);
    for (desc, task) in tasks {
        ctx.rpt.desc(desc);
        ctx.rpt.inc();

        if let Err(error) = task(&mut ctx) {
            howudoin::disable();
            thread::sleep(Duration::from_millis(10)); // Give howudoin time to shut down
            return Err(error);
        }
    }

    ctx.rpt.finish();
    howudoin::disable();
    thread::sleep(Duration::from_millis(10)); // Give howudoin time to shut down
    let _ = led_tx.send(led::LED_DONE);

    Ok(())
}

/// Locate the rootfs and bootloader to be written from a fixed partitioned SDcard layout
///
/// # Returns
///
///  tuple (bootloader, rootfs)
pub fn read_from_sdcard() -> anyhow::Result<(impl Read + Debug, impl Read + Seek + Debug)> {
    const ROOTFS_PATH: &str = "/dev/mmcblk0p2";
    const BOOTLOADER_PATH: &str = "/dev/mmcblk0";
    const BOOTLOADER_SIZE: u64 = 6 * 64 * 2048;
    const BOOTLOADER_OFFSET: u64 = 8192; // Boot ROM expects this offset, so it will never change

    let rootfs = retry(Fixed::from_millis(100).take(10), || {
        fs::File::open(ROOTFS_PATH)
    })
    .context(ROOTFS_PATH)?;

    let mut bootloader = retry(Fixed::from_millis(100).take(10), || {
        fs::File::open(BOOTLOADER_PATH)
    })
    .context(BOOTLOADER_PATH)?;

    bootloader.seek(io::SeekFrom::Start(BOOTLOADER_OFFSET))?;
    let bootloader = bootloader.take(BOOTLOADER_SIZE);
    Ok((bootloader, rootfs))
}
