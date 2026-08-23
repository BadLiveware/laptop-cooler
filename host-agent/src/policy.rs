use std::{error::Error, fmt, time::Duration};

pub const MIN_DUTY_PERCENT: u8 = 20;
pub const MAX_DUTY_PERCENT: u8 = 100;
pub const MAX_CONFIGURABLE_EMERGENCY_C: u32 = 90;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CurvePoint {
    pub temperature_milli_c: i32,
    pub duty_percent: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FanPolicyConfig {
    pub curve: Vec<CurvePoint>,
    pub smoothing_seconds: u32,
    pub duty_step_percent: u8,
    pub duty_hysteresis_percent: u8,
    pub ramp_up_percent_per_second: u8,
    pub ramp_down_percent_per_second: u8,
    pub emergency_temperature_milli_c: i32,
}

impl Default for FanPolicyConfig {
    fn default() -> Self {
        Self {
            curve: parse_curve("45:25,55:40,65:60,75:80,80:100")
                .expect("default curve must be valid"),
            smoothing_seconds: 5,
            duty_step_percent: 5,
            duty_hysteresis_percent: 5,
            ramp_up_percent_per_second: 10,
            ramp_down_percent_per_second: 2,
            emergency_temperature_milli_c: 80_000,
        }
    }
}

impl FanPolicyConfig {
    pub fn validate(&self) -> Result<(), PolicyConfigError> {
        if self.curve.len() < 2
            || self
                .curve
                .windows(2)
                .any(|points| {
                    points[0].temperature_milli_c >= points[1].temperature_milli_c
                        || points[0].duty_percent > points[1].duty_percent
                })
            || self.curve.iter().any(|point| {
                !(0..=120_000).contains(&point.temperature_milli_c)
                    || !(MIN_DUTY_PERCENT..=MAX_DUTY_PERCENT).contains(&point.duty_percent)
            })
            || self.curve.last().is_none_or(|point| {
                point.duty_percent != MAX_DUTY_PERCENT
                    || point.temperature_milli_c > self.emergency_temperature_milli_c
            })
        {
            return Err(PolicyConfigError::InvalidCurve);
        }
        if self.smoothing_seconds > 60 {
            return Err(PolicyConfigError::InvalidSmoothing);
        }
        if !(1..=20).contains(&self.duty_step_percent) {
            return Err(PolicyConfigError::InvalidDutyStep);
        }
        if self.duty_hysteresis_percent > 20 {
            return Err(PolicyConfigError::InvalidHysteresis);
        }
        if !(1..=100).contains(&self.ramp_up_percent_per_second)
            || !(1..=100).contains(&self.ramp_down_percent_per_second)
        {
            return Err(PolicyConfigError::InvalidRamp);
        }
        if !(50_000..=i32::try_from(MAX_CONFIGURABLE_EMERGENCY_C).unwrap() * 1_000)
            .contains(&self.emergency_temperature_milli_c)
        {
            return Err(PolicyConfigError::InvalidEmergencyTemperature);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct FanPolicy {
    config: FanPolicyConfig,
    smoothed_temperature_milli_c: Option<f64>,
    target_duty_percent: u8,
    commanded_duty_percent: u8,
    slew_budget_percent: f64,
}

impl FanPolicy {
    pub fn new(config: FanPolicyConfig) -> Result<Self, PolicyConfigError> {
        config.validate()?;
        Ok(Self {
            config,
            smoothed_temperature_milli_c: None,
            target_duty_percent: MAX_DUTY_PERCENT,
            commanded_duty_percent: MAX_DUTY_PERCENT,
            slew_budget_percent: 0.0,
        })
    }

    pub fn update(&mut self, temperature_milli_c: i32, elapsed: Duration) -> u8 {
        if temperature_milli_c >= self.config.emergency_temperature_milli_c {
            self.smoothed_temperature_milli_c = Some(f64::from(temperature_milli_c));
            self.target_duty_percent = MAX_DUTY_PERCENT;
            self.commanded_duty_percent = MAX_DUTY_PERCENT;
            self.slew_budget_percent = 0.0;
            return MAX_DUTY_PERCENT;
        }

        let raw_temperature = f64::from(temperature_milli_c);
        let elapsed_seconds = elapsed.as_secs_f64();
        let smoothed = match self.smoothed_temperature_milli_c {
            None => raw_temperature,
            Some(_) if self.config.smoothing_seconds == 0 => raw_temperature,
            Some(previous) => {
                let smoothing_seconds = f64::from(self.config.smoothing_seconds);
                let weight = elapsed_seconds / (smoothing_seconds + elapsed_seconds);
                previous + weight * (raw_temperature - previous)
            }
        };
        self.smoothed_temperature_milli_c = Some(smoothed);

        let desired = interpolate_curve(&self.config.curve, smoothed);
        let target = f64::from(self.target_duty_percent);
        let hysteresis = f64::from(self.config.duty_hysteresis_percent);
        if desired >= target + hysteresis || desired <= target - hysteresis {
            let new_target = quantize_duty(desired, self.config.duty_step_percent);
            if new_target != self.target_duty_percent {
                self.target_duty_percent = new_target;
                self.slew_budget_percent = 0.0;
            }
        }

        if self.commanded_duty_percent == self.target_duty_percent {
            self.slew_budget_percent = 0.0;
            return self.commanded_duty_percent;
        }

        let rate = if self.target_duty_percent > self.commanded_duty_percent {
            self.config.ramp_up_percent_per_second
        } else {
            self.config.ramp_down_percent_per_second
        };
        self.slew_budget_percent += f64::from(rate) * elapsed_seconds;

        let step = self.config.duty_step_percent;
        let available_steps = (self.slew_budget_percent / f64::from(step)).floor() as u8;
        if available_steps == 0 {
            return self.commanded_duty_percent;
        }
        let available_change = available_steps.saturating_mul(step);
        let distance = self
            .target_duty_percent
            .abs_diff(self.commanded_duty_percent);
        let change = available_change.min(distance);
        if self.target_duty_percent > self.commanded_duty_percent {
            self.commanded_duty_percent = self.commanded_duty_percent.saturating_add(change);
        } else {
            self.commanded_duty_percent = self.commanded_duty_percent.saturating_sub(change);
        }
        self.slew_budget_percent -= f64::from(change);
        self.commanded_duty_percent
    }
}

pub fn parse_curve(value: &str) -> Result<Vec<CurvePoint>, PolicyConfigError> {
    let mut points = Vec::new();
    for field in value.split(',') {
        let (temperature, duty) = field
            .trim()
            .split_once(':')
            .ok_or(PolicyConfigError::InvalidCurve)?;
        let temperature_c = temperature
            .trim()
            .parse::<f64>()
            .map_err(|_| PolicyConfigError::InvalidCurve)?;
        let duty_percent = duty
            .trim()
            .parse::<u8>()
            .map_err(|_| PolicyConfigError::InvalidCurve)?;
        if !temperature_c.is_finite() {
            return Err(PolicyConfigError::InvalidCurve);
        }
        let temperature_milli_c = (temperature_c * 1_000.0).round();
        if temperature_milli_c < f64::from(i32::MIN)
            || temperature_milli_c > f64::from(i32::MAX)
        {
            return Err(PolicyConfigError::InvalidCurve);
        }
        points.push(CurvePoint {
            temperature_milli_c: temperature_milli_c as i32,
            duty_percent,
        });
    }
    Ok(points)
}

fn interpolate_curve(curve: &[CurvePoint], temperature_milli_c: f64) -> f64 {
    if temperature_milli_c <= f64::from(curve[0].temperature_milli_c) {
        return f64::from(curve[0].duty_percent);
    }
    for points in curve.windows(2) {
        let lower = points[0];
        let upper = points[1];
        if temperature_milli_c <= f64::from(upper.temperature_milli_c) {
            let temperature_span = f64::from(upper.temperature_milli_c - lower.temperature_milli_c);
            let position =
                (temperature_milli_c - f64::from(lower.temperature_milli_c)) / temperature_span;
            return f64::from(lower.duty_percent)
                + position * f64::from(upper.duty_percent - lower.duty_percent);
        }
    }
    f64::from(curve.last().expect("validated curve is non-empty").duty_percent)
}

fn quantize_duty(duty_percent: f64, step: u8) -> u8 {
    let quantized = (duty_percent / f64::from(step)).round() * f64::from(step);
    quantized
        .clamp(f64::from(MIN_DUTY_PERCENT), f64::from(MAX_DUTY_PERCENT)) as u8
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyConfigError {
    InvalidCurve,
    InvalidSmoothing,
    InvalidDutyStep,
    InvalidHysteresis,
    InvalidRamp,
    InvalidEmergencyTemperature,
}

impl fmt::Display for PolicyConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCurve => write!(formatter, "curve must contain increasing temperature:duty points and end at 100% no later than the emergency temperature"),
            Self::InvalidSmoothing => write!(formatter, "smoothing_seconds must be between 0 and 60"),
            Self::InvalidDutyStep => write!(formatter, "duty_step_percent must be between 1 and 20"),
            Self::InvalidHysteresis => write!(formatter, "duty_hysteresis_percent must be between 0 and 20"),
            Self::InvalidRamp => write!(formatter, "ramp rates must be between 1 and 100 percent per second"),
            Self::InvalidEmergencyTemperature => write!(formatter, "emergency_temperature_c must be between 50 and 90"),
        }
    }
}

impl Error for PolicyConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_small_fluctuations_after_reaching_a_quantized_target() {
        let mut policy = FanPolicy::new(FanPolicyConfig {
            smoothing_seconds: 0,
            ramp_down_percent_per_second: 100,
            ..FanPolicyConfig::default()
        })
        .unwrap();

        assert_eq!(policy.update(60_000, Duration::from_secs(1)), 50);
        assert_eq!(policy.update(60_500, Duration::from_secs(1)), 50);
        assert_eq!(policy.update(59_500, Duration::from_secs(1)), 50);
    }

    #[test]
    fn ramps_up_faster_than_down() {
        let mut policy = FanPolicy::new(FanPolicyConfig {
            smoothing_seconds: 0,
            ramp_down_percent_per_second: 100,
            ..FanPolicyConfig::default()
        })
        .unwrap();
        assert_eq!(policy.update(50_000, Duration::from_secs(1)), 35);
        assert_eq!(policy.update(75_000, Duration::from_secs(1)), 45);
        assert_eq!(policy.update(75_000, Duration::from_secs(1)), 55);
    }

    #[test]
    fn emergency_temperature_bypasses_smoothing_and_slew() {
        let mut policy = FanPolicy::new(FanPolicyConfig::default()).unwrap();
        policy.update(50_000, Duration::from_secs(30));
        assert!(policy.update(50_000, Duration::from_secs(1)) < 100);
        assert_eq!(policy.update(80_000, Duration::from_millis(250)), 100);
    }

    #[test]
    fn rejects_non_monotonic_curve() {
        let mut config = FanPolicyConfig::default();
        config.curve = parse_curve("50:50,40:70,80:100").unwrap();
        assert_eq!(config.validate(), Err(PolicyConfigError::InvalidCurve));
    }
}
