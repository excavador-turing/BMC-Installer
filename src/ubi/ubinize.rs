//! This module implements volume flashing support.
//!
//! The user specifies a series of volumes as any iterable (i.e. implementing IntoIterator) type.
//! The code here will, when told the LEB size by the consumer, iterate over the volumes and
//! yield `(Vid, Vec<u8>)` pairs for each LEB. This also takes care of synthesizing the layout
//! volume.

use super::headers::{OptionIntoBytes, Vid, VolTableRecord, VolType, UBI_CRC};
use crate::util::ReadExt;

use anyhow::{bail, ensure};
use std::collections::BTreeSet;
use std::io::Read;
use std::num::NonZeroU32;

/// Represents a UBI volume to be written to flash or an image file
pub trait Volume {
    /// Begin reading the data for the `Volume`, chunked so as to fit within a given eraseblock
    /// size.
    fn into_data<'a>(self: Box<Self>, eb_size: NonZeroU32, vol_id: u32) -> Box<dyn VolumeData + 'a>
    where
        Self: 'a;

    /// Get the *preferred* volume ID that this `Volume` would like to use, if any.
    ///
    /// Note that the caller may choose to ignore this and supply its own `vol_id` to `into_data`,
    /// especially if there is a conflict.
    fn get_vol_id(&self) -> Option<u32>;

    /// Whether the ID from [Volume::get_vol_id] is mandatory rather than a preference.
    ///
    /// A volume that already lives on flash (see [PreservedVolume]) is identified by its index in
    /// the volume table, so it cannot be moved; every other volume is allocated around it.
    fn requires_vol_id(&self) -> bool {
        false
    }

    /// Estimate how many blocks this `Volume` will occupy at the given `eb_size`.
    ///
    /// This is an estimate only; its accuracy is not enforced.
    fn estimate_blocks(&self, eb_size: NonZeroU32) -> u32;
}

/// A provider of data for a single volume of an image
pub trait VolumeData {
    /// Try to determine the next block that should be written as part of this volume.
    ///
    /// The block's data will be *appended* to `data`, so space for headers may be pre-reserved by
    /// the caller if desired.
    ///
    /// On success, the result will be `Some(Vid)`, or `None` if there are no further blocks.
    /// The `Vid` will not have the sqnum set to anything in particular; the caller must
    /// override this.
    fn next_block(&mut self, data: &mut Vec<u8>) -> anyhow::Result<Option<Vid>>;

    /// Generate a volume table record for this `VolumeData`.
    ///
    /// This consumes the `VolumeData`, so it must be done after iterating over blocks. This is
    /// because the `VolumeData` may not know its size until after the blocks are written.
    fn into_vtbl_record(self: Box<Self>) -> VolTableRecord;
}

pub(super) const UBI_LAYOUT_VOLUME_ID: u32 = 0x7FFFEFFF;
const UBI_LAYOUT_VOLUME_TYPE: VolType = VolType::Dynamic;
const UBI_LAYOUT_VOLUME_EBS: u32 = 2;
const UBI_LAYOUT_VOLUME_COMPAT: u8 = 5u8;

pub(super) const UBI_VTBL_RECORD_SIZE: usize = 0xAC;
const UBI_MAX_VOLUMES: usize = 128;

/// The UBI volume table flag that asks UBI to grow the volume over all free PEBs at attach time.
const UBI_VTBL_AUTORESIZE_FLG: u8 = 0x01;

/// The number of records in the volume table, for a given LEB size.
///
/// This mirrors `ubi->vtbl_slots` in the kernel (drivers/mtd/ubi/build.c, `io_init`).
pub(super) fn vtbl_slots(eb_size: NonZeroU32) -> usize {
    std::cmp::min(
        (u32::from(eb_size) as usize) / UBI_VTBL_RECORD_SIZE,
        UBI_MAX_VOLUMES,
    )
}

/// An internal volume, describing the layout of volumes on flash.
struct LayoutVolume {
    records: Vec<Option<VolTableRecord>>,

    /// IDs of volumes carried over from the previous layout, see [PreservedVolume]
    preserved_ids: BTreeSet<u32>,
}

impl LayoutVolume {
    /// Begin building a new layout volume. The EB size must be known ahead of time.
    fn new(eb_size: NonZeroU32) -> Self {
        let records = vec![Default::default(); vtbl_slots(eb_size)];

        Self {
            records,
            preserved_ids: Default::default(),
        }
    }

