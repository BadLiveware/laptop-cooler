use core::str;

use heapless::String;

pub const MAX_SSID_LEN: usize = 32;
pub const MAX_PASSWORD_LEN: usize = 64;
pub const MAX_TOKEN_LEN: usize = 32;
pub const MIN_TOKEN_LEN: usize = 16;
pub const RECORD_LEN: usize = 256;
pub const COMMIT_OFFSET: usize = RECORD_LEN - 4;

const MAGIC: &[u8; 4] = b"LCWF";
const COMMIT_MARKER: &[u8; 4] = b"LCOK";
const VERSION: u16 = 1;
const CRC_OFFSET: usize = 16;
const HEADER_LEN: usize = 20;
const SSID_OFFSET: usize = HEADER_LEN;
const PASSWORD_OFFSET: usize = SSID_OFFSET + MAX_SSID_LEN;
const TOKEN_OFFSET: usize = PASSWORD_OFFSET + MAX_PASSWORD_LEN;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceConfig {
    pub ssid: String<MAX_SSID_LEN>,
    pub password: String<MAX_PASSWORD_LEN>,
    pub token: String<MAX_TOKEN_LEN>,
}

impl DeviceConfig {
    pub fn new(ssid: &str, password: &str, token: &str) -> Result<Self, ConfigError> {
        if ssid.is_empty() || ssid.len() > MAX_SSID_LEN {
            return Err(ConfigError::InvalidSsid);
        }
        let password_is_valid = password.is_empty()
            || (8..=63).contains(&password.len())
            || (password.len() == 64 && password.bytes().all(|byte| byte.is_ascii_hexdigit()));
        if !password_is_valid {
            return Err(ConfigError::InvalidPassword);
        }
        if token.len() < MIN_TOKEN_LEN
            || token.len() > MAX_TOKEN_LEN
            || !token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(ConfigError::InvalidToken);
        }

        let mut ssid_value = String::new();
        ssid_value
            .push_str(ssid)
            .map_err(|_| ConfigError::InvalidSsid)?;

        let mut password_value = String::new();
        password_value
            .push_str(password)
            .map_err(|_| ConfigError::InvalidPassword)?;

        let mut token_value = String::new();
        token_value
            .push_str(token)
            .map_err(|_| ConfigError::InvalidToken)?;

        Ok(Self {
            ssid: ssid_value,
            password: password_value,
            token: token_value,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigError {
    Missing,
    Uncommitted,
    UnsupportedVersion,
    Corrupt,
    InvalidUtf8,
    InvalidSsid,
    InvalidPassword,
    InvalidToken,
}

pub fn encode(config: &DeviceConfig, generation: u32) -> [u8; RECORD_LEN] {
    let mut record = [0xff; RECORD_LEN];
    record[..MAGIC.len()].copy_from_slice(MAGIC);
    record[4..6].copy_from_slice(&VERSION.to_le_bytes());
    record[6] = config.ssid.len() as u8;
    record[7] = config.password.len() as u8;
    record[8] = config.token.len() as u8;
    record[9..12].fill(0);
    record[12..16].copy_from_slice(&generation.to_le_bytes());
    record[CRC_OFFSET..HEADER_LEN].fill(0);

    record[SSID_OFFSET..SSID_OFFSET + config.ssid.len()].copy_from_slice(config.ssid.as_bytes());
    record[PASSWORD_OFFSET..PASSWORD_OFFSET + config.password.len()]
        .copy_from_slice(config.password.as_bytes());
    record[TOKEN_OFFSET..TOKEN_OFFSET + config.token.len()].copy_from_slice(config.token.as_bytes());
    record[COMMIT_OFFSET..].copy_from_slice(COMMIT_MARKER);

    let checksum = record_crc(&record);
    record[CRC_OFFSET..HEADER_LEN].copy_from_slice(&checksum.to_le_bytes());
    record
}

pub fn decode(record: &[u8; RECORD_LEN]) -> Result<(DeviceConfig, u32), ConfigError> {
    if record.iter().all(|byte| *byte == 0xff) {
        return Err(ConfigError::Missing);
    }
    if &record[COMMIT_OFFSET..] != COMMIT_MARKER {
        return Err(ConfigError::Uncommitted);
    }
    if &record[..MAGIC.len()] != MAGIC {
        return Err(ConfigError::Corrupt);
    }
    if u16::from_le_bytes(
        record[4..6]
            .try_into()
            .map_err(|_| ConfigError::Corrupt)?,
    ) != VERSION
    {
        return Err(ConfigError::UnsupportedVersion);
    }

    let expected_checksum = u32::from_le_bytes(
        record[CRC_OFFSET..HEADER_LEN]
            .try_into()
            .map_err(|_| ConfigError::Corrupt)?,
    );
    if record_crc(record) != expected_checksum {
        return Err(ConfigError::Corrupt);
    }

    let ssid_len = usize::from(record[6]);
    let password_len = usize::from(record[7]);
    let token_len = usize::from(record[8]);
    if ssid_len == 0 || ssid_len > MAX_SSID_LEN {
        return Err(ConfigError::InvalidSsid);
    }
    if password_len > MAX_PASSWORD_LEN {
        return Err(ConfigError::InvalidPassword);
    }
    if !(MIN_TOKEN_LEN..=MAX_TOKEN_LEN).contains(&token_len) {
        return Err(ConfigError::InvalidToken);
    }

    let ssid = str::from_utf8(&record[SSID_OFFSET..SSID_OFFSET + ssid_len])
        .map_err(|_| ConfigError::InvalidUtf8)?;
    let password = str::from_utf8(&record[PASSWORD_OFFSET..PASSWORD_OFFSET + password_len])
        .map_err(|_| ConfigError::InvalidUtf8)?;
    let token = str::from_utf8(&record[TOKEN_OFFSET..TOKEN_OFFSET + token_len])
        .map_err(|_| ConfigError::InvalidUtf8)?;
    let generation = u32::from_le_bytes(
        record[12..16]
            .try_into()
            .map_err(|_| ConfigError::Corrupt)?,
    );

    Ok((DeviceConfig::new(ssid, password, token)?, generation))
}

fn record_crc(record: &[u8; RECORD_LEN]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for (index, byte) in record[..COMMIT_OFFSET].iter().enumerate() {
        let value = if (CRC_OFFSET..HEADER_LEN).contains(&index) {
            0
        } else {
            *byte
        };
        crc ^= u32::from(value);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}
