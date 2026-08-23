mod policy;

use std::{
    collections::BTreeSet,
    env,
    error::Error,
    fmt,
    fs,
    net::{SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use policy::{FanPolicy, FanPolicyConfig, PolicyConfigError, parse_curve};

const DEFAULT_DESTINATION: &str = "255.255.255.255:42110";
const DEFAULT_INTERVAL: Duration = Duration::from_secs(1);
const MIN_TOKEN_LEN: usize = 16;
const MAX_TOKEN_LEN: usize = 32;
const SUPPORTED_HWMON_NAMES: &[&str] = &[
    "amdgpu",
    "coretemp",
    "i915",
    "k10temp",
    "nvidia",
    "nouveau",
    "zenpower",
];

#[derive(Debug)]
struct AgentConfig {
    token: String,
    destination: SocketAddr,
    interval: Duration,
    policy: FanPolicyConfig,
}

#[derive(Clone, Debug)]
struct Sensor {
    device: String,
    label: String,
    path: PathBuf,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("laptop-cooler-agent: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    let once = arguments.iter().any(|argument| argument == "--once");
    if arguments.iter().any(|argument| argument == "--help") {
        print_help();
        return Ok(());
    }
    let config_path = argument_value(&arguments, "--config")
        .map(PathBuf::from)
        .unwrap_or(default_config_path()?);
    let config = parse_config(&fs::read_to_string(&config_path)?)?;
    let mut sensors = discover_sensors(Path::new("/sys/class/hwmon"))?;
    if sensors.is_empty() {
        return Err("no supported CPU or GPU temperature sensors found in /sys/class/hwmon".into());
    }

    eprintln!("Using configuration: {}", config_path.display());
    for sensor in &sensors {
        eprintln!(
            "Temperature source: {} / {} ({})",
            sensor.device,
            sensor.label,
            sensor.path.display()
        );
    }
    eprintln!("Sending to {}", config.destination);

    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_broadcast(true)?;
    let mut sequence = initial_sequence();
    let mut policy = FanPolicy::new(config.policy.clone())?;
    let mut last_update = Instant::now()
        .checked_sub(config.interval)
        .unwrap_or_else(Instant::now);

    loop {
        let temperature_milli_c = match maximum_temperature(&sensors) {
            Ok(temperature) => temperature,
            Err(error) => {
                eprintln!("Temperature read failed: {error}; rediscovering sensors");
                sensors = discover_sensors(Path::new("/sys/class/hwmon"))?;
                if sensors.is_empty() {
                    thread::sleep(config.interval);
                    continue;
                }
                maximum_temperature(&sensors)?
            }
        };
        let now = Instant::now();
        let requested_duty_percent = policy.update(temperature_milli_c, now.duration_since(last_update));
        last_update = now;
        let packet = format!(
            "LC2 {} {} {} {}\n",
            config.token, sequence, temperature_milli_c, requested_duty_percent
        );
        socket.send_to(packet.as_bytes(), config.destination)?;
        if once {
            println!(
                "sent sequence {sequence}, temperature {:.3} C, requested duty {requested_duty_percent}% to {}",
                f64::from(temperature_milli_c) / 1000.0,
                config.destination
            );
            return Ok(());
        }

        sequence = sequence.wrapping_add(1);
        thread::sleep(config.interval);
    }
}

fn parse_config(contents: &str) -> Result<AgentConfig, ConfigError> {
    let mut token = None;
    let mut destination = None;
    let mut interval_millis = None;
    let mut policy = FanPolicyConfig::default();
    let mut seen_keys = BTreeSet::new();

    for (line_index, raw_line) in contents.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or(ConfigError::InvalidLine(line_index + 1))?;
        let key = key.trim();
        let value = value.trim();
        if !seen_keys.insert(key.to_owned()) {
            return Err(ConfigError::DuplicateKey(key.to_owned()));
        }
        match key {
            "token" => token = Some(value.to_owned()),
            "destination" => {
                destination = Some(
                    value
                        .parse()
                        .map_err(|_| ConfigError::InvalidDestination)?,
                )
            }
            "interval_ms" => {
                interval_millis = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| ConfigError::InvalidInterval)?,
                )
            }
            "curve" => {
                policy.curve = parse_curve(value).map_err(ConfigError::InvalidPolicy)?;
            }
            "smoothing_seconds" => {
                policy.smoothing_seconds = value
                    .parse::<u32>()
                    .map_err(|_| ConfigError::InvalidPolicy(PolicyConfigError::InvalidSmoothing))?;
            }
            "duty_step_percent" => {
                policy.duty_step_percent = value
                    .parse::<u8>()
                    .map_err(|_| ConfigError::InvalidPolicy(PolicyConfigError::InvalidDutyStep))?;
            }
            "duty_hysteresis_percent" => {
                policy.duty_hysteresis_percent = value
                    .parse::<u8>()
                    .map_err(|_| ConfigError::InvalidPolicy(PolicyConfigError::InvalidHysteresis))?;
            }
            "ramp_up_percent_per_second" => {
                policy.ramp_up_percent_per_second = value
                    .parse::<u8>()
                    .map_err(|_| ConfigError::InvalidPolicy(PolicyConfigError::InvalidRamp))?;
            }
            "ramp_down_percent_per_second" => {
                policy.ramp_down_percent_per_second = value
                    .parse::<u8>()
                    .map_err(|_| ConfigError::InvalidPolicy(PolicyConfigError::InvalidRamp))?;
            }
            "emergency_temperature_c" => {
                let temperature_c = value.parse::<f64>().map_err(|_| {
                    ConfigError::InvalidPolicy(PolicyConfigError::InvalidEmergencyTemperature)
                })?;
                let temperature_milli_c = temperature_c * 1_000.0;
                if !temperature_milli_c.is_finite()
                    || temperature_milli_c < f64::from(i32::MIN)
                    || temperature_milli_c > f64::from(i32::MAX)
                {
                    return Err(ConfigError::InvalidPolicy(
                        PolicyConfigError::InvalidEmergencyTemperature,
                    ));
                }
                policy.emergency_temperature_milli_c = temperature_milli_c.round() as i32;
            }
            _ => return Err(ConfigError::UnknownKey(key.to_owned())),
        }
    }

    let token = token.ok_or(ConfigError::MissingToken)?;
    if token.len() < MIN_TOKEN_LEN
        || token.len() > MAX_TOKEN_LEN
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ConfigError::InvalidToken);
    }
    let interval_millis = interval_millis.unwrap_or(DEFAULT_INTERVAL.as_millis() as u64);
    if !(250..=60_000).contains(&interval_millis) {
        return Err(ConfigError::InvalidInterval);
    }
    policy.validate().map_err(ConfigError::InvalidPolicy)?;

    Ok(AgentConfig {
        token,
        destination: destination.unwrap_or_else(|| {
            DEFAULT_DESTINATION
                .parse()
                .expect("default destination must be valid")
        }),
        interval: Duration::from_millis(interval_millis),
        policy,
    })
}

