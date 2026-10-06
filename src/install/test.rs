//! Tests of the whole UBI installation flow on a simulated NAND: a "running board" UBI is built
//! with the installer's own writer, then the installer runs over it.

use super::*;

use crate::nand::{NandBlock, NandLayout, SimNand};
use crate::ubi::preserve::{bad_peb_limit, read_volume_tables, CAPACITY_MARGIN};
use crate::ubi::ubinize::{BasicVolume, PreservedVolume, Ubinizer};
use crate::ubi::{
    find_preserved_volume, format, scan_blocks, write_volumes, BlockContent, Ec, Vid,
    VolTableRecord, VolType, VtblSlot,
};

use std::collections::{BTreeMap, BTreeSet};

const LAYOUT: NandLayout = NandLayout {
    blocks: 256,
    pages_per_block: 16,
    bytes_per_page: 512,
};

/// LEB size for [LAYOUT]
const LEB: usize = 512 * 14;

/// Volume ID of the layout volume
const LAYOUT_VOL_ID: u32 = 0x7FFFEFFF;

/// Size of a volume table record
const VTBL_RECORD: usize = 172;

const UBIFS_MAGIC: [u8; 4] = 0x06101831u32.to_le_bytes();

/// The volumes of a simulated running board
struct Board {
    overlay_id: u32,
    overlay_lebs: u64,
    overlay_data: Vec<u8>,
    uboot_env_id: u32,
}

impl Default for Board {
    fn default() -> Self {
        // Recognisable content in 5 LEBs, starting like a UBIFS.
        let mut overlay_data: Vec<u8> = (0..5 * LEB).map(|i| (i * 7 + i / LEB) as u8).collect();
        overlay_data[..4].copy_from_slice(&UBIFS_MAGIC);

        Self {
            overlay_id: 3,
            overlay_lebs: 20,
            overlay_data,
            uboot_env_id: 0,
        }
    }
}

impl Board {
    /// Write a UBI the way a board ends up after OTA updates: `uboot-env`, `rootfs_prev`,
    /// `rootfs` and `overlay`, with a hole in the IDs where `rootfs_new` used to be, and one stale
    /// copy of an overlay LEB.
    fn build(&self, with_overlay: bool) -> SimNand {
        let mut nand = SimNand::new(LAYOUT);
        let mut ebt = scan_blocks(&mut nand).unwrap();
        format(&mut nand, &mut ebt, &Default::default()).unwrap();

        let mut prev = &[0x11u8; 3 * LEB][..];
        let mut cur = &[0x22u8; 3 * LEB][..];
        let mut overlay = &self.overlay_data[..];

        let mut volumes: Vec<Box<dyn Volume + '_>> = vec![
            Box::new(
                BasicVolume::new(VolType::Dynamic)
                    .id(self.uboot_env_id)
                    .name("uboot-env")
                    .size(65536),
            ),
            Box::new(
                BasicVolume::new(VolType::Static)
                    .id(1)
                    .name("rootfs_prev")
                    .size(prev.len() as u64)
                    .image(&mut prev),
            ),
            Box::new(
                BasicVolume::new(VolType::Static)
                    .id(4)
                    .name("rootfs")
                    .size(cur.len() as u64)
                    .image(&mut cur),
            ),
        ];
        if with_overlay {
            volumes.push(Box::new(
                BasicVolume::new(VolType::Dynamic)
                    .id(self.overlay_id)
                    .name("overlay")
                    .size(self.overlay_lebs * LEB as u64)
                    .image(&mut overlay),
            ));
        }
        write_volumes(&mut nand, &mut ebt, volumes).unwrap();

        if with_overlay {
            add_stale_copy(&mut nand, self.overlay_id, 1);
        }

        nand
    }
}

/// The new rootfs image the installer writes in these tests
fn new_rootfs() -> Vec<u8> {
    (0..2 * LEB + 100).map(|i| (i % 251) as u8).collect()
}

/// Read a whole raw block
fn raw_block(nand: &mut SimNand, peb: u32) -> Vec<u8> {
    let mut buf = vec![0u8; LAYOUT.bytes_per_page * LAYOUT.pages_per_block as usize];
    nand.block(peb).unwrap().unwrap().read(0, &mut buf).unwrap();
    buf
}

