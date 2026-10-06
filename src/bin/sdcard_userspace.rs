//! The installer, run from a live system instead of the SD card's initramfs.
//!
//! The running firmware has the settings volume mounted, so it is never kept from here: this
//! always performs a factory reset, as the installer always did.
use bmc_installer::install::InstallMode;
use bmc_installer::turing_pi::{led, read_from_sdcard, upgrade_bmc, Confirmation};

fn main() -> anyhow::Result<()> {
    let led_tx = led::led_blink_thread();
    let (bootloader, rootfs) = read_from_sdcard()?;
    upgrade_bmc(
        rootfs,
        bootloader,
        InstallMode::FactoryReset,
        |_| Confirmation::Proceed,
        led_tx,
    )
}