fn discover_sensors(root: &Path) -> Result<Vec<Sensor>, Box<dyn Error>> {
    let mut sensors = Vec::new();
    for entry in fs::read_dir(root)? {
        let directory = entry?.path();
        let name = match fs::read_to_string(directory.join("name")) {
            Ok(name) => name.trim().to_owned(),
            Err(_) => continue,
        };
        if !SUPPORTED_HWMON_NAMES.contains(&name.as_str()) {
            continue;
        }

        for file in fs::read_dir(&directory)? {
            let path = file?.path();
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !file_name.starts_with("temp") || !file_name.ends_with("_input") {
                continue;
            }
            let prefix = file_name.trim_end_matches("_input");
            let label = fs::read_to_string(directory.join(format!("{prefix}_label")))
                .map(|label| label.trim().to_owned())
                .unwrap_or_else(|_| prefix.to_owned());
            sensors.push(Sensor {
                device: name.clone(),
                label,
                path,
            });
        }
    }
    sensors.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(sensors)
}

fn maximum_temperature(sensors: &[Sensor]) -> Result<i32, Box<dyn Error>> {
    let mut maximum = None;
    for sensor in sensors {
        let value = fs::read_to_string(&sensor.path)?.trim().parse::<i32>()?;
        if !(-20_000..=150_000).contains(&value) {
            return Err(format!(
                "{} returned implausible temperature {value}",
                sensor.path.display()
            )
            .into());
        }
        maximum = Some(maximum.map_or(value, |current: i32| current.max(value)));
    }
    maximum.ok_or_else(|| "no readable temperature sensors".into())
}