    /// Store a volume table record
    ///
    /// Panics if the provided `id` is not available
    fn store_record(&mut self, id: u32, record: VolTableRecord) {
        assert!(self.records[id as usize].replace(record).is_none());
    }
}

impl Volume for LayoutVolume {
    fn into_data<'a>(self: Box<Self>, eb_size: NonZeroU32, vol_id: u32) -> Box<dyn VolumeData + 'a>
    where
        Self: 'a,
    {
        let data_size = UBI_VTBL_RECORD_SIZE * self.records.len();
        assert!(data_size <= u32::from(eb_size) as usize);
        assert_eq!(vol_id, UBI_LAYOUT_VOLUME_ID);

        let vid = Vid {
            vol_id: UBI_LAYOUT_VOLUME_ID,
            vol_type: UBI_LAYOUT_VOLUME_TYPE,
            compat: UBI_LAYOUT_VOLUME_COMPAT,
            lnum: 0,
            ..Default::default()
        };

        // UBI refuses to attach when more than one volume carries the autoresize flag (see
        // `init_volumes` in drivers/mtd/ubi/vtbl.c). A newly written volume asking for it wins;
        // a preserved volume has long since been resized by the kernel anyway.
        let LayoutVolume {
            mut records,
            preserved_ids,
        } = *self;
        let new_autoresize = records.iter().enumerate().any(|(id, record)| {
            !preserved_ids.contains(&(id as u32))
                && record
                    .as_ref()
                    .is_some_and(|r| r.flags & UBI_VTBL_AUTORESIZE_FLG != 0)
        });
        if new_autoresize {
            for &id in &preserved_ids {
                if let Some(record) = records[id as usize].as_mut() {
                    record.flags &= !UBI_VTBL_AUTORESIZE_FLG;
                }
            }
        }

        let mut data = Vec::with_capacity(data_size);
        records
            .into_iter()
            .for_each(|record| data.append(&mut record.into_bytes()));
        assert_eq!(data.len(), data_size);

        Box::new(LayoutVolumeData { vid, data })
    }

    fn get_vol_id(&self) -> Option<u32> {
        Some(UBI_LAYOUT_VOLUME_ID)
    }

    fn estimate_blocks(&self, _: NonZeroU32) -> u32 {
        UBI_LAYOUT_VOLUME_EBS
    }
}

struct LayoutVolumeData {
    vid: Vid,
    data: Vec<u8>,
}

impl VolumeData for LayoutVolumeData {
    fn next_block(&mut self, data: &mut Vec<u8>) -> anyhow::Result<Option<Vid>> {
        if self.vid.lnum >= UBI_LAYOUT_VOLUME_EBS {
            return Ok(None);
        }

        let vid = self.vid;
        self.vid.lnum += 1;

        data.extend(&self.data);
        Ok(Some(vid))
    }

    fn into_vtbl_record(self: Box<Self>) -> VolTableRecord {
        panic!("tried to query volume table record for layout volume");
    }
}

/// A non-internal volume, the contents of which come from an image or are initially blank
pub struct BasicVolume<'a> {
    image: Option<&'a mut dyn Read>,
    vtype: VolType,
    id: Option<u32>,
    size: Option<u64>,
    name: String,
    flags: u8,
    alignment: NonZeroU32,
}

impl Default for BasicVolume<'_> {
    fn default() -> Self {
        Self {
            image: Default::default(),
            vtype: Default::default(),
            id: Default::default(),
            size: Default::default(),
            name: Default::default(),
            flags: Default::default(),
            alignment: NonZeroU32::new(1).unwrap(),
        }
    }
}

impl<'a> BasicVolume<'a> {
    /// Begin creating a new `BasicVolume`, of a given type
    pub fn new(vtype: VolType) -> Self {
        Self {
            vtype,
            ..Default::default()
        }
    }

    /// Change the source of the volume's contents.
    pub fn image(mut self, image: &'a mut dyn Read) -> Self {
        self.image = Some(image);
        self
    }

    /// Change the ID assigned to the volume from a default of auto-assigned.
    ///
    /// Note that this may be ignored if the ID is invalid.
    pub fn id(mut self, id: u32) -> Self {
        self.id = Some(id);
        self
    }

