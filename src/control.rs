use core::str;

pub const SAMPLE_TIMEOUT_MILLIS: u64 = 5_000;
pub const OFFLINE_COOLDOWN_MILLIS: u64 = 60_000;
pub const MIN_TEMPERATURE_MILLI_C: i32 = 0;
pub const MAX_TEMPERATURE_MILLI_C: i32 = 120_000;
pub const CONTROL_PORT: u16 = 42_110;
pub const MIN_DUTY_PERCENT: u8 = 20;
pub const MAX_DUTY_PERCENT: u8 = 100;
pub const HARD_EMERGENCY_TEMPERATURE_MILLI_C: i32 = 90_000;

const OFFLINE_FAN_DUTY_PERCENT: u8 = 0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlCommand {
    pub sequence: u32,
    pub temperature_milli_c: i32,
    pub requested_duty_percent: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleError {
    InvalidUtf8,
    InvalidShape,
    UnsupportedVersion,
    InvalidToken,
    InvalidSequence,
    InvalidTemperature,
    InvalidDuty,
    OutOfOrder,
}

#[derive(Clone, Copy, Debug)]
pub struct HostState {
    last_sequence: Option<u32>,
    last_update_millis: Option<u64>,
    temperature_milli_c: i32,
    requested_duty_percent: u8,
}

impl HostState {
    pub const fn new() -> Self {
        Self {
            last_sequence: None,
            last_update_millis: None,
            temperature_milli_c: 0,
            requested_duty_percent: OFFLINE_FAN_DUTY_PERCENT,
        }
    }

    pub fn accept(
        &mut self,
        expected_token: &str,
        packet: &[u8],
        now_millis: u64,
    ) -> Result<ControlCommand, SampleError> {
        let command = parse_command(expected_token, packet)?;
        let stale = self
            .last_update_millis
            .is_none_or(|updated| now_millis.saturating_sub(updated) > SAMPLE_TIMEOUT_MILLIS);
        if !stale
            && self
                .last_sequence
                .is_some_and(|current| !sequence_is_newer(command.sequence, current))
        {
            return Err(SampleError::OutOfOrder);
        }

        self.last_sequence = Some(command.sequence);
        self.last_update_millis = Some(now_millis);
        self.temperature_milli_c = command.temperature_milli_c;
        self.requested_duty_percent = command.requested_duty_percent;
        Ok(command)
    }

    pub fn duty_percent(&self, now_millis: u64) -> u8 {
        let Some(updated) = self.last_update_millis else {
            return OFFLINE_FAN_DUTY_PERCENT;
        };
        if now_millis.saturating_sub(updated) > OFFLINE_COOLDOWN_MILLIS {
            return OFFLINE_FAN_DUTY_PERCENT;
        }
        if self.temperature_milli_c >= HARD_EMERGENCY_TEMPERATURE_MILLI_C {
            MAX_DUTY_PERCENT
        } else {
            self.requested_duty_percent
        }
    }
}

pub fn parse_command(expected_token: &str, packet: &[u8]) -> Result<ControlCommand, SampleError> {
    let packet = str::from_utf8(packet).map_err(|_| SampleError::InvalidUtf8)?;
    let mut fields = packet.split_ascii_whitespace();
    if fields.next() != Some("LC2") {
        return Err(SampleError::UnsupportedVersion);
    }
    let token = fields.next().ok_or(SampleError::InvalidShape)?;
    if !constant_time_eq(token.as_bytes(), expected_token.as_bytes()) {
        return Err(SampleError::InvalidToken);
    }
    let sequence = fields
        .next()
        .ok_or(SampleError::InvalidShape)?
        .parse::<u32>()
        .map_err(|_| SampleError::InvalidSequence)?;
    let temperature_milli_c = fields
        .next()
        .ok_or(SampleError::InvalidShape)?
        .parse::<i32>()
        .map_err(|_| SampleError::InvalidTemperature)?;
    let requested_duty_percent = fields
        .next()
        .ok_or(SampleError::InvalidShape)?
        .parse::<u8>()
        .map_err(|_| SampleError::InvalidDuty)?;
    if fields.next().is_some() {
        return Err(SampleError::InvalidShape);
    }
    if !(MIN_TEMPERATURE_MILLI_C..=MAX_TEMPERATURE_MILLI_C).contains(&temperature_milli_c) {
        return Err(SampleError::InvalidTemperature);
    }
    if !(MIN_DUTY_PERCENT..=MAX_DUTY_PERCENT).contains(&requested_duty_percent) {
        return Err(SampleError::InvalidDuty);
    }

    Ok(ControlCommand {
        sequence,
        temperature_milli_c,
        requested_duty_percent,
    })
}

fn sequence_is_newer(candidate: u32, current: u32) -> bool {
    candidate != current && candidate.wrapping_sub(current) < 0x8000_0000
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}