/// Read, modify and reprogram a whole block
fn rewrite_block(nand: &mut SimNand, peb: u32, modify: impl FnOnce(&mut Vec<u8>)) {
    let mut buf = raw_block(nand, peb);
    modify(&mut buf);
    let mut block = nand.block(peb).unwrap().unwrap();
    block.erase().unwrap();
    block.program(0, &buf).unwrap();
}

/// Replace the record of volume `id` in both copies of the volume table
fn rewrite_record(nand: &mut SimNand, id: u32, record: &VolTableRecord, copies: &[u32]) {
    let ebt = scan_blocks(nand).unwrap();
    for lnum in copies {
        let peb = find_pebs(&ebt, LAYOUT_VOL_ID)[lnum][0];
        let bytes = record.clone().into_bytes();
        rewrite_block(nand, peb, |raw| {
            let at = 2 * LAYOUT.bytes_per_page + id as usize * VTBL_RECORD;
            raw[at..at + VTBL_RECORD].copy_from_slice(&bytes);
        });
    }
}

/// Write an older copy of `vol_id:lnum`, with different data, into a free block, as an
/// interrupted wear-leveling move would leave behind.
fn add_stale_copy(nand: &mut SimNand, vol_id: u32, lnum: u32) {
    let ebt = scan_blocks(nand).unwrap();
    let source = find_pebs(&ebt, vol_id)[&lnum][0];
    let target = ebt
        .iter()
        .position(|x| matches!(x, BlockContent::EcErased(_)))
        .unwrap() as u32;

    let source_raw = raw_block(nand, source);
    let page = LAYOUT.bytes_per_page;
    let vid = Vid::decode(&source_raw[page..]).unwrap().sqnum(0);

    let mut buf = vec![0xFFu8; page * (LAYOUT.pages_per_block as usize - 1)];
    vid.encode(&mut buf[..page]).unwrap();
    buf[page..page + 64].fill(0x5A);
    nand.block(target)
        .unwrap()
        .unwrap()
        .program(1, &buf)
        .unwrap();
}

/// PEBs of a volume per LEB, newest copy first
fn find_pebs(ebt: &Ebt, vol_id: u32) -> BTreeMap<u32, Vec<u32>> {
    let mut lebs: BTreeMap<u32, Vec<(u64, u32)>> = BTreeMap::new();
    for (peb, content) in ebt.iter().enumerate() {
        if let BlockContent::EcData(_, Some(vid)) = content {
            if vid.vol_id == vol_id {
                lebs.entry(vid.lnum)
                    .or_default()
                    .push((vid.sqnum, peb as u32));
            }
        }
    }
    lebs.into_iter()
        .map(|(lnum, mut copies)| {
            copies.sort_by(|a, b| b.cmp(a));
            (lnum, copies.into_iter().map(|(_, peb)| peb).collect())
        })
        .collect()
}

/// All PEBs of a volume
fn all_pebs(ebt: &Ebt, vol_id: u32) -> BTreeSet<u32> {
    find_pebs(ebt, vol_id).into_values().flatten().collect()
}

/// The volume table, as UBI would read it, by name
fn volume_table(nand: &mut SimNand) -> BTreeMap<String, (u32, VolTableRecord)> {
    let ebt = scan_blocks(nand).unwrap();
    let [t0, t1] = read_volume_tables(nand, &ebt).unwrap();
    assert_eq!(t0, t1, "volume table copies differ");
    t0.into_iter()
        .enumerate()
        .filter_map(|(id, r)| r.map(|r| (r.name.clone(), (id as u32, r))))
        .collect()
}

/// Read back the content of a volume (newest copy of each LEB)
fn volume_data(nand: &mut SimNand, vol_id: u32) -> Vec<u8> {
    let ebt = scan_blocks(nand).unwrap();
    let mut data = Vec::new();
    for (lnum, pebs) in find_pebs(&ebt, vol_id) {
        assert_eq!(lnum as usize, data.len() / LEB, "LEBs not contiguous");
        let raw = raw_block(nand, pebs[0]);
        let vid = Vid::decode(&raw[LAYOUT.bytes_per_page..]).unwrap();
        let start = 2 * LAYOUT.bytes_per_page;
        let len = match vid.data_size {
            0 => LEB,
            n => n as usize,
        };
        data.extend_from_slice(&raw[start..start + len]);
    }
    data
}

