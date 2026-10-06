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
        format(&mut nand, &mut ebt, None).unwrap();

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

/// Check an EC header the way `validate_ec_hdr` (drivers/mtd/ubi/io.c) does
fn assert_ec_valid(ec: &Ec) {
    let page = LAYOUT.bytes_per_page as u32;
    assert_eq!(ec.vid_hdr_offset, page, "bad VID header offset");
    assert_eq!(ec.data_offset, 2 * page, "bad data offset");
    assert!(ec.ec <= 0x7FFF_FFFF, "bad erase counter {}", ec.ec);
}

/// Check a VID header the way `validate_vid_hdr` (drivers/mtd/ubi/io.c) does
fn assert_vid_valid(vid: &Vid) {
    let leb = LEB as u32;
    let internal = vid.vol_id >= LAYOUT_VOL_ID;
    assert!(internal || vid.vol_id < 128, "bad vol_id {}", vid.vol_id);
    match internal {
        false => assert_eq!(vid.compat, 0, "bad compat"),
        true => assert!([1, 2, 4, 5].contains(&vid.compat), "bad compat"),
    }
    assert!(vid.data_pad < leb / 2, "bad data_pad");
    assert!(vid.data_size <= leb, "bad data_size");
    match vid.vol_type {
        VolType::Static => {
            assert!(vid.used_ebs != 0 && vid.data_size != 0);
            if vid.lnum < vid.used_ebs - 1 {
                assert_eq!(vid.data_size, leb - vid.data_pad);
            } else {
                assert!(vid.lnum < vid.used_ebs, "too high lnum");
            }
        }
        VolType::Dynamic => {
            match vid.copy_flag {
                false => assert!(vid.data_size == 0 && vid.data_crc == 0),
                true => assert!(vid.data_size != 0),
            }
            assert_eq!(vid.used_ebs, 0, "bad used_ebs");
        }
    }
}

