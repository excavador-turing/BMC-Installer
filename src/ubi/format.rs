//! This module implements the reformatting/erasing logic.

use super::headers::{Ec, UBI_MAX_ERASECOUNTER};
use super::preserve::Preserved;
use super::scan::{BlockContent, Ebt};
use super::ubinize::{Ubinizer, Volume};

use crate::nand::{Nand, NandBlock, NandLayout, PageUtil};

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::num::NonZeroU32;

/// These are the actions that may be taken on each block to migrate away from SIMULATE_MULTIPLANE;
/// this type implements the "command pattern"
#[derive(Debug, Eq, PartialEq, Copy, Clone)]
enum FormatAction {
    /// Do nothing
    Ignore,

    /// Write an EC header; only valid for `Erased` blocks
    Write(Ec),

    /// Erase the block, then write an EC header
    Erase(Ec),
}

impl FormatAction {
    /// Run the action on the specified NAND block
    fn execute<B: NandBlock>(self, mut block: B, content: &mut BlockContent) -> anyhow::Result<()> {
        let (erase, ec) = match self {
            Self::Ignore => return Ok(()),
            Self::Write(x) => (false, x),
            Self::Erase(x) => (true, x),
        };

        if erase {
            let erase_result = block.erase();
            if erase_result.is_err() {
                // Error when trying to erase means the block is definitely bad
                *content = BlockContent::Bad;
                return block.mark_bad();
            }
        }

        let mut hdr_bytes = vec![0; block.page_size()];
        ec.encode(&mut hdr_bytes)?;

        let program_result = block.program(0, &hdr_bytes);
        match (program_result, erase) {
            // An error when we weren't trying to erase is probably from the block being in an
            // unclean state; promote this to an `Erase` and try again:
            (Err(_), false) => Self::Erase(ec.inc_ec()).execute(block, content),

            // An error when we *were* trying to erase is a sign of a bad block.
            (Err(_), true) => {
                *content = BlockContent::Bad;
                block.mark_bad()
            }

            // Success means the block is erased
            (Ok(_), _) => {
                *content = BlockContent::EcErased(ec);
                Ok(())
            }
        }
    }
}

/// Determine what formatting action needs to be taken to erase a block in a given state
fn erase_action(content: BlockContent, ec_proto: Ec) -> FormatAction {
    use BlockContent::*;
    use FormatAction::*;

    match content {
        // Bad blocks can't have anything done with them
        Bad => Ignore,

        // We can ignore any empty blocks with ECs that already match the prototype's layout fields
        // (and that UBI will accept: an erase counter out of range fails the whole attach)
        EcErased(x) if x == ec_proto.ec(x.ec) && x.ec <= UBI_MAX_ERASECOUNTER => Ignore,

        // Otherwise, we have to do something.

        // Fully-erased blocks should have the prototypical EC written in, no erase needed first
        Erased => Write(ec_proto),

        // If we know the EC, erase and use that. Otherwise, just use the prototypical EC, which
        // holds the mean erase count.
        EcData(x, _) | EcErased(x) => Erase(ec_proto.ec(x.ec).inc_ec()),
        RawVid(_) | Garbage => Erase(ec_proto),
    }
}

/// Determine what actions to take on a pair of blocks (forming a superblock) to migrate away from
/// SIMULATE_MULTIPLANE.
///
/// Returns a pair of [FormatAction]s for the even and odd block, respectively.
fn migrate_superblock_action(
    even: BlockContent,
    odd: BlockContent,
    ec_proto: Ec,
) -> [FormatAction; 2] {
    use BlockContent::*;
    use FormatAction::*;

    // Determine the action to take on the even block; this is a standard UBI erase:
    let even_action = erase_action(even, ec_proto);

    // Determine the action to take on the odd block, depending on the even block:
    let odd_action = match (even, odd) {
        // Bad blocks can't have anything done with them
        (_, Bad) => Ignore,

        // If there's already an EC in the odd block, no special even-block analysis is required
        (_, EcErased(x)) if x == ec_proto.ec(x.ec) && x.ec <= UBI_MAX_ERASECOUNTER => Ignore,
        (_, EcErased(x) | EcData(x, _)) => Erase(ec_proto.ec(x.ec).inc_ec()),

        // Copy superblock EC (from even physical block) to odd block
        (EcErased(x) | EcData(x, _), Erased) => Write(ec_proto.ec(x.ec.min(UBI_MAX_ERASECOUNTER))),
        (EcErased(x) | EcData(x, _), RawVid(_) | Garbage) => Erase(ec_proto.ec(x.ec).inc_ec()),

        // When the superblock EC cannot be copied, just use the prototypical EC header:
        (_, Erased) => Write(ec_proto),
        (_, RawVid(_) | Garbage) => Erase(ec_proto),
    };

    [even_action, odd_action]
}