    /// Set the size, in bytes, allocated to the volume.
    ///
    /// The actual volume size will be at least this large, but will be rounded-up to the next
    /// multiple of the LEB size.
    ///
    /// The default is to learn the size from the `image`, or 0 if there is no image.
    pub fn size(mut self, bytes: u64) -> Self {
        self.size = Some(bytes);
        self
    }

    /// Set the name of the volume.
    ///
    /// The default is `""`
    pub fn name<S: Into<String>>(mut self, name: S) -> Self {
        self.name = name.into();
        self
    }

    /// Set the UBI "autoresize" flag.
    pub fn autoresize(mut self) -> Self {
        self.flags |= 0x01;
        self
    }

    /// Set the UBI "skip CRC check" flag.
    pub fn skipcheck(mut self) -> Self {
        self.flags |= 0x02;
        self
    }

    /// Set the alignment of the volume. All LEBs will be a multiple of this size, and will
    /// therefore begin at offsets that are multiples of this size.
    ///
    /// The default alignment is 1.
    pub fn align(mut self, alignment: NonZeroU32) -> Self {
        self.alignment = alignment;
        self
    }
}

impl Volume for BasicVolume<'_> {
    fn into_data<'a>(self: Box<Self>, eb_size: NonZeroU32, vol_id: u32) -> Box<dyn VolumeData + 'a>
    where
        Self: 'a,
    {
        if self.vtype == VolType::Static {
            assert!(
                self.size.is_some(),
                "size MUST be specified for Static volumes",
            );
        }

        // Compute this volume's layout, now that eb_size is known:
        let used_ebs = match self.vtype {
            VolType::Dynamic => 0,
            VolType::Static => self.estimate_blocks(eb_size), // Guaranteed correct for `Static`
        };
        let data_pad = u32::from(eb_size) % self.alignment;
        let leb_size = u32::from(eb_size) - data_pad;

        let vid = Vid {
            vol_type: self.vtype,
            copy_flag: false,
            vol_id,
            used_ebs,
            data_pad,
            ..Default::default()
        };

        let record = VolTableRecord {
            reserved_pebs: self.estimate_blocks(eb_size),
            alignment: self.alignment.into(),
            data_pad,
            vol_type: self.vtype,
            upd_marker: false,
            name: self.name,
            flags: self.flags,
        };

        // If a size was provided, limit how much we read from `image` to that size:
        let size = self.size.unwrap_or(u64::MAX);
        let image = self.image.map(|image| image.take(size));

        let data = BasicVolumeData {
            image,
            leb_size,
            vid,
            record,
        };

        Box::new(data)
    }

    fn get_vol_id(&self) -> Option<u32> {
        self.id
    }

    fn estimate_blocks(&self, eb_size: NonZeroU32) -> u32 {
        let eb_size: u32 = eb_size.into();
        let data_pad = eb_size % self.alignment;
        let leb_size = eb_size - data_pad;
        ((self.size.unwrap_or(0) + (leb_size - 1) as u64) / leb_size as u64) as u32
    }
}

struct BasicVolumeData<'a> {
    image: Option<std::io::Take<&'a mut dyn Read>>,
    leb_size: u32,
    vid: Vid,
    record: VolTableRecord,
}

impl VolumeData for BasicVolumeData<'_> {
    fn next_block(&mut self, data: &mut Vec<u8>) -> anyhow::Result<Option<Vid>> {
        let image = match &mut self.image {
            Some(image) => image,
            None => return Ok(None),
        };

        let data_len = data.len();
        image.read_to_vec(data, self.leb_size as usize)?;
        let new_data = &data[data_len..];

        if new_data.is_empty() {
            return Ok(None);
        }

        let mut vid = self.vid;
        self.vid.lnum += 1;

        if vid.vol_type == VolType::Static {
            vid.data_size = new_data.len() as u32;
            vid.data_crc = UBI_CRC.checksum(new_data);
        }

        Ok(Some(vid))
    }

    fn into_vtbl_record(self: Box<Self>) -> VolTableRecord {
        let mut record = self.record;
        if record.reserved_pebs == 0 {
            record.reserved_pebs = self.vid.lnum;
        }
        record
    }
}

/// A volume that already exists on flash and must survive the reimaging untouched.
///
/// Its PEBs are left alone by [crate::ubi::format] and are never written by the [Ubinizer]; all
/// this contributes is its original volume table record, at its original volume ID, so that the
/// new layout volume still describes it. UBI finds the data again when it scans the PEBs at
/// attach time, because every PEB carries its own `vol_id:lnum` in the VID header.
pub struct PreservedVolume {
    vol_id: u32,
    record: VolTableRecord,
}