/// Check what the kernel checks when attaching (drivers/mtd/ubi/attach.c, vtbl.c): one image_seq,
/// every VID header pointing to a volume in the table with its LEB inside the volume and the same
/// type, at most one autoresize volume, and enough good PEBs for every reservation.
fn assert_attachable(nand: &mut SimNand) {
    let ebt = scan_blocks(nand).unwrap();
    let table = volume_table(nand);
    let by_id: BTreeMap<u32, &VolTableRecord> = table.values().map(|(id, r)| (*id, r)).collect();

    let mut image_seqs = BTreeSet::new();
    for content in ebt.iter() {
        match content {
            BlockContent::EcErased(ec) => {
                image_seqs.insert(ec.image_seq);
            }
            BlockContent::EcData(ec, Some(vid)) => {
                image_seqs.insert(ec.image_seq);
                if vid.vol_id == LAYOUT_VOL_ID {
                    continue;
                }
                let record = by_id
                    .get(&vid.vol_id)
                    .unwrap_or_else(|| panic!("PEB of unknown volume {}", vid.vol_id));
                assert!(vid.lnum < record.reserved_pebs);
                assert_eq!(vid.vol_type, record.vol_type);
                assert_eq!(vid.data_pad, record.data_pad);
            }
            BlockContent::Bad => (),
            other => panic!("unexpected block state {other:?}"),
        }
    }
    assert_eq!(image_seqs.len(), 1, "image_seq differs between PEBs");

    let reserved: u32 = table.values().map(|(_, r)| r.reserved_pebs).sum();
    let good = ebt.iter().filter(|x| **x != BlockContent::Bad).count() as u32;
    assert!(reserved + 2 + 2 <= good, "volumes over-commit the flash");

    let autoresize = table.values().filter(|(_, r)| r.flags & 1 != 0).count();
    assert!(autoresize <= 1);
}

/// Run the installer's UBI path, as `upgrade_bmc` and `install_on_mtd` do.
fn install(nand: &mut SimNand, mode: InstallMode) -> SettingsPlan {
    let rootfs = new_rootfs();
    let mut image = &rootfs[..];
    let volumes = installer_volumes(&mut image, rootfs.len() as u64);
    let mut ebt = scan_blocks(nand).unwrap();
    let plan = plan(nand, &ebt, mode, &volumes, LAYOUT.blocks);
    format_ubi(nand, &mut ebt, &plan).unwrap();
    write_ubi(nand, &mut ebt, volumes, &plan).unwrap();
    plan
}

/// Dump the whole simulated flash
fn dump(nand: &mut SimNand) -> Vec<u8> {
    let mut out = Vec::new();
    nand.save(&mut out).unwrap();
    out
}

/// Install in keep mode, expect a refusal mentioning `reason`, and check that the result is
/// exactly what a factory reset of the same flash produces.
fn assert_refused_like_reset(mut nand: SimNand, reason: &str) {
    let mut reset = nand.clone();

    let plan = install(&mut nand, InstallMode::KeepSettings);
    match &plan {
        SettingsPlan::CannotKeep(why) => assert!(
            why.contains(reason),
            "refused for {why:?}, expected {reason:?}"
        ),
        other => panic!("expected a refusal, got {other:?}"),
    }

    assert_eq!(
        install(&mut reset, InstallMode::FactoryReset),
        SettingsPlan::Reset
    );
    assert!(
        dump(&mut nand) == dump(&mut reset),
        "refusal differs from a factory reset"
    );

    let table = volume_table(&mut nand);
    assert!(!table.contains_key("overlay"));
    assert_eq!(volume_data(&mut nand, table["rootfs"].0), new_rootfs());
    assert_attachable(&mut nand);
}

