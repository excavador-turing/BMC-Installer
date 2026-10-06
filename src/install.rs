//! The UBI side of a firmware installation: what goes onto the `ubi` partition, and whether the
//! board's settings are kept or erased.
//!
//! This is shared by the SD-card installer ([crate::turing_pi::upgrade_bmc]) and the
//! `install_on_mtd` test binary, so that the path tested against a real kernel UBI is the path
//! that runs on boards.
//!
//! The order is always: [scan](crate::ubi::scan_blocks), [plan], (confirmation, in the
//! installer), [format_ubi], [write_ubi]. [plan] only reads the flash.

use std::io::Read;

use crate::nand::Nand;
use crate::ubi::{
    self, check_capacity, find_preserved_volume,
    ubinize::{BasicVolume, Ubinizer, Volume},
    Ebt, Preserved, VolType,
};

/// The name of the UBI volume holding the board's settings (the overlayfs upper directory,
/// mounted at /mnt/overlay by the firmware).
pub const SETTINGS_VOLUME: &str = "overlay";

/// What the user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMode {
    /// Keep the settings volume if that can be done safely, otherwise erase everything.
    KeepSettings,

    /// Erase everything, restoring factory defaults.
    FactoryReset,
}

/// What the installation will actually do with the settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsPlan {
    /// The settings volume passed every check and will be kept bit-for-bit.
    Keep(Preserved),

    /// A factory reset was asked for: everything will be erased.
    Reset,

    /// Keeping the settings was asked for but is not safe, for the given reason: everything will
    /// be erased.
    CannotKeep(String),
}

impl SettingsPlan {
    /// Does this plan keep the settings?
    pub fn keeps_settings(&self) -> bool {
        matches!(self, Self::Keep(_))
    }

    /// A one-line summary of the decision, for logs.
    pub fn describe(&self) -> String {
        match self {
            Self::Keep(p) => format!(
                "kept: volume {SETTINGS_VOLUME:?} (id {}, {} PEBs reserved, {} PEBs kept)",
                p.vol_id,
                p.record.reserved_pebs,
                p.pebs.len(),
            ),
            Self::Reset => "erased: factory reset requested".to_string(),
            Self::CannotKeep(reason) => format!("erased: {reason}"),
        }
    }
}

/// The volumes the installer writes: the U-Boot environment and the root file system.
///
/// `rootfs_size` must be the meaningful size of the image (see [crate::image::erofs_size]); it
/// sets the volume size, and with it the PEBs reserved for the volume.
pub fn installer_volumes<'a>(
    rootfs: &'a mut dyn Read,
    rootfs_size: u64,
) -> Vec<Box<dyn Volume + 'a>> {
    vec![
        Box::new(
            BasicVolume::new(VolType::Dynamic)
                .id(0)
                .name("uboot-env")
                .size(65536),
        ),
        Box::new(
            BasicVolume::new(VolType::Static)
                .name("rootfs")
                .skipcheck() // Opening the volume at boot takes ~10sec. longer without this flag
                .size(rootfs_size)
                .image(rootfs),
        ),
    ]
}

/// Decide what happens to the settings. This only reads the flash.
///
/// `ebt` must come straight from [ubi::scan_blocks]. `volumes` are the volumes about to be
/// written (without the settings volume). `device_pebs` is the size of the whole flash chip in
/// PEBs of the `ubi` partition's size, which UBI's bad-block reserve is computed from.
pub fn plan<N: Nand>(
    nand: &mut N,
    ebt: &Ebt,
    mode: InstallMode,
    volumes: &[Box<dyn Volume + '_>],
    device_pebs: u32,
) -> SettingsPlan {
    if mode == InstallMode::FactoryReset {
        return SettingsPlan::Reset;
    }

    let preserved = match find_preserved_volume(nand, ebt, SETTINGS_VOLUME) {
        Ok(preserved) => preserved,
        Err(refusal) => return SettingsPlan::CannotKeep(refusal.0),
    };

    // The new volumes ask for their IDs (`uboot-env` is pinned to 0, where U-Boot expects it).
    // Renumbering either side is not worth the risk; such a board just gets a clean install.
    if volumes
        .iter()
        .any(|v| v.get_vol_id() == Some(preserved.vol_id))
    {
        return SettingsPlan::CannotKeep(format!(
            "the settings are stored under volume ID {}, which the new firmware needs",
            preserved.vol_id,
        ));
    }

    let eb_size = ubi::leb_size(nand.get_layout());
    let new_pebs = Ubinizer::estimate_blocks(volumes.iter().map(|x| &**x), eb_size);
    if let Err(refusal) = check_capacity(ebt, &preserved, new_pebs, device_pebs) {
        return SettingsPlan::CannotKeep(refusal.0);
    }

    SettingsPlan::Keep(preserved)
}

/// Erase the `ubi` partition, except the settings volume's PEBs if the plan keeps them.
pub fn format_ubi<N: Nand>(nand: &mut N, ebt: &mut Ebt, plan: &SettingsPlan) -> anyhow::Result<()> {
    match plan {
        SettingsPlan::Keep(preserved) => ubi::format(nand, ebt, &preserved.pebs),
        _ => ubi::format(nand, ebt, &Default::default()),
    }
}

/// Write the new volumes, and the settings volume's record if the plan keeps it.
pub fn write_ubi<'a, N: Nand>(
    nand: &mut N,
    ebt: &mut Ebt,
    mut volumes: Vec<Box<dyn Volume + 'a>>,
    plan: &SettingsPlan,
) -> anyhow::Result<()> {
    if let SettingsPlan::Keep(preserved) = plan {
        volumes.push(Box::new(preserved.volume()));
    }
    ubi::write_volumes(nand, ebt, volumes)
}

#[cfg(test)]
mod test;