/// The LEB size the installer formats for: the full block, minus the first 2 pages (EC and VID).
pub fn leb_size(layout: NandLayout) -> NonZeroU32 {
    let eb_size = layout.bytes_per_page as u32 * (layout.pages_per_block - 2);
    eb_size.try_into().expect("LEB size must be nonzero")
}

/// Figure out the "prototype" EC header. That is, the header that should be written to every PEB
/// in the UBI partition.
///
/// The image_seq is the most common one (ties go to the lowest value, so that the result does not
/// depend on hash order), and the erase counter is the mean, clamped to what UBI accepts. Every
/// value comes from flash, so nothing here may overflow.
pub(super) fn compute_prototype(
    layout: NandLayout,
    blocks: impl Iterator<Item = BlockContent>,
) -> anyhow::Result<Ec> {
    let page_size: u32 = layout.bytes_per_page.try_into()?;

    // Find the mode of image_seq so that we can reuse most of the EC headers without erasing.
    let mut image_seq_ctrs = HashMap::new();

    // Find the mean EC value
    let mut ec_sum: u128 = 0;
    let mut ec_count: u128 = 0;

    for content in blocks {
        let echdr = match content {
            BlockContent::EcErased(x) => x,
            BlockContent::EcData(x, _) => x,
            _ => continue,
        };

        // Add a tally to the number of times `echdr.image_seq` is seen
        *image_seq_ctrs.entry(echdr.image_seq).or_insert(0u64) += 1;

        ec_sum += u128::from(echdr.ec);
        ec_count += 1;
    }

    // Determine the mode of `echdr.image_seq`
    let image_seq = image_seq_ctrs
        .into_iter()
        .max_by_key(|&(k, v)| (v, std::cmp::Reverse(k))) // most hits, then lowest value
        .map_or(0, |(k, _)| k); // get only the key (or 0 if the HashMap is empty)

    // Compute mean EC value, rounded to nearest integer, or 1 if ec_count == 0
    let ec = (ec_sum + ec_count / 2).checked_div(ec_count).unwrap_or(1);
    let ec = ec.min(u128::from(UBI_MAX_ERASECOUNTER)) as u64;

    Ok(Ec {
        vid_hdr_offset: page_size,
        data_offset: page_size * 2,

        ec,
        image_seq,
    })
}

/// Reformat the UBI partition, performing AWNAND `SIMULATE_MULTIPLANE` migration (if deemed
/// necessary), otherwise do regular UBI erase.
///
/// The PEBs of `keep` are left exactly as they are (see [super::find_preserved_volume]); pass
/// `None` to erase everything. When keeping, the prototype EC header is the one discovery checked
/// the kept PEBs against, and this re-checks, before erasing anything, that it still is what the
/// flash gives and that every kept PEB still matches it. Keeping PEBs is refused when migration is
/// needed, because migration rewrites the geometry under them.
///
/// This does not write the layout volume, so it is not sufficient for UBI to accept the partition.
pub fn format<N: Nand>(
    nand: &mut N,
    ebt: &mut Ebt,
    keep: Option<&Preserved>,
) -> anyhow::Result<()> {
    let rpt = howudoin::new().label("Erasing blocks");

    let proto = compute_prototype(nand.get_layout(), ebt.iter().copied())?;
    let empty = Default::default();
    let keep_pebs = match keep {
        None => &empty,
        Some(preserved) => {
            anyhow::ensure!(
                preserved.proto == proto,
                "the flash changed since it was analyzed (UBI header prototype differs)",
            );
            for &peb in &preserved.pebs {
                let matches = match ebt.get(peb as usize) {
                    Some(BlockContent::EcData(ec, Some(_))) => *ec == proto.ec(ec.ec),
                    _ => false,
                };
                anyhow::ensure!(matches, "kept PEB {peb} changed since it was analyzed");
            }
            &preserved.pebs
        }
    };

    let needs_migration = ebt.iter().any(|x| matches!(x, BlockContent::RawVid(_)));
    anyhow::ensure!(
        keep_pebs.is_empty() || !needs_migration,
        "cannot keep any blocks while migrating away from SIMULATE_MULTIPLANE",
    );
    let work: VecDeque<(u32, FormatAction)> = if needs_migration {
        rpt.add_info("AWNAND SIMULATE_MULTIPLANE layout detected, performing migration");

        let mut work = VecDeque::new();
        for (i, action) in ebt
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&[even, odd]| migrate_superblock_action(even, odd, proto))
            .enumerate()
            .filter(|&(_, action)| action != FormatAction::Ignore)
        {
            // Is this one of the superblocks making us need migration? If so, handle it last, so
            // that if this process is interrupted (e.g. by power loss) we resume the correct
            // operation.
            let is_vid = [ebt[i], ebt[i ^ 1]]
                .iter()
                .any(|x| matches!(x, BlockContent::RawVid(_)));

            if is_vid {
                work.push_back((i as u32, action));
            } else {
                work.push_front((i as u32, action));
            }
        }

        work
    } else {
        // Migration not needed, just do a regular erase
        ebt.iter()
            .enumerate()
            .map(|(i, s)| match keep_pebs.contains(&(i as u32)) {
                true => (i as u32, FormatAction::Ignore),
                false => (i as u32, erase_action(*s, proto)),
            })
            .filter(|&(_, action)| action != FormatAction::Ignore)
            .collect()
    };

    rpt.set_len(u64::try_from(work.len()).ok());
    if !work.is_empty() {
        for (block, action) in work.into_iter() {
            let content = &mut ebt[block as usize];

            action.execute(
                nand.block(block)?
                    .ok_or(anyhow::anyhow!("Block unexpectedly marked bad"))?,
                content,
            )?;
            rpt.inc();
        }
    }

    rpt.close();

    Ok(())
}

