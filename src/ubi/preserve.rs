//! This module decides whether one existing UBI volume can survive a reimaging, and if so, which
//! PEBs hold it.
//!
//! The firmware keeps its settings (root password, TLS certificate, network and node settings) in
//! a dynamic UBIFS volume named `overlay`. Reimaging normally erases every PEB and writes a fresh
//! layout volume, which resets the board to factory defaults. To keep the settings instead, the
//! installer leaves that volume's PEBs exactly as they are and copies its volume table record into
//! the new layout volume, at the same volume ID. When UBI attaches, it scans every PEB, finds the
//! old `vol_id:lnum` pairs in their VID headers, and the volume is back.
//!
//! That only works if the result is a UBI the kernel will attach, and a board whose UBI does not
//! attach does not boot. So everything here is a check, and every check that fails produces a
//! [Refusal] with a reason for the console; the caller then falls back to erasing everything,
//! which is what the installer has always done.

use super::format::{compute_prototype, leb_size};
use super::headers::{Ec, Vid, VolTableRecord, VolType, VtblSlot};
use super::scan::{BlockContent, Ebt};
use super::ubinize::{vtbl_slots, PreservedVolume, UBI_LAYOUT_VOLUME_ID, UBI_VTBL_RECORD_SIZE};
use crate::nand::{Nand, NandBlock};

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// The common-header magic of every UBIFS node, as stored on flash (little-endian). LEB 0 of a
/// UBIFS volume starts with the superblock node.
const UBIFS_NODE_MAGIC: u32 = 0x06101831;

/// The default of the kernel's `CONFIG_MTD_UBI_BEB_LIMIT`: how many bad PEBs, per 1024 PEBs of
/// the whole flash chip, UBI reserves room for. The BMC kernel and U-Boot both use the default.
pub const BEB_LIMIT_PER1024: u32 = 20;

/// PEBs reserved by UBI's wear-leveling sub-system (`WL_RESERVED_PEBS`, drivers/mtd/ubi/wl.c).
const WL_RESERVED_PEBS: u32 = 1;

/// PEBs reserved for atomic LEB change (`EBA_RESERVED_PEBS`, drivers/mtd/ubi/ubi.h).
const EBA_RESERVED_PEBS: u32 = 1;

/// Extra PEBs that must be left over on top of everything UBI is known to need.
pub const CAPACITY_MARGIN: u32 = 4;

/// The reason a volume cannot be preserved, meant to be shown to the user as-is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

macro_rules! refuse {
    ($($arg:tt)*) => {
        return Err(Refusal(format!($($arg)*)))
    };
}

/// An existing volume that has passed every check and can be kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preserved {
    /// The volume ID, which is its index in the volume table
    pub vol_id: u32,

    /// The volume's record from the current volume table
    pub record: VolTableRecord,

    /// Every PEB holding one of its LEBs, including older duplicate copies (UBI resolves those at
    /// attach time, by sqnum, and the newer copy may fail its data CRC check)
    pub pebs: BTreeSet<u32>,
}

impl Preserved {
    /// The [Volume](super::ubinize::Volume) that writes this volume's record into the new layout.
    pub fn volume(&self) -> PreservedVolume {
        PreservedVolume::new(self.vol_id, self.record.clone())
    }
}

/// Read the data area of a PEB, `len` bytes (rounded up to whole pages).
fn read_data<N: Nand>(nand: &mut N, peb: u32, ec: Ec, len: usize) -> Result<Vec<u8>, Refusal> {
    let page_size = nand.get_layout().bytes_per_page;
    let start_page = ec.data_offset as usize / page_size;
    let pages = len.div_ceil(page_size);

    let block = match nand.block(peb) {
        Ok(Some(block)) => block,
        Ok(None) => refuse!("PEB {peb} went bad while reading it"),
        Err(e) => refuse!("PEB {peb} could not be read: {e}"),
    };

    let mut buf = vec![0u8; pages * page_size];
    if let Err(e) = block.read(start_page as u32, &mut buf) {
        refuse!("PEB {peb} could not be read: {e}");
    }
    buf.truncate(len);
    Ok(buf)
}

/// For every LEB of `vol_id`, the PEB holding the newest copy (highest sqnum).
fn newest_copies(ebt: &Ebt, vol_id: u32) -> BTreeMap<u32, (u32, Ec, Vid)> {
    let mut lebs: BTreeMap<u32, (u32, Ec, Vid)> = BTreeMap::new();
    for (peb, content) in ebt.iter().enumerate() {
        if let BlockContent::EcData(ec, Some(vid)) = *content {
            if vid.vol_id != vol_id {
                continue;
            }
            let newer = lebs
                .get(&vid.lnum)
                .is_none_or(|&(_, _, old)| vid.sqnum > old.sqnum);
            if newer {
                lebs.insert(vid.lnum, (peb as u32, ec, vid));
            }
        }
    }
    lebs
}