/// Check what the kernel checks when attaching (drivers/mtd/ubi/attach.c, io.c, vtbl.c): every EC
/// and VID header valid, one image_seq, no two copies of a LEB with the same sqnum, a volume table
/// `vtbl_check` accepts (both copies equal, see [volume_table]), every VID header pointing to a
/// volume in the table with its LEB inside the volume and the same type, at most one autoresize
/// volume, and enough good PEBs for every reservation.
fn assert_attachable(nand: &mut SimNand) {
    let ebt = scan_blocks(nand).unwrap();
    let table = volume_table(nand);
    let by_id: BTreeMap<u32, &VolTableRecord> = table.values().map(|(id, r)| (*id, r)).collect();

    let mut image_seqs = BTreeSet::new();
    let mut copies = BTreeSet::new();
    for content in ebt.iter() {
        match content {
            BlockContent::EcErased(ec) => {
                assert_ec_valid(ec);
                image_seqs.insert(ec.image_seq);
            }
            BlockContent::EcData(ec, Some(vid)) => {
                assert_ec_valid(ec);
                assert_vid_valid(vid);
                image_seqs.insert(ec.image_seq);
                assert!(
                    copies.insert((vid.vol_id, vid.lnum, vid.sqnum)),
                    "two copies of {}:{} with sqnum {}",
                    vid.vol_id,
                    vid.lnum,
                    vid.sqnum
                );
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
    format(&mut reference, &mut ebt, None).unwrap();
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
    // It never keeps the autoresize flag, whether or not a new volume asks for it.
    let (preserved, other) = tables(false);
    assert_eq!(
        preserved,
        VolTableRecord {
            flags: 0,
            ..record.clone()
        }
    );
    assert_eq!(other.name, "other");
    assert_eq!(other.flags, 0);

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

// Flash content that UBI would reject, or that the parser must survive. Each of these started as a
// proof of concept from a security review, against which the installer either panicked or kept
// the settings in a way that left a UBI the kernel refuses to attach.

/// Rewrite the VID header of a block
fn rewrite_vid(nand: &mut SimNand, peb: u32, modify: impl FnOnce(&mut Vid)) {
    rewrite_block(nand, peb, |raw| {
        let page = LAYOUT.bytes_per_page;
        let mut vid = Vid::decode(&raw[page..]).unwrap();
        modify(&mut vid);
        vid.encode(&mut raw[page..2 * page]).unwrap();
    });
}

/// Write the same raw bytes into slot `id` of both copies of the volume table
fn rewrite_slot_raw(nand: &mut SimNand, id: usize, bytes: &[u8]) {
    let ebt = scan_blocks(nand).unwrap();
    for lnum in [0, 1] {
        let peb = find_pebs(&ebt, LAYOUT_VOL_ID)[&lnum][0];
        rewrite_block(nand, peb, |raw| {
            let at = 2 * LAYOUT.bytes_per_page + id * VTBL_RECORD;
            raw[at..at + VTBL_RECORD].copy_from_slice(bytes);
        });
    }
}

/// A raw volume table record with a correct CRC
fn raw_vtbl_record(modify: impl FnOnce(&mut [u8])) -> Vec<u8> {
    let mut r = vec![0u8; VTBL_RECORD];
    r[0..4].copy_from_slice(&5u32.to_be_bytes()); // reserved_pebs
    r[4..8].copy_from_slice(&1u32.to_be_bytes()); // alignment
    r[12] = 1; // dynamic
    r[14..16].copy_from_slice(&1u16.to_be_bytes()); // name_len
    r[16] = b'x';
    modify(&mut r);
    let crc = crate::ubi::UBI_CRC.checksum(&r[..VTBL_RECORD - 4]);
    r[VTBL_RECORD - 4..].copy_from_slice(&crc.to_be_bytes());
    r
}

/// The PEB holding the newest copy of an overlay LEB
fn overlay_peb(nand: &mut SimNand, lnum: u32) -> u32 {
    let ebt = scan_blocks(nand).unwrap();
    find_pebs(&ebt, Board::default().overlay_id)[&lnum][0]
}

#[test]
fn test_decode_slot_rejects_bad_names() {
    let name_len = |len: u16| raw_vtbl_record(|r| r[14..16].copy_from_slice(&len.to_be_bytes()));

    assert!(matches!(
        VolTableRecord::decode_slot(&raw_vtbl_record(|_| ())),
        VtblSlot::Volume(_)
    ));
    // name_len past the 128-byte field used to panic (slice out of range)
    for len in [0, 127, 128, 200, u16::MAX] {
        assert_eq!(
            VolTableRecord::decode_slot(&name_len(len)),
            VtblSlot::Corrupt,
            "{len}"
        );
    }
    // NUL inside the name, or no NUL right after it
    let nul_inside = raw_vtbl_record(|r| {
        r[14..16].copy_from_slice(&3u16.to_be_bytes());
        r[16..19].copy_from_slice(b"a\0b");
    });
    let unterminated = raw_vtbl_record(|r| {
        r[17] = b'y';
    });
    let bad_upd_marker = raw_vtbl_record(|r| r[13] = 2);
    let negative = raw_vtbl_record(|r| r[0..4].copy_from_slice(&0x8000_0000u32.to_be_bytes()));
    // An unused slot must be exactly the canonical empty record
    let odd_empty = raw_vtbl_record(|r| r[0..4].fill(0));
    for bytes in [
        nul_inside,
        unterminated,
        bad_upd_marker,
        negative,
        odd_empty,
    ] {
        assert_eq!(VolTableRecord::decode_slot(&bytes), VtblSlot::Corrupt);
    }
    assert_eq!(
        VolTableRecord::decode_slot(&VolTableRecord::none_into_bytes()),
        VtblSlot::Empty
    );
}

#[test]
fn test_refuse_name_len_out_of_range() {
    let mut nand = Board::default().build(true);
    rewrite_slot_raw(
        &mut nand,
        7,
        &raw_vtbl_record(|r| r[14..16].copy_from_slice(&200u16.to_be_bytes())),
    );
    assert_refused_like_reset(nand, "corrupt");
}

#[test]
fn test_refuse_bad_alignment() {
    for (alignment, data_pad) in [(0, 0), (LEB as u32 * 2, 0), (3, 0), (1, 1)] {
        let mut nand = Board::default().build(true);
        let (id, mut record) = volume_table(&mut nand)["overlay"].clone();
        record.alignment = alignment;
        record.data_pad = data_pad;
        rewrite_record(&mut nand, id, &record, &[0, 1]);
        assert_refused_like_reset(nand, "corrupt");
    }
}

#[test]
fn test_refuse_equal_sqnum_copies() {
    let mut nand = Board::default().build(true);
    let ebt = scan_blocks(&mut nand).unwrap();
    let copies = &find_pebs(&ebt, Board::default().overlay_id)[&1];
    let BlockContent::EcData(_, Some(newest)) = ebt[copies[0] as usize] else {
        unreachable!()
    };
    rewrite_vid(&mut nand, copies[1], |vid| vid.sqnum = newest.sqnum);
    assert_refused_like_reset(nand, "same sequence number");
}

#[test]
fn test_refuse_invalid_vid_headers() {
    let cases: [fn(&mut Vid); 5] = [
        |vid| vid.compat = 5,
        |vid| vid.data_size = 123,
        |vid| vid.data_crc = 1,
        |vid| vid.used_ebs = 1,
        |vid| {
            vid.copy_flag = true;
            vid.data_size = LEB as u32 + 1;
        },
    ];
    for modify in cases {
        let mut nand = Board::default().build(true);
        let peb = overlay_peb(&mut nand, 2);
        rewrite_vid(&mut nand, peb, modify);
        assert_refused_like_reset(nand, "VID header UBI would reject");
    }
}

#[test]
fn test_refuse_copy_flag_out_of_range() {
    // copy_flag 2 is rejected by the kernel, and by our decoder: the PEB then scans as data
    // without a usable VID header, which must not be silently dropped.
    let mut nand = Board::default().build(true);
    let peb = overlay_peb(&mut nand, 2);
    rewrite_block(&mut nand, peb, |raw| {
        let vid = &mut raw[LAYOUT.bytes_per_page..][..64];
        vid[6] = 2;
        let crc = crate::ubi::UBI_CRC.checksum(&vid[..60]);
        vid[60..].copy_from_slice(&crc.to_be_bytes());
    });
    assert!(matches!(
        scan_blocks(&mut nand).unwrap()[peb as usize],
        BlockContent::EcData(_, None)
    ));
    assert_refused_like_reset(nand, "damaged UBI header");
}

#[test]
fn test_refuse_erase_counter_out_of_range() {
    let mut nand = Board::default().build(true);
    let peb = overlay_peb(&mut nand, 0);
    rewrite_block(&mut nand, peb, |raw| {
        let ec = Ec::decode(raw).unwrap();
        Ec {
            ec: 0x8000_0000,
            ..ec
        }
        .encode(raw)
        .unwrap();
    });
    assert_refused_like_reset(nand, "unexpected UBI header");
}

#[test]
fn test_refuse_corrupt_ec_header_on_kept_peb() {
    // The kernel reads the VID header of a PEB whose EC header is damaged and uses the LEB; the
    // scan here calls the PEB garbage and `format` would erase it.
    let mut nand = Board::default().build(true);
    let peb = overlay_peb(&mut nand, 3);
    rewrite_block(&mut nand, peb, |raw| raw[10] ^= 0xFF);
    assert_eq!(
        scan_blocks(&mut nand).unwrap()[peb as usize],
        BlockContent::Garbage
    );
    assert_refused_like_reset(nand, "damaged UBI header");
}

#[test]
fn test_refuse_corrupt_ec_header_on_layout_peb() {
    let mut nand = Board::default().build(true);
    let ebt = scan_blocks(&mut nand).unwrap();
    let peb = find_pebs(&ebt, LAYOUT_VOL_ID)[&1][0];
    rewrite_block(&mut nand, peb, |raw| raw[10] ^= 0xFF);
    assert_refused_like_reset(nand, "volume table");
}

#[test]
fn test_refuse_huge_sqnum() {
    let mut nand = Board::default().build(true);
    let peb = overlay_peb(&mut nand, 4);
    rewrite_vid(&mut nand, peb, |vid| vid.sqnum = 1 << 63);
    assert_refused_like_reset(nand, "implausibly high");
}

/// A NAND that panics once armed, to stand in for a bug in discovery
struct PanickyNand(SimNand, bool);

impl crate::nand::Nand for PanickyNand {
    type Block<'a> = <SimNand as crate::nand::Nand>::Block<'a>;

    fn block(&mut self, index: u32) -> anyhow::Result<Option<Self::Block<'_>>> {
        assert!(!self.1, "simulated bug");
        self.0.block(index)
    }

    fn get_layout(&self) -> NandLayout {
        self.0.get_layout()
    }
}

#[test]
fn test_panic_in_discovery_is_a_refusal() {
    let mut nand = PanickyNand(Board::default().build(true), false);
    let ebt = scan_blocks(&mut nand).unwrap();
    nand.1 = true;
    let rootfs = new_rootfs();
    let mut image = &rootfs[..];
    let volumes = installer_volumes(&mut image, rootfs.len() as u64);
    let plan = plan(
        &mut nand,
        &ebt,
        InstallMode::KeepSettings,
        &volumes,
        LAYOUT.blocks,
    );
    assert_eq!(
        plan,
        SettingsPlan::CannotKeep("internal error during discovery: simulated bug".into())
    );
}

#[test]
fn test_format_rechecks_the_plan() {
    let mut nand = Board::default().build(true);
    let mut ebt = scan_blocks(&mut nand).unwrap();
    let mut preserved = find_preserved_volume(&mut nand, &ebt, SETTINGS_VOLUME).unwrap();
    let before = dump(&mut nand);

    // A prototype other than the one the flash gives
    preserved.proto.image_seq ^= 1;
    assert!(format(&mut nand, &mut ebt, Some(&preserved)).is_err());
    preserved.proto.image_seq ^= 1;

    // A kept PEB that is no longer what discovery saw
    let peb = *preserved.pebs.first().unwrap();
    let saved = ebt[peb as usize];
    ebt[peb as usize] = BlockContent::Garbage;
    assert!(format(&mut nand, &mut ebt, Some(&preserved)).is_err());
    ebt[peb as usize] = saved;

    // Nothing was erased
    assert!(dump(&mut nand) == before);
    assert!(format(&mut nand, &mut ebt, Some(&preserved)).is_ok());
}

#[test]
fn test_duplicate_names_fail_before_writing() {
    let mut nand = SimNand::new(LAYOUT);
    let mut ebt = scan_blocks(&mut nand).unwrap();
    format(&mut nand, &mut ebt, None).unwrap();
    let before = dump(&mut nand);

    let mut a = &[1u8; LEB][..];
    let volumes: Vec<Box<dyn Volume + '_>> = vec![
        Box::new(
            BasicVolume::new(VolType::Dynamic)
                .name("same")
                .size(LEB as u64)
                .image(&mut a),
        ),
        Box::new(BasicVolume::new(VolType::Dynamic).name("same")),
    ];
    assert!(write_volumes(&mut nand, &mut ebt, volumes).is_err());
    assert!(dump(&mut nand) == before);
}

#[test]
fn test_sqnum_exhaustion_is_an_error() {
    let mut a = &[1u8; LEB][..];
    let volumes: Vec<Box<dyn Volume + '_>> = vec![Box::new(
        BasicVolume::new(VolType::Dynamic)
            .name("a")
            .size(LEB as u64)
            .image(&mut a),
    )];
    let mut ubinizer = Ubinizer::new(volumes, (LEB as u32).try_into().unwrap())
        .unwrap()
        .sqnum_base(u64::MAX);
    assert!(ubinizer.next_block(&mut Vec::new()).is_err());
}

#[test]
fn test_factory_reset_cmdline() {
    for cmdline in [
        "factory_reset",
        "console=ttyS0 factory_reset loglevel=4",
        "factory_reset=1",
        "factory_reset=y",
        "factory_reset=YES",
        "factory_reset=True",
        "factory_reset=0 factory_reset",
    ] {
        assert!(factory_reset_requested(cmdline), "{cmdline:?}");
    }
    for cmdline in [
        "",
        "loglevel=4",
        "nofactory_reset",
        "factory_reset=0",
        "factory_reset=n",
        "factory_reset=No",
        "factory_reset=FALSE",
        "factory_reset=",
        "factory_reset=maybe",
        "factory_resets",
        "FACTORY_RESET_please",
        "factory_reset factory_reset=0",
    ] {
        assert!(!factory_reset_requested(cmdline), "{cmdline:?}");
    }
}