/// Use the `ubinize` module to write UBI volumes to the flash device.
///
/// Any PEB still holding data in `ebt` (that is, kept by [format]) is left alone, and the new VID
/// headers are numbered above the highest sqnum found in them.
pub fn write_volumes<'a, N, V>(nand: &mut N, ebt: &mut Ebt, volumes: V) -> anyhow::Result<()>
where
    N: Nand,
    V: IntoIterator<Item = Box<dyn Volume + 'a>>,
    for<'x> &'x V: IntoIterator<Item = &'x V::Item>,
{
    // Compute the EB size. This is the full block size, minus the first 2 pages (for EC and VID).
    let layout = nand.get_layout();
    let eb_size = leb_size(layout);

    // The highest sqnum among the PEBs that stay on flash.
    let max_kept_sqnum = ebt
        .iter()
        .filter_map(|content| match content {
            BlockContent::EcData(_, Some(vid)) => Some(vid.sqnum),
            _ => None,
        })
        .max()
        .unwrap_or(0);

    // Estimate the needed blocks to complete the flashing operation.
    let blocks = Ubinizer::estimate_blocks((&volumes).into_iter().map(|x| &**x), eb_size);

    // Scan the ebt for only EcErased blocks (ignore all others) and sort them by a percentile of
    // the EC value. The reason we use a percentile is so that there's still decent wear-leveling,
    // but we don't crowd lots of (probably static) blocks onto low EC blocks where they're likely
    // to get moved by UBI's own wear-leveling algorithm anyway.
    const PERCENTILE: usize = 25;
    let mut blocks_by_ec: BTreeMap<u64, Vec<u32>> = BTreeMap::new();

    ebt.iter()
        .enumerate()
        .filter_map(|(i, content)| match content {
            BlockContent::EcErased(ec) => Some((i as u32, ec.ec)),
            _ => None,
        })
        .for_each(|(block, ec)| {
            blocks_by_ec.entry(ec).or_default().push(block);
        });

    let mut block_ordering = std::iter::from_fn(move || {
        let sum: usize = blocks_by_ec.values().map(|x| x.len()).sum();
        let mut threshold = sum * PERCENTILE / 100;
        blocks_by_ec
            .iter()
            .find_map(|(&k, v)| {
                if v.len() >= threshold {
                    Some(k)
                } else {
                    threshold -= v.len();
                    None
                }
            })
            .and_then(|percentile_ec| blocks_by_ec.remove(&percentile_ec))
    })
    .flatten();

    // Begin ubinizing volumes
    let mut ubinizer = Ubinizer::new(volumes, eb_size)?.sqnum_base(max_kept_sqnum);
    let vid_size = layout.bytes_per_page;
    let mut data = Vec::with_capacity(u32::from(eb_size) as usize + vid_size);
    data.resize(vid_size, 0u8);

    // Iterate over all logical blocks provided by the Ubinizer
    let rpt = howudoin::new()
        .label("Programming blocks")
        .set_len(u64::from(blocks));
    while let Some(vid) = ubinizer.next_block(&mut data)? {
        // Prepare the `data` buffer: first, pad it to a multiple of the page size
        let mut size = data.len() + layout.bytes_per_page - 1;
        size -= size % layout.bytes_per_page;
        data.resize(size, 0xFFu8);

        // Writing an "erased" (all-0xFF) page is (theoretically, at least) a no-op. So, as a
        // simple optimization, strip off any erased page(s) from the end.
        loop {
            if data.is_empty() {
                break;
            }
            let minus_last_page = data.len() - layout.bytes_per_page;
            if data[minus_last_page..].is_erased() {
                data.truncate(minus_last_page);
            } else {
                break;
            }
        }

        // Prepare the VID header to be written out.
        vid.encode(&mut data[..vid_size])?;

        // Loop until the logical block is successfully written. This is a loop because the
        // physical block may end up getting marked bad, and new physical blocks will have to be
        // selected until the logical block can be written.
        'write_loop: loop {
            // Select physical block to write into
            let (block_id, ebt_entry, ec) = loop {
                let block_id = block_ordering
                    .next()
                    .ok_or(anyhow::anyhow!("Flash is full"))?;
                let ebt_entry = &mut ebt[block_id as usize];
                let ec = match *ebt_entry {
                    BlockContent::EcErased(ec) => ec,
                    _ => unreachable!(),
                };
                match nand.block(block_id)? {
                    Some(_) => break (block_id, ebt_entry, ec),
                    None => {
                        // Guess it went bad? Try again...
                        *ebt_entry = BlockContent::Bad;
                        continue;
                    }
                };
            };

            // Try to write the block; if that fails, erase it and try again; if that still fails,
            // mark the block bad.
            let mut tried_erase = false;
            loop {
                let mut block = nand.block(block_id)?.expect("block went bad on its own");
                if block.program(1, &data).is_ok() {
                    *ebt_entry = BlockContent::EcData(ec, Some(vid));

                    // Success! Move on to the next logical block.
                    break 'write_loop;
                }

                if tried_erase {
                    // Block just doesn't want to be written; it's bad.
                    block.mark_bad()?;
                    *ebt_entry = BlockContent::Bad;
                    break;
                } else {
                    // Erase the block before trying again.
                    FormatAction::Erase(ec.inc_ec()).execute(block, ebt_entry)?;
                    tried_erase = true;
                }
            }
        }

        rpt.inc();
        data.truncate(vid_size);
    }

    rpt.close();

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    use super::super::scan_blocks;
    use crate::nand::{NandBlock, NandLayout, SimNand};

    const TEST_LAYOUT: NandLayout = NandLayout {
        blocks: 16,
        pages_per_block: 16,
        bytes_per_page: 128,
    };

    #[test]
    fn test_format_blank() -> anyhow::Result<()> {
        let mut nand = SimNand::new(TEST_LAYOUT);

        let mut ebt = scan_blocks(&mut nand)?;
        format(&mut nand, &mut ebt, None)?;

        // Make sure `format` updated `ebt`:
        let ebt2 = scan_blocks(&mut nand)?;
        assert_eq!(ebt, ebt2);

        Ok(())
    }

    #[test]
    fn test_prototype_survives_any_flash() {
        let ec = |ec: u64, image_seq: u32| {
            BlockContent::EcErased(Ec {
                ec,
                image_seq,
                ..Default::default()
            })
        };

        // Erase counters that would overflow a u64 sum: the mean is clamped, nothing panics.
        let blocks = [ec(u64::MAX, 7), ec(u64::MAX, 7), ec(u64::MAX - 1, 7)];
        let proto = compute_prototype(TEST_LAYOUT, blocks.into_iter()).unwrap();
        assert_eq!(proto.ec, UBI_MAX_ERASECOUNTER);
        assert_eq!(proto.image_seq, 7);

        // A tie between image_seqs goes to the lowest, whatever the hash order.
        for _ in 0..16 {
            let blocks = [ec(1, 9), ec(1, 3), ec(1, 9), ec(1, 3), ec(1, 5)];
            let proto = compute_prototype(TEST_LAYOUT, blocks.into_iter()).unwrap();
            assert_eq!(proto.image_seq, 3);
        }
    }

    #[test]
    fn test_format_clamps_erase_counters() -> anyhow::Result<()> {
        let mut nand = SimNand::new(TEST_LAYOUT);
        let mut buf = vec![0xFFu8; TEST_LAYOUT.bytes_per_page];
        for (i, value) in [u64::MAX, UBI_MAX_ERASECOUNTER + 1, UBI_MAX_ERASECOUNTER]
            .into_iter()
            .enumerate()
        {
            let page = TEST_LAYOUT.bytes_per_page as u32;
            Ec {
                ec: value,
                vid_hdr_offset: page,
                data_offset: 2 * page,
                image_seq: 0,
            }
            .encode(&mut buf)?;
            nand.block(i as u32)?.unwrap().program(0, &buf)?;
        }

        let mut ebt = scan_blocks(&mut nand)?;
        format(&mut nand, &mut ebt, None)?;
        for content in scan_blocks(&mut nand)?.iter() {
            match content {
                BlockContent::EcErased(ec) => assert!(ec.ec <= UBI_MAX_ERASECOUNTER),
                other => panic!("unexpected {other:?}"),
            }
        }

        Ok(())
    }
}