/// Read and decode both copies of the volume table (LEB 0 and LEB 1 of the layout volume).
///
/// Either copy missing, unreadable, or holding a slot that fails its CRC is a refusal: the kernel
/// would recover from one good copy, but here there is no reason to trust a table UBI itself would
/// have to repair.
pub(crate) fn read_volume_tables<N: Nand>(
    nand: &mut N,
    ebt: &Ebt,
) -> Result<[Vec<Option<VolTableRecord>>; 2], Refusal> {
    let layout = nand.get_layout();
    let slots = vtbl_slots(leb_size(layout));
    let table_len = slots * UBI_VTBL_RECORD_SIZE;

    let copies = newest_copies(ebt, UBI_LAYOUT_VOLUME_ID);
    if let Some(&lnum) = copies.keys().find(|&&lnum| lnum > 1) {
        refuse!("the volume table has an unexpected LEB {lnum}");
    }

    let mut tables: [Vec<Option<VolTableRecord>>; 2] = Default::default();
    for (lnum, table) in tables.iter_mut().enumerate() {
        let Some(&(peb, ec, _)) = copies.get(&(lnum as u32)) else {
            refuse!("copy {} of the volume table is missing", lnum + 1);
        };
        let bytes = read_data(nand, peb, ec, table_len)?;
        for record in bytes.chunks_exact(UBI_VTBL_RECORD_SIZE) {
            match VolTableRecord::decode_slot(record) {
                VtblSlot::Empty => table.push(None),
                VtblSlot::Volume(record) => table.push(Some(record)),
                VtblSlot::Corrupt => refuse!("copy {} of the volume table is corrupt", lnum + 1),
            }
        }
    }

    Ok(tables)
}

/// Find the one record called `name` in a volume table, returning its ID (its index).
fn find_record(
    table: &[Option<VolTableRecord>],
    name: &str,
) -> Result<Option<(u32, VolTableRecord)>, Refusal> {
    let mut found = table
        .iter()
        .enumerate()
        .filter_map(|(id, record)| Some((id as u32, record.as_ref()?)))
        .filter(|(_, record)| record.name == name);

    let first = found.next();
    if found.next().is_some() {
        refuse!("the volume table names two volumes {name:?}");
    }
    Ok(first.map(|(id, record)| (id, record.clone())))
}

/// Find the volume named `name` and check that it can be kept bit-for-bit across a reimaging.
///
/// This only reads from flash. It must be called on the [Ebt] from [super::scan_blocks], before
/// [super::format] changes anything, and the result only holds for that same [Ebt].
pub fn find_preserved_volume<N: Nand>(
    nand: &mut N,
    ebt: &Ebt,
    name: &str,
) -> Result<Preserved, Refusal> {
    // A board still on the v1.x NAND layout has to be migrated, which rewrites every PEB, and its
    // firmware never had this volume anyway.
    if ebt.iter().any(|x| matches!(x, BlockContent::RawVid(_))) {
        refuse!("the flash still has the v1.x layout, which must be converted");
    }

    // Every EC header that survives must match the ones `format` is about to write: UBI refuses
    // to attach when image_seq differs between PEBs (`scan_peb` in drivers/mtd/ubi/attach.c), or
    // when the VID header or data offsets differ from its own (`validate_ec_hdr` in io.c).
    let proto = match compute_prototype(nand.get_layout(), ebt.iter().copied()) {
        Ok(proto) => proto,
        Err(e) => refuse!("the UBI headers could not be analyzed: {e}"),
    };
    let matches_proto = |ec: Ec| ec == proto.ec(ec.ec);

    let layout_copies = newest_copies(ebt, UBI_LAYOUT_VOLUME_ID);
    if layout_copies.is_empty() {
        refuse!("the flash holds no UBI volumes (blank or never installed)");
    }
    for (lnum, &(peb, ec, _)) in &layout_copies {
        if !matches_proto(ec) {
            refuse!("the volume table (LEB {lnum}, PEB {peb}) has an unexpected UBI header");
        }
    }

    let [table0, table1] = read_volume_tables(nand, ebt)?;
    let Some((vol_id, record)) = find_record(&table0, name)? else {
        refuse!("no {name:?} volume was found");
    };
    if find_record(&table1, name)? != Some((vol_id, record.clone())) {
        refuse!("the two copies of the volume table disagree about {name:?}");
    }

    if record.vol_type != VolType::Dynamic {
        refuse!("the {name:?} volume is not a dynamic volume");
    }
    if record.upd_marker {
        refuse!("the {name:?} volume was left half-updated");
    }

    // Collect every PEB of the volume, duplicates included, checking each the way UBI will when
    // it attaches (`check_av` in drivers/mtd/ubi/vtbl.c).
    let mut pebs = BTreeSet::new();
    for (peb, content) in ebt.iter().enumerate() {
        let BlockContent::EcData(ec, Some(vid)) = *content else {
            continue;
        };
        if vid.vol_id != vol_id {
            continue;
        }
        if ec.image_seq != proto.image_seq {
            refuse!(
                "PEB {peb} of {name:?} has image sequence {:#x}, the rest of the flash {:#x}",
                ec.image_seq,
                proto.image_seq,
            );
        }
        if !matches_proto(ec) {
            refuse!("PEB {peb} of {name:?} has an unexpected UBI header layout");
        }
        if vid.vol_type != VolType::Dynamic || vid.data_pad != record.data_pad {
            refuse!("PEB {peb} of {name:?} does not match its volume table record");
        }
        if vid.lnum >= record.reserved_pebs {
            refuse!(
                "PEB {peb} of {name:?} holds LEB {} beyond its size",
                vid.lnum
            );
        }
        pebs.insert(peb as u32);
    }

    // The volume must actually hold a UBIFS: its LEB 0 starts with the superblock node.
    let Some(&(leb0_peb, leb0_ec, _)) = newest_copies(ebt, vol_id).get(&0) else {
        refuse!("the {name:?} volume holds no data");
    };
    let head = read_data(nand, leb0_peb, leb0_ec, 4)?;
    if head != UBIFS_NODE_MAGIC.to_le_bytes() {
        refuse!("the {name:?} volume does not hold a UBIFS file system");
    }

    Ok(Preserved {
        vol_id,
        record,
        pebs,
    })
}