#[test]
fn test_keep_settings() {
    let board = Board::default();
    let mut nand = board.build(true);
    assert_attachable(&mut nand);

    let before = volume_table(&mut nand);
    let (overlay_id, overlay_record) = before["overlay"].clone();
    assert_eq!(overlay_id, board.overlay_id);

    let ebt = scan_blocks(&mut nand).unwrap();
    let kept = all_pebs(&ebt, overlay_id);
    assert_eq!(kept.len(), 6, "5 LEBs and one stale copy");
    let kept_raw: Vec<_> = kept.iter().map(|&p| raw_block(&mut nand, p)).collect();
    let kept_max_sqnum = kept
        .iter()
        .filter_map(|&p| match ebt[p as usize] {
            BlockContent::EcData(_, Some(vid)) => Some(vid.sqnum),
            _ => None,
        })
        .max()
        .unwrap();

    let plan = install(&mut nand, InstallMode::KeepSettings);
    match &plan {
        SettingsPlan::Keep(p) => {
            assert_eq!(p.vol_id, overlay_id);
            assert_eq!(p.pebs, kept);
        }
        other => panic!("expected to keep settings, got {other:?}"),
    }

    // The overlay's PEBs are bit-for-bit what they were.
    for (&peb, raw) in kept.iter().zip(&kept_raw) {
        assert!(raw_block(&mut nand, peb) == *raw, "PEB {peb} changed");
    }

    // The new layout volume describes the overlay at the same ID, with the same record.
    let after = volume_table(&mut nand);
    assert_eq!(after["overlay"], (overlay_id, overlay_record));
    assert_eq!(volume_data(&mut nand, overlay_id), board.overlay_data);

    // The rootfs is new, U-Boot's environment is where U-Boot looks, the old rootfs is gone.
    assert_eq!(after["uboot-env"].0, 0);
    assert_eq!(volume_data(&mut nand, after["rootfs"].0), new_rootfs());
    assert!(!after.contains_key("rootfs_prev"));
    assert_eq!(after.len(), 3);

    // Every newly written PEB is numbered after every kept one.
    let ebt = scan_blocks(&mut nand).unwrap();
    for (peb, content) in ebt.iter().enumerate() {
        if let BlockContent::EcData(_, Some(vid)) = content {
            if !kept.contains(&(peb as u32)) {
                assert!(vid.sqnum > kept_max_sqnum, "PEB {peb} has an older sqnum");
            }
        }
    }

    assert_attachable(&mut nand);

    // A second install keeps them again.
    let plan = install(&mut nand, InstallMode::KeepSettings);
    assert!(plan.keeps_settings(), "{plan:?}");
    for (&peb, raw) in kept.iter().zip(&kept_raw) {
        assert!(raw_block(&mut nand, peb) == *raw, "PEB {peb} changed");
    }
    assert_attachable(&mut nand);
}

#[test]
fn test_factory_reset_mode() {
    let mut nand = Board::default().build(true);
    let mut reference = nand.clone();

    assert_eq!(
        install(&mut nand, InstallMode::FactoryReset),
        SettingsPlan::Reset
    );
    assert!(!volume_table(&mut nand).contains_key("overlay"));
    assert_attachable(&mut nand);

    // Same as the installer before settings could be kept: format everything, write volumes.
    let rootfs = new_rootfs();
    let mut image = &rootfs[..];
    let mut ebt = scan_blocks(&mut reference).unwrap();
    format(&mut reference, &mut ebt, &Default::default()).unwrap();
    write_volumes(
        &mut reference,
        &mut ebt,
        installer_volumes(&mut image, rootfs.len() as u64),
    )
    .unwrap();
    assert!(dump(&mut nand) == dump(&mut reference));
}

#[test]
fn test_refuse_no_overlay() {
    assert_refused_like_reset(Board::default().build(false), "no \"overlay\" volume");
}

#[test]
fn test_refuse_mismatched_vtbl_copies() {
    let mut nand = Board::default().build(true);
    let (id, mut record) = volume_table(&mut nand)["overlay"].clone();
    record.reserved_pebs += 1;
    rewrite_record(&mut nand, id, &record, &[1]);
    assert_refused_like_reset(nand, "disagree");
}

#[test]
fn test_refuse_corrupt_vtbl() {
    let mut nand = Board::default().build(true);
    let ebt = scan_blocks(&mut nand).unwrap();
    let peb = find_pebs(&ebt, LAYOUT_VOL_ID)[&0][0];
    rewrite_block(&mut nand, peb, |raw| {
        raw[2 * LAYOUT.bytes_per_page + 5] ^= 0x01
    });
    assert_refused_like_reset(nand, "corrupt");
}