impl PreservedVolume {
    /// Carry over the volume at `vol_id`, described by `record` in the old volume table.
    pub fn new(vol_id: u32, record: VolTableRecord) -> Self {
        Self { vol_id, record }
    }
}

impl Volume for PreservedVolume {
    fn into_data<'a>(self: Box<Self>, _: NonZeroU32, vol_id: u32) -> Box<dyn VolumeData + 'a>
    where
        Self: 'a,
    {
        assert_eq!(vol_id, self.vol_id, "preserved volume moved to another ID");
        Box::new(PreservedVolumeData(self.record))
    }

    fn get_vol_id(&self) -> Option<u32> {
        Some(self.vol_id)
    }

    fn requires_vol_id(&self) -> bool {
        true
    }

    fn estimate_blocks(&self, _: NonZeroU32) -> u32 {
        // Nothing is written: the PEBs are already on flash.
        0
    }
}

struct PreservedVolumeData(VolTableRecord);

impl VolumeData for PreservedVolumeData {
    fn next_block(&mut self, _: &mut Vec<u8>) -> anyhow::Result<Option<Vid>> {
        Ok(None)
    }

    fn into_vtbl_record(self: Box<Self>) -> VolTableRecord {
        self.0
    }
}

/// Given a sequence of volumes, and the EB size (i.e. PEB size minus EC/VID HDR pages), allows
/// iterating over the individual PEBs that must be written in order to image the flash.
pub struct Ubinizer<'a> {
    volumes: std::vec::IntoIter<(u32, Box<dyn Volume + 'a>)>,
    eb_size: NonZeroU32,
    layout: Option<Box<LayoutVolume>>,
    sqnum: u64,
    current_id: u32,
    current_data: Option<Box<dyn VolumeData + 'a>>,
}

impl<'a> Ubinizer<'a> {
    /// Estimate how many blocks the [Ubinizer] will yield, for a given [Volume] collection.
    pub fn estimate_blocks<'x, V>(volumes: V, eb_size: NonZeroU32) -> u32
    where
        V: IntoIterator<Item = &'x dyn Volume> + 'x,
    {
        volumes
            .into_iter()
            .map(|x| x.estimate_blocks(eb_size))
            .chain(std::iter::once(UBI_LAYOUT_VOLUME_EBS))
            .sum()
    }

