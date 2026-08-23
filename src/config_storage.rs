use core::fmt;

use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_storage::{FlashStorage, FlashStorageError};

use crate::config::{COMMIT_OFFSET, DeviceConfig, RECORD_LEN, decode, encode};

const PARTITION_OFFSET: u32 = 0x9000;
const PARTITION_END: u32 = 0xf000;
const SLOT_SIZE: u32 = FlashStorage::SECTOR_SIZE;
const SLOT_COUNT: usize = ((PARTITION_END - PARTITION_OFFSET) / SLOT_SIZE) as usize;

pub enum ConfigStorageError {
    Flash(FlashStorageError),
    VerificationFailed,
}

impl fmt::Debug for ConfigStorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Flash(error) => formatter.debug_tuple("Flash").field(error).finish(),
            Self::VerificationFailed => formatter.write_str("VerificationFailed"),
        }
    }
}

impl From<FlashStorageError> for ConfigStorageError {
    fn from(error: FlashStorageError) -> Self {
        Self::Flash(error)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StoredConfig {
    config: DeviceConfig,
    generation: u32,
    slot: usize,
}

pub fn load(flash: &mut FlashStorage<'_>) -> Result<Option<DeviceConfig>, ConfigStorageError> {
    Ok(load_latest(flash)?.map(|stored| stored.config))
}

pub fn save(
    flash: &mut FlashStorage<'_>,
    config: &DeviceConfig,
) -> Result<(), ConfigStorageError> {
    let latest = load_latest(flash)?;
    let (slot, generation) = match latest {
        Some(stored) => ((stored.slot + 1) % SLOT_COUNT, stored.generation.wrapping_add(1)),
        None => (0, 1),
    };

    let address = slot_address(slot);
    let record = encode(config, generation);
    flash.erase(address, address + SLOT_SIZE)?;
    flash.write(address, &record[..COMMIT_OFFSET])?;
    flash.write(
        address + COMMIT_OFFSET as u32,
        &record[COMMIT_OFFSET..RECORD_LEN],
    )?;

    let mut written = [0_u8; RECORD_LEN];
    flash.read(address, &mut written)?;
    match decode(&written) {
        Ok((stored_config, stored_generation))
            if stored_config == *config && stored_generation == generation =>
        {
            Ok(())
        }
        _ => Err(ConfigStorageError::VerificationFailed),
    }
}

fn load_latest(
    flash: &mut FlashStorage<'_>,
) -> Result<Option<StoredConfig>, ConfigStorageError> {
    let mut latest: Option<StoredConfig> = None;
    let mut record = [0_u8; RECORD_LEN];

    for slot in 0..SLOT_COUNT {
        flash.read(slot_address(slot), &mut record)?;
        let Ok((config, generation)) = decode(&record) else {
            continue;
        };

        let replace = latest
            .as_ref()
            .is_none_or(|current| generation_is_newer(generation, current.generation));
        if replace {
            latest = Some(StoredConfig {
                config,
                generation,
                slot,
            });
        }
    }

    Ok(latest)
}

fn slot_address(slot: usize) -> u32 {
    PARTITION_OFFSET + slot as u32 * SLOT_SIZE
}

fn generation_is_newer(candidate: u32, current: u32) -> bool {
    candidate != current && candidate.wrapping_sub(current) < 0x8000_0000
}