fn default_config_path() -> Result<PathBuf, Box<dyn Error>> {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(path).join("laptop-cooler/config"));
    }
    let home = env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/laptop-cooler/config"))
}

fn argument_value<'a>(arguments: &'a [String], name: &str) -> Option<&'a str> {
    arguments
        .windows(2)
        .find(|window| window[0] == name)
        .map(|window| window[1].as_str())
}

fn initial_sequence() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos() ^ duration.as_secs() as u32)
        .unwrap_or(0)
}

fn print_help() {
    println!(
        "Usage: laptop-cooler-agent [--config PATH] [--once]\n\
         \n\
         Reads CPU/GPU temperatures from Linux hwmon and broadcasts them to the cooler.\n\
         Default config: $XDG_CONFIG_HOME/laptop-cooler/config or ~/.config/laptop-cooler/config"
    );
}

#[derive(Debug, Eq, PartialEq)]
enum ConfigError {
    InvalidLine(usize),
    MissingToken,
    InvalidToken,
    InvalidDestination,
    InvalidInterval,
    InvalidPolicy(PolicyConfigError),
    DuplicateKey(String),
    UnknownKey(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLine(line) => write!(formatter, "invalid config line {line}"),
            Self::MissingToken => write!(formatter, "config is missing token"),
            Self::InvalidToken => write!(
                formatter,
                "token must contain 16-32 letters, numbers, underscores, or hyphens"
            ),
            Self::InvalidDestination => write!(formatter, "invalid destination socket address"),
            Self::InvalidInterval => write!(formatter, "interval_ms must be between 250 and 60000"),
            Self::InvalidPolicy(error) => write!(formatter, "invalid fan policy: {error}"),
            Self::DuplicateKey(key) => write!(formatter, "duplicate config key {key}"),
            Self::UnknownKey(key) => write!(formatter, "unknown config key {key}"),
        }
    }
}

impl Error for ConfigError {}