#[test]
fn test_refuse_upd_marker() {
    let mut nand = Board::default().build(true);
    let (id, mut record) = volume_table(&mut nand)["overlay"].clone();
    record.upd_marker = true;
    rewrite_record(&mut nand, id, &record, &[0, 1]);
    assert_refused_like_reset(nand, "half-updated");
}

#[test]
fn test_refuse_static_overlay() {
    let mut nand = Board::default().build(true);
    let (id, mut record) = volume_table(&mut nand)["overlay"].clone();
    record.vol_type = VolType::Static;
    rewrite_record(&mut nand, id, &record, &[0, 1]);
    assert_refused_like_reset(nand, "not a dynamic volume");
}

#[test]
fn test_refuse_duplicate_name() {
    let mut nand = Board::default().build(true);
    let (_, record) = volume_table(&mut nand)["overlay"].clone();
    rewrite_record(&mut nand, 2, &record, &[0, 1]);
    assert_refused_like_reset(nand, "two volumes");
}

#[test]
fn test_refuse_no_ubifs() {
    let mut board = Board::default();
    board.overlay_data[..4].fill(0);
    assert_refused_like_reset(board.build(true), "UBIFS");
}

#[test]
fn test_refuse_image_seq_mismatch() {
    let mut nand = Board::default().build(true);
    let ebt = scan_blocks(&mut nand).unwrap();
    let peb = find_pebs(&ebt, Board::default().overlay_id)[&2][0];
    let BlockContent::EcData(ec, _) = ebt[peb as usize] else {
        unreachable!()
    };
    rewrite_block(&mut nand, peb, |raw| {
        let ec = Ec {
            image_seq: 0x1234,
            ..ec
        };
        ec.encode(raw).unwrap();
    });
    assert_refused_like_reset(nand, "image sequence");
}

#[test]
fn test_refuse_raw_vid() {
    let mut nand = Board::default().build(true);
    let ebt = scan_blocks(&mut nand).unwrap();
    let peb = ebt
        .iter()
        .position(|x| matches!(x, BlockContent::EcErased(_)))
        .unwrap() as u32;
    rewrite_block(&mut nand, peb, |raw| {
        raw.fill(0xFF);
        Vid::default().encode(raw).unwrap();
    });
    assert!(matches!(
        scan_blocks(&mut nand).unwrap()[peb as usize],
        BlockContent::RawVid(_)
    ));
    assert_refused_like_reset(nand, "v1.x");
}

#[test]
fn test_refuse_capacity_short() {
    // Fits on the board, but not with the bad-block reserve and margin on top.
    let board = Board {
        overlay_lebs: 232,
        ..Default::default()
    };
    assert_refused_like_reset(board.build(true), "not enough room");

    // A little smaller fits.
    let board = Board {
        overlay_lebs: 220,
        ..Default::default()
    };
    let mut nand = board.build(true);
    assert!(install(&mut nand, InstallMode::KeepSettings).keeps_settings());
    assert_attachable(&mut nand);
}

#[test]
fn test_refuse_overlay_at_uboot_env_id() {
    let board = Board {
        overlay_id: 0,
        uboot_env_id: 2,
        ..Default::default()
    };
    assert_refused_like_reset(board.build(true), "volume ID 0");
}

#[test]
fn test_capacity_boundary() {
    let mut nand = Board::default().build(true);
    let ebt = scan_blocks(&mut nand).unwrap();
    let preserved = find_preserved_volume(&mut nand, &ebt, SETTINGS_VOLUME).unwrap();

    // reserved + new + WL + EBA + bad-block reserve + margin == good PEBs: just fits.
    let fixed =
        preserved.record.reserved_pebs + 1 + 1 + bad_peb_limit(LAYOUT.blocks) + CAPACITY_MARGIN;
    let new_pebs = LAYOUT.blocks - fixed;
    assert!(check_capacity(&ebt, &preserved, new_pebs, LAYOUT.blocks).is_ok());
    assert!(check_capacity(&ebt, &preserved, new_pebs + 1, LAYOUT.blocks).is_err());

    // A bigger chip means a bigger bad-block reserve.
    assert!(check_capacity(&ebt, &preserved, new_pebs, LAYOUT.blocks * 2).is_err());
}