/// The kernel's `mult_frac(x, numer, denom)`: `x * numer / denom` without overflowing.
fn mult_frac(x: u64, numer: u64, denom: u64) -> u64 {
    (x / denom) * numer + ((x % denom) * numer) / denom
}

/// How many bad PEBs UBI expects at most, mirroring `get_bad_peb_limit` in
/// drivers/mtd/ubi/build.c: [BEB_LIMIT_PER1024] per 1024 PEBs of the whole flash chip (not just
/// the UBI partition), rounded up.
pub fn bad_peb_limit(device_pebs: u32) -> u32 {
    let device_pebs = u64::from(device_pebs);
    let per1024 = u64::from(BEB_LIMIT_PER1024);
    let mut limit = mult_frac(device_pebs, per1024, 1024);
    if mult_frac(limit, 1024, per1024) < device_pebs {
        limit += 1;
    }
    limit as u32
}

/// Check that the flash has room for the preserved volume next to the new ones, so that UBI will
/// attach the result.
///
/// UBI refuses to attach (-ENOSPC) when the volumes' `reserved_pebs`, the 2 layout PEBs, the
/// wear-leveling reserve and the EBA reserve exceed the good PEBs (`init_volumes` in vtbl.c,
/// `ubi_wl_init` in wl.c, `ubi_eba_init` in eba.c). The bad-PEB reserve on top of that is only a
/// warning there, but a board without it loses data as soon as a block wears out, so it is
/// required here too, computed like `ubi_calculate_reserved` in misc.c, plus [CAPACITY_MARGIN].
///
/// `new_pebs` is what the new volumes need, including the layout volume; this is
/// [Ubinizer::estimate_blocks](super::ubinize::Ubinizer::estimate_blocks), which equals the
/// `reserved_pebs` the installer writes for volumes with a set size. `device_pebs` is the size of
/// the whole flash chip in PEBs.
pub fn check_capacity(
    ebt: &Ebt,
    preserved: &Preserved,
    new_pebs: u32,
    device_pebs: u32,
) -> Result<(), Refusal> {
    let bad = ebt.iter().filter(|x| **x == BlockContent::Bad).count() as u32;
    let good = ebt.len() as u32 - bad;
    let beb_reserve = bad_peb_limit(device_pebs).saturating_sub(bad);

    let needed = u64::from(preserved.record.reserved_pebs)
        + u64::from(new_pebs)
        + u64::from(WL_RESERVED_PEBS)
        + u64::from(EBA_RESERVED_PEBS)
        + u64::from(beb_reserve)
        + u64::from(CAPACITY_MARGIN);

    // The PEBs physically occupied right after writing: the kept ones (duplicates included) and
    // the newly written ones.
    let occupied = preserved.pebs.len() as u64 + u64::from(new_pebs) + u64::from(CAPACITY_MARGIN);

    if needed > u64::from(good) || occupied > u64::from(good) {
        refuse!(
            "not enough room on the flash ({needed} blocks needed, {good} usable)",
            needed = needed.max(occupied),
        );
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_bad_peb_limit() {
        // 128 MiB of 128 KiB PEBs: exactly 20.
        assert_eq!(bad_peb_limit(1024), 20);
        // 256 MiB: exactly 40.
        assert_eq!(bad_peb_limit(2048), 40);
        // Rounded up when not a multiple of 1024 / 20.
        assert_eq!(bad_peb_limit(1016), 20);
        assert_eq!(bad_peb_limit(256), 5);
        assert_eq!(bad_peb_limit(100), 2);
    }
}