    /// Create a new [Ubinizer], which will build an image with the given volumes that fits in
    /// flash with a given EB size.
    ///
    /// Volume IDs are settled here, before anything is written, in three passes: first the IDs
    /// that volumes require (preserved volumes, which cannot move), then the IDs that volumes
    /// prefer (first come, first served), then the lowest free ID for everything else. This way a
    /// volume without a preference can never land on an ID that a later volume depends on.
    pub fn new<V: IntoIterator<Item = Box<dyn Volume + 'a>>>(
        volumes: V,
        eb_size: NonZeroU32,
    ) -> anyhow::Result<Self> {
        let volumes: Vec<_> = volumes.into_iter().collect();
        let mut layout = Box::new(LayoutVolume::new(eb_size));
        let slots = layout.records.len() as u32;

        let mut ids: Vec<Option<u32>> = vec![None; volumes.len()];
        let mut taken = BTreeSet::new();

        for (slot, volume) in ids.iter_mut().zip(&volumes) {
            if !volume.requires_vol_id() {
                continue;
            }
            let id = volume
                .get_vol_id()
                .ok_or(anyhow::anyhow!("volume requires an ID but has none"))?;
            ensure!(id < slots, "required volume ID {id} is out of range");
            ensure!(taken.insert(id), "volume ID {id} is required twice");
            layout.preserved_ids.insert(id);
            *slot = Some(id);
        }

        for (slot, volume) in ids.iter_mut().zip(&volumes) {
            if slot.is_some() {
                continue;
            }
            if let Some(id) = volume.get_vol_id() {
                if id < slots && taken.insert(id) {
                    *slot = Some(id);
                }
            }
        }

        for slot in ids.iter_mut() {
            if slot.is_some() {
                continue;
            }
            let id = (0..slots)
                .find(|id| !taken.contains(id))
                .ok_or(anyhow::anyhow!("volume table is full"))?;
            taken.insert(id);
            *slot = Some(id);
        }

        let volumes: Vec<_> = ids.into_iter().map(Option::unwrap).zip(volumes).collect();

        Ok(Self {
            volumes: volumes.into_iter(),
            eb_size,
            layout: Some(layout),
            sqnum: 0,
            current_id: 0,
            current_data: None,
        })
    }

    /// Number the written VID headers starting just above `sqnum`.
    ///
    /// UBI resolves two PEBs claiming the same `vol_id:lnum` by the higher sqnum, and starts its
    /// own counter above the highest one it finds. When some PEBs are kept from the previous
    /// image, the new headers should be numbered above them so that the image reads as written
    /// after them.
    pub fn sqnum_base(mut self, sqnum: u64) -> Self {
        self.sqnum = sqnum;
        self
    }

    /// Pull the next volume from `self.volumes`, turn it into [VolumeData], and put it in
    /// `self.current_data`.
    ///
    /// This is an internal function.
    fn next_volume(&mut self) {
        let (vol_id, volume) = match self.volumes.next() {
            Some(x) => x,
            None => match self.layout.take() {
                // `self.volumes` exhausted => take layout volume
                Some(layout) => (UBI_LAYOUT_VOLUME_ID, layout as Box<dyn Volume + 'a>),
                None => {
                    // End of all volumes
                    self.current_data = None;
                    return;
                }
            },
        };

        self.current_id = vol_id;
        let boxed_data = volume.into_data(self.eb_size, self.current_id);
        self.current_data = Some(boxed_data);
    }

    /// Yield the next block of the image, or None if this is the end of the image.
    ///
    /// The block's data will be *appended* to `data`, so space for headers may be pre-reserved by
    /// the caller if desired.
    pub fn next_block(&mut self, data: &mut Vec<u8>) -> anyhow::Result<Option<Vid>> {
        loop {
            if self.current_data.is_none() {
                self.next_volume();
            }

            let current_data = match self.current_data.as_deref_mut() {
                Some(x) => x,
                None => return Ok(None), // End of all blocks
            };

            // As long as `current_data` is providing blocks, just keep consuming it:
            if let Some(vid) = current_data.next_block(data)? {
                assert_eq!(vid.vol_id, self.current_id);
                self.sqnum += 1;
                return Ok(Some(vid.sqnum(self.sqnum)));
            }

            // Upon getting here, the `current_data` is empty; need to cycle it; drop the reference
            // and grab the box.
            let current_data = self.current_data.take().unwrap();

            // If we still have the layout volume, tell it about the vtbl record.
            if let Some(ref mut layout) = self.layout {
                let record = current_data.into_vtbl_record();

                // UBI refuses to attach a volume table with two volumes of the same name (see
                // `vtbl_check` in drivers/mtd/ubi/vtbl.c), so never write one.
                if !record.name.is_empty()
                    && layout
                        .records
                        .iter()
                        .flatten()
                        .any(|x| x.name == record.name)
                {
                    bail!("two volumes are named {:?}", record.name);
                }

                layout.store_record(self.current_id, record);
            }

            // Continue on to the next loop iteration, which will start on a fresh volume.
        }
    }
}

#[test]
fn test_basic_volume() -> anyhow::Result<()> {
    let mut image = std::io::repeat(0x11);
    let x = Box::new(
        BasicVolume::new(VolType::Static)
            .image(&mut image)
            .size(4096),
    );
    let mut d = x.into_data(1024.try_into().unwrap(), 7);

    let mut data = Vec::with_capacity(1024);
    for i in 0..4 {
        let vid = d.next_block(&mut data)?.unwrap();

        assert_eq!(
            vid,
            Vid {
                vol_type: VolType::Static,
                copy_flag: false,
                compat: 0,
                vol_id: 7,
                lnum: i,
                data_size: 1024,
                used_ebs: 4,
                data_pad: 0,
                data_crc: 0x8d746e93,
                sqnum: vid.sqnum, // not tested
            }
        );

        assert_eq!(data.len(), 1024 * (i + 1) as usize);
        assert!(data.iter().all(|&b| b == 0x11));
    }
    assert_eq!(d.next_block(&mut data)?, None);

    let record = d.into_vtbl_record();
    assert_eq!(
        record,
        VolTableRecord {
            reserved_pebs: 4,
            alignment: 1,
            vol_type: VolType::Static,
            ..Default::default()
        }
    );

    Ok(())
}