#[cfg(test)]
#[path = "../../src/config.rs"]
mod config;
#[cfg(test)]
#[path = "../../src/control.rs"]
mod firmware_control;
#[cfg(test)]
#[path = "../../src/provisioning.rs"]
mod firmware_provisioning;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config() {
        let config = parse_config("token=abcdefghijklmnop\n").unwrap();
        assert_eq!(config.token, "abcdefghijklmnop");
        assert_eq!(config.destination.to_string(), DEFAULT_DESTINATION);
        assert_eq!(config.interval, DEFAULT_INTERVAL);
    }

    #[test]
    fn rejects_short_token() {
        assert_eq!(
            parse_config("token=too-short\n").unwrap_err(),
            ConfigError::InvalidToken
        );
    }

    #[test]
    fn parses_overrides() {
        let config = parse_config(
            "token=abcdefghijklmnop\ndestination=127.0.0.1:42110\ninterval_ms=250\n",
        )
        .unwrap();
        assert_eq!(config.destination.to_string(), "127.0.0.1:42110");
        assert_eq!(config.interval, Duration::from_millis(250));
    }

    #[test]
    fn parses_configurable_fan_policy() {
        let config = parse_config(
            "token=abcdefghijklmnop\n\
             curve=40:20,70:70,85:100\n\
             smoothing_seconds=8\n\
             duty_step_percent=10\n\
             duty_hysteresis_percent=4\n\
             ramp_up_percent_per_second=20\n\
             ramp_down_percent_per_second=3\n\
             emergency_temperature_c=85\n",
        )
        .unwrap();
        assert_eq!(config.policy.curve.len(), 3);
        assert_eq!(config.policy.smoothing_seconds, 8);
        assert_eq!(config.policy.duty_step_percent, 10);
        assert_eq!(config.policy.duty_hysteresis_percent, 4);
        assert_eq!(config.policy.ramp_up_percent_per_second, 20);
        assert_eq!(config.policy.ramp_down_percent_per_second, 3);
        assert_eq!(config.policy.emergency_temperature_milli_c, 85_000);
    }

    #[test]
    fn firmware_config_record_round_trips_and_detects_corruption() {
        use crate::config::{ConfigError, DeviceConfig, decode, encode};

        let config = DeviceConfig::new("home", "password", "abcdefghijklmnop").unwrap();
        let record = encode(&config, 42);
        assert_eq!(decode(&record), Ok((config.clone(), 42)));

        let mut corrupt = record;
        corrupt[30] ^= 1;
        assert_eq!(decode(&corrupt), Err(ConfigError::Corrupt));

        let mut uncommitted = record;
        uncommitted[252..].fill(0xff);
        assert_eq!(decode(&uncommitted), Err(ConfigError::Uncommitted));
    }

    #[test]
    fn firmware_setup_parser_waits_for_and_validates_complete_post() {
        use crate::firmware_provisioning::{Request, parse_request};

        let body = "ssid=Home+WiFi&password=password&token=abcdefghijklmnop";
        let request = format!(
            "POST /save HTTP/1.1\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let split = request.len() - 5;
        assert_eq!(parse_request(&request.as_bytes()[..split], 1024), Ok(None));
        match parse_request(request.as_bytes(), 1024).unwrap() {
            Some(Request::Save(config)) => {
                assert_eq!(config.ssid.as_str(), "Home WiFi");
                assert_eq!(config.password.as_str(), "password");
                assert_eq!(config.token.as_str(), "abcdefghijklmnop");
            }
            result => panic!("unexpected parser result: {result:?}"),
        }
    }

    #[test]
    fn firmware_fan_control_guards_host_duty_commands() {
        use crate::firmware_control::{HostState, SampleError};

        let token = "abcdefghijklmnop";
        let mut state = HostState::new();
        assert_eq!(crate::firmware_control::CONTROL_PORT, 42_110);
        assert_eq!(state.duty_percent(0), 0);

        state
            .accept(token, b"LC2 abcdefghijklmnop 10 40000 35\n", 1_000)
            .unwrap();
        assert_eq!(state.duty_percent(1_000), 35);
        assert_eq!(state.duty_percent(6_001), 35);
        assert_eq!(state.duty_percent(61_001), 0);

        assert_eq!(
            state.accept(token, b"LC2 wrongwrongwrongw 11 40000 35\n", 7_000),
            Err(SampleError::InvalidToken)
        );
        assert_eq!(state.duty_percent(7_000), 35);

        assert_eq!(
            state.accept(token, b"LC2 abcdefghijklmnop 1 40000 19\n", 7_001),
            Err(SampleError::InvalidDuty)
        );
        state
            .accept(token, b"LC2 abcdefghijklmnop 1 90000 20\n", 7_001)
            .unwrap();
        assert_eq!(state.duty_percent(7_001), 100);
        assert_eq!(
            state.accept(token, b"LC2 abcdefghijklmnop 1 40000 50\n", 7_002),
            Err(SampleError::OutOfOrder)
        );
        assert_eq!(state.duty_percent(7_002), 100);
        assert_eq!(state.duty_percent(67_002), 0);
    }
}