#[test]
fn test_bad_blocks_and_reserve() {
    let mut nand = Board::default().build(true);
    let mut ebt = scan_blocks(&mut nand).unwrap();
    let preserved = find_preserved_volume(&mut nand, &ebt, SETTINGS_VOLUME).unwrap();
    let limit = bad_peb_limit(LAYOUT.blocks);
    let fixed = preserved.record.reserved_pebs + 2 + limit + CAPACITY_MARGIN;
    let new_pebs = LAYOUT.blocks - fixed;
    assert!(check_capacity(&ebt, &preserved, new_pebs, LAYOUT.blocks).is_ok());

    // Up to the limit, a bad block costs a usable PEB but frees one from the reserve.
    let mark_bad = |ebt: &mut Ebt, n: usize| {
        for x in ebt
            .iter_mut()
            .filter(|x| matches!(x, BlockContent::EcErased(_)))
            .take(n)
        {
            *x = BlockContent::Bad;
        }
    };
    mark_bad(&mut ebt, limit as usize);
    assert!(check_capacity(&ebt, &preserved, new_pebs, LAYOUT.blocks).is_ok());

    // Past the limit, it only costs.
    mark_bad(&mut ebt, 1);
    assert!(check_capacity(&ebt, &preserved, new_pebs, LAYOUT.blocks).is_err());
}

#[test]
fn test_preserved_volume_id_and_autoresize() {
    let record = VolTableRecord {
        reserved_pebs: 10,
        alignment: 1,
        name: "overlay".into(),
        flags: 0x01,
        ..Default::default()
    };
    let tables = |new_autoresize: bool| {
        let mut new = BasicVolume::new(VolType::Dynamic)
            .name("other")
            .size(LEB as u64);
        if new_autoresize {
            new = new.autoresize();
        }
        let volumes: Vec<Box<dyn Volume>> = vec![
            Box::new(new),
            Box::new(PreservedVolume::new(0, record.clone())),
        ];
        let mut ubinizer = Ubinizer::new(volumes, (LEB as u32).try_into().unwrap()).unwrap();
        let mut data = Vec::new();
        let mut last = Vec::new();
        while let Some(vid) = ubinizer.next_block(&mut data).unwrap() {
            if vid.vol_id == LAYOUT_VOL_ID {
                last = data.clone();
            }
            data.clear();
        }
        let slot =
            |id: usize| match VolTableRecord::decode_slot(&last[id * VTBL_RECORD..][..VTBL_RECORD])
            {
                VtblSlot::Volume(record) => record,
                other => panic!("slot {id}: {other:?}"),
            };
        (slot(0), slot(1))
    };

    // The preserved volume keeps its ID even though it comes last; the other one goes around it.
    let (preserved, other) = tables(false);
    assert_eq!(preserved, record);
    assert_eq!(other.name, "other");

    // Only one volume may carry the autoresize flag; the new one wins.
    let (preserved, other) = tables(true);
    assert_eq!(preserved.flags, 0);
    assert_eq!(other.flags, 0x01);
}

#[test]
fn test_required_id_conflict() {
    let record = VolTableRecord {
        reserved_pebs: 1,
        alignment: 1,
        vol_type: VolType::Dynamic,
        ..Default::default()
    };
    let volumes: Vec<Box<dyn Volume>> = vec![
        Box::new(PreservedVolume::new(3, record.clone())),
        Box::new(PreservedVolume::new(3, record)),
    ];
    assert!(Ubinizer::new(volumes, (LEB as u32).try_into().unwrap()).is_err());
}

#[test]
fn test_refuse_blank_flash() {
    let mut nand = SimNand::new(LAYOUT);
    let mut reset = nand.clone();
    let plan = install(&mut nand, InstallMode::KeepSettings);
    assert!(
        matches!(&plan, SettingsPlan::CannotKeep(why) if why.contains("no UBI volumes")),
        "{plan:?}"
    );
    install(&mut reset, InstallMode::FactoryReset);
    assert!(dump(&mut nand) == dump(&mut reset));
    assert_attachable(&mut nand);
}
