#![no_std]
#![no_main]
#![deny(clippy::mem_forget)]
#![deny(clippy::large_stack_frames)]

extern crate alloc;

mod config;
mod config_storage;
mod control;
mod provisioning;

use alloc::string::String as AllocString;
use core::{cell::RefCell, net::Ipv4Addr, str::FromStr};

use embassy_executor::Spawner;
use embassy_net::{
    IpListenEndpoint, Ipv4Cidr, Runner, Stack, StackResources, StaticConfigV4,
    tcp::TcpSocket,
    udp::{PacketMetadata, UdpSocket},
};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel};
use embassy_time::{Duration as EmbassyDuration, Instant as EmbassyInstant, Timer, with_timeout};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::{DriveMode, Input, InputConfig, Pull},
    interrupt::software::SoftwareInterruptControl,
    ledc::{
        LSGlobalClkSource, Ledc, LowSpeed,
        channel::{self, ChannelIFace},
        timer::{self, TimerIFace},
    },
    ram,
    rng::Rng,
    time::{Duration, Instant, Rate},
    timer::timg::TimerGroup,
};
use esp_println::println;
use esp_radio::wifi::{
    AuthenticationMethod, Config as WifiConfig, ControllerConfig, Interface, WifiController,
    ap::AccessPointConfig,
    sta::StationConfig,
};
use esp_storage::FlashStorage;

esp_bootloader_esp_idf::esp_app_desc!();

const INITIAL_PWM_DUTY_PERCENT: u8 = 0;
const TACH_PULSES_PER_REVOLUTION: u64 = 2;
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const SETUP_IP: &str = "192.168.4.1";
const SETUP_SSID: &str = "LaptopCooler-Setup";
const SETUP_PASSWORD: &str = "cooler-setup";

static NEW_CONFIG: Channel<CriticalSectionRawMutex, config::DeviceConfig, 1> = Channel::new();
static CONFIG_SAVE_RESULT: Channel<CriticalSectionRawMutex, bool, 1> = Channel::new();
static HOST_STATE: critical_section::Mutex<RefCell<control::HostState>> =
    critical_section::Mutex::new(RefCell::new(control::HostState::new()));

macro_rules! mk_static {
    ($type:ty, $value:expr) => {{
        static CELL: static_cell::StaticCell<$type> = static_cell::StaticCell::new();
        CELL.uninit().write($value)
    }};
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    let mut config_flash = FlashStorage::new(peripherals.FLASH);
    let stored_config = match config_storage::load(&mut config_flash) {
        Ok(config) => config,
        Err(error) => {
            println!("Could not read stored configuration: {error:?}");
            None
        }
    };
    println!(
        "Stored Wi-Fi configuration: {}",
        if stored_config.is_some() { "present" } else { "missing" }
    );

    let timer_group = TimerGroup::new(peripherals.TIMG0);
    let software_interrupts = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timer_group.timer0, software_interrupts.software_interrupt0);

    let initial_wifi_config = stored_config
        .as_ref()
        .map(station_wifi_config)
        .unwrap_or_else(setup_wifi_config);
    let (mut wifi_controller, interfaces) = esp_radio::wifi::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(initial_wifi_config),
    )
    .expect("failed to initialize Wi-Fi");

    let rng = Rng::new();
    let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());
    let station_connected = if stored_config.is_some() {
        println!("Connecting to stored Wi-Fi network");
        match with_timeout(
            EmbassyDuration::from_secs(20),
            wifi_controller.connect_async(),
        )
        .await
        {
            Ok(Ok(info)) => {
                println!("Wi-Fi associated on channel {}", info.channel);
                true
            }
            Ok(Err(error)) => {
                println!("Stored Wi-Fi connection failed: {error:?}");
                false
            }
            Err(_) => {
                println!("Stored Wi-Fi connection timed out");
                false
            }
        }
    } else {
        false
    };

    if station_connected {
        let (station_stack, station_runner) = embassy_net::new(
            interfaces.station,
            embassy_net::Config::dhcpv4(Default::default()),
            mk_static!(StackResources<4>, StackResources::<4>::new()),
            seed,
        );
        spawner.spawn(network_runner(station_runner).expect("failed to allocate network task"));
        spawner.spawn(
            wifi_station_monitor(wifi_controller)
                .expect("failed to allocate Wi-Fi station monitor task"),
        );
        spawner.spawn(
            station_network_status(station_stack)
                .expect("failed to allocate station status task"),
        );
        let token = stored_config
            .as_ref()
            .expect("station mode requires stored configuration")
            .token
            .clone();
        spawner.spawn(
            temperature_receiver(station_stack, token)
                .expect("failed to allocate temperature receiver task"),
        );
    } else {
        if stored_config.is_some() {
            wifi_controller
                .set_config(&setup_wifi_config())
                .expect("failed to enter setup access-point mode");
        }

        let setup_ip = Ipv4Addr::from_str(SETUP_IP).expect("invalid setup IP address");
        let network_config = embassy_net::Config::ipv4_static(StaticConfigV4 {
            address: Ipv4Cidr::new(setup_ip, 24),
            gateway: Some(setup_ip),
            dns_servers: Default::default(),
        });
        let (setup_stack, setup_runner) = embassy_net::new(
            interfaces.access_point,
            network_config,
            mk_static!(StackResources<8>, StackResources::<8>::new()),
            seed,
        );

        spawner.spawn(
            wifi_access_point_events(wifi_controller)
                .expect("failed to allocate Wi-Fi controller task"),
        );
        spawner.spawn(network_runner(setup_runner).expect("failed to allocate network task"));
        spawner.spawn(dhcp_server(setup_stack).expect("failed to allocate DHCP task"));
        for _ in 0..3 {
            spawner.spawn(
                setup_http_server(setup_stack).expect("failed to allocate setup HTTP worker"),
            );
        }
    }

    let tach = Input::new(
        peripherals.GPIO27,
        InputConfig::default().with_pull(Pull::Up),
    );

    let mut ledc = Ledc::new(peripherals.LEDC);
    ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);

    let mut pwm_timer = ledc.timer::<LowSpeed>(timer::Number::Timer0);
    pwm_timer
        .configure(timer::config::Config {
            duty: timer::config::Duty::Duty10Bit,
            clock_source: timer::LSClockSource::APBClk,
            frequency: Rate::from_khz(25),
        })
        .expect("failed to configure 25 kHz PWM timer");

    let mut pwm = ledc.channel(channel::Number::Channel0, peripherals.GPIO32);
    pwm.configure(channel::config::Config {
        timer: &pwm_timer,
        duty_pct: INITIAL_PWM_DUTY_PERCENT,
        drive_mode: DriveMode::OpenDrain,
    })
    .expect("failed to configure fan PWM output");

    println!();
    println!("Laptop cooler controller");
    println!(
        "PWM: GPIO32, 25 kHz, {}% offline duty",
        INITIAL_PWM_DUTY_PERCENT
    );
    println!("Tach: GPIO27, internal pull-up, 2 pulses/revolution");
    println!("Setup Wi-Fi: {SETUP_SSID}");
    println!("Setup page: http://{SETUP_IP}/");

    let mut sample_started_at = Instant::now();
    let mut pulse_count = 0_u64;
    let mut tach_was_low = tach.is_low();
    let mut current_duty = INITIAL_PWM_DUTY_PERCENT;

    loop {
        if let Ok(new_config) = NEW_CONFIG.try_receive() {
            let saved = match config_storage::save(&mut config_flash, &new_config) {
                Ok(()) => {
                    println!("Wi-Fi configuration saved and verified");
                    true
                }
                Err(error) => {
                    println!("Could not save Wi-Fi configuration: {error:?}");
                    false
                }
            };
            CONFIG_SAVE_RESULT.send(saved).await;
        }

        let target_duty = critical_section::with(|critical_section| {
            HOST_STATE
                .borrow(critical_section)
                .borrow()
                .duty_percent(EmbassyInstant::now().as_millis())
        });
        if target_duty != current_duty {
            pwm.set_duty(target_duty)
                .expect("failed to update fan PWM duty");
            current_duty = target_duty;
            println!("Fan duty: {current_duty}%");
        }

        let tach_is_low = tach.is_low();
        if tach_is_low && !tach_was_low {
            pulse_count += 1;
        }
        tach_was_low = tach_is_low;

        let elapsed = sample_started_at.elapsed();
        if elapsed >= SAMPLE_INTERVAL {
            let elapsed_ms = elapsed.as_millis();
            let rpm = pulse_count * 60_000 / (TACH_PULSES_PER_REVOLUTION * elapsed_ms.max(1));

            println!("tach pulses: {}, speed: {} RPM", pulse_count, rpm);

            pulse_count = 0;
            sample_started_at = Instant::now();
        }

        Timer::after(EmbassyDuration::from_millis(1)).await;
    }
}

fn setup_wifi_config() -> WifiConfig {
    WifiConfig::AccessPoint(
        AccessPointConfig::default()
            .with_ssid(SETUP_SSID)
            .with_password(AllocString::from(SETUP_PASSWORD))
            .with_auth_method(AuthenticationMethod::Wpa2Personal),
    )
}

fn station_wifi_config(config: &config::DeviceConfig) -> WifiConfig {
    let authentication = if config.password.is_empty() {
        AuthenticationMethod::None
    } else {
        AuthenticationMethod::Wpa2Personal
    };
    WifiConfig::Station(
        StationConfig::default()
            .with_ssid(config.ssid.as_str())
            .with_password(AllocString::from(config.password.as_str()))
            .with_auth_method(authentication),
    )
}

#[embassy_executor::task]
async fn network_runner(mut runner: Runner<'static, Interface<'static>>) {
    runner.run().await
}

#[embassy_executor::task]
async fn wifi_station_monitor(mut controller: WifiController<'static>) {
    loop {
        if controller.is_connected() {
            if let Ok(info) = controller.wait_for_disconnect_async().await {
                println!("Wi-Fi disconnected: {:?}", info.reason);
            }
        }

        let mut reconnected = false;
        for attempt in 1..=3 {
            Timer::after(EmbassyDuration::from_secs(2)).await;
            println!("Wi-Fi reconnect attempt {attempt}/3");
            if matches!(
                with_timeout(
                    EmbassyDuration::from_secs(20),
                    controller.connect_async()
                )
                .await,
                Ok(Ok(_))
            ) {
                println!("Wi-Fi reconnected");
                reconnected = true;
                break;
            }
        }

        if !reconnected {
            println!("Wi-Fi unavailable; restarting into bounded fallback");
            esp_hal::system::software_reset();
        }
    }
}

#[embassy_executor::task]
async fn station_network_status(stack: Stack<'static>) {
    stack.wait_config_up().await;
    if let Some(config) = stack.config_v4() {
        println!("Station address: {}", config.address);
    }
    core::future::pending::<()>().await;
}

#[embassy_executor::task]
async fn temperature_receiver(
    stack: Stack<'static>,
    token: heapless::String<{ config::MAX_TOKEN_LEN }>,
) {
    let mut receive_metadata = [PacketMetadata::EMPTY; 2];
    let mut receive_buffer = [0_u8; 512];
    let mut transmit_metadata = [PacketMetadata::EMPTY; 1];
    let mut transmit_buffer = [0_u8; 128];
    let mut packet = [0_u8; 256];
    let mut socket = UdpSocket::new(
        stack,
        &mut receive_metadata,
        &mut receive_buffer,
        &mut transmit_metadata,
        &mut transmit_buffer,
    );
    socket
        .bind(control::CONTROL_PORT)
        .expect("failed to bind temperature-control socket");
    stack.wait_config_up().await;
    println!("Temperature control listening on UDP {}", control::CONTROL_PORT);

    loop {
        match socket.recv_from(&mut packet).await {
            Ok((length, _remote)) => {
                let now_millis = EmbassyInstant::now().as_millis();
                let result = critical_section::with(|critical_section| {
                    HOST_STATE.borrow(critical_section).borrow_mut().accept(
                        token.as_str(),
                        &packet[..length],
                        now_millis,
                    )
                });
                match result {
                    Ok(command) => println!(
                        "Host command: {}.{:03} C, requested duty {}% (sequence {})",
                        command.temperature_milli_c / 1000,
                        command.temperature_milli_c.unsigned_abs() % 1000,
                        command.requested_duty_percent,
                        command.sequence
                    ),
                    Err(error) => println!("Rejected temperature packet: {error:?}"),
                }
            }
            Err(error) => println!("Temperature UDP receive error: {error:?}"),
        }
    }
}

#[embassy_executor::task]
async fn wifi_access_point_events(controller: WifiController<'static>) {
    loop {
        if let Ok(event) = controller.wait_for_access_point_connected_event_async().await {
            println!("Setup AP event: {event:?}");
        }
    }
}

#[embassy_executor::task]
async fn dhcp_server(stack: Stack<'static>) {
    use edge_dhcp::{
        io::{self, DEFAULT_SERVER_PORT},
        server::{Server, ServerOptions},
    };
    use edge_nal::UdpBind;
    use edge_nal_embassy::{Udp, UdpBuffers};

    let setup_ip = Ipv4Addr::from_str(SETUP_IP).expect("invalid setup IP address");
    let mut packet = [0_u8; 1500];
    let mut gateway = [Ipv4Addr::UNSPECIFIED];
    let buffers = UdpBuffers::<3, 1024, 1024, 10>::new();
    let udp = Udp::new(stack, &buffers);
    let mut socket = udp
        .bind(core::net::SocketAddr::V4(core::net::SocketAddrV4::new(
            Ipv4Addr::UNSPECIFIED,
            DEFAULT_SERVER_PORT,
        )))
        .await
        .expect("failed to bind DHCP socket");

    loop {
        if let Err(error) = io::server::run(
            &mut Server::<_, 64>::new_with_et(setup_ip),
            &ServerOptions::new(setup_ip, Some(&mut gateway)),
            &mut socket,
            &mut packet,
        )
        .await
        {
            println!("DHCP error: {error:?}");
        }
        Timer::after(EmbassyDuration::from_millis(500)).await;
    }
}

#[embassy_executor::task(pool_size = 3)]
async fn setup_http_server(stack: Stack<'static>) {
    use embedded_io_async::Write;

    const FORM_PAGE: &[u8] = b"HTTP/1.1 200 OK\r\n\
Content-Type: text/html; charset=utf-8\r\n\
Connection: close\r\n\
\r\n\
<!doctype html><html><head><meta name=viewport content='width=device-width'>\
<link rel=icon href='data:,'><title>Laptop cooler setup</title></head>\
<body><h1>Laptop cooler setup</h1>\
<form method=post action=/save>\
<label>Wi-Fi name <input name=ssid maxlength=32 required></label><br>\
<label>Wi-Fi password <input type=password name=password maxlength=64></label><br>\
<label>Control token <input name=token minlength=16 maxlength=32 pattern='[A-Za-z0-9_-]+' required></label><br>\
<button type=submit>Save and connect</button></form>\
<p>The token must contain 16-32 letters, numbers, underscores, or hyphens.</p>\
</body></html>";
    const SAVED_PAGE: &[u8] = b"HTTP/1.1 200 OK\r\n\
Content-Type: text/html; charset=utf-8\r\n\
Connection: close\r\n\
\r\n\
<!doctype html><html><body><h1>Saved</h1><p>The cooler is restarting.</p></body></html>";
    const BAD_REQUEST: &[u8] = b"HTTP/1.1 400 Bad Request\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Connection: close\r\n\
\r\nInvalid setup request. Check the field lengths and token format.";
    const SAVE_FAILED: &[u8] = b"HTTP/1.1 500 Internal Server Error\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Connection: close\r\n\
\r\nThe configuration could not be saved. The existing configuration is unchanged.";
    const NOT_FOUND: &[u8] = b"HTTP/1.1 404 Not Found\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Connection: close\r\n\
\r\nNot found.";

    let mut receive_buffer = [0_u8; 1536];
    let mut transmit_buffer = [0_u8; 1536];
    let mut request_buffer = [0_u8; 2048];
    let mut socket = TcpSocket::new(stack, &mut receive_buffer, &mut transmit_buffer);
    socket.set_timeout(Some(EmbassyDuration::from_secs(3)));

    loop {
        if let Err(error) = socket
            .accept(IpListenEndpoint {
                addr: None,
                port: 80,
            })
            .await
        {
            println!("HTTP accept error: {error:?}");
            continue;
        }

        let mut used = 0_usize;
        let parsed_request = loop {
            if used == request_buffer.len() {
                break Err(provisioning::RequestError::RequestTooLarge);
            }
            match socket.read(&mut request_buffer[used..]).await {
                Ok(0) => break Err(provisioning::RequestError::InvalidRequest),
                Ok(length) => {
                    used += length;
                    match provisioning::parse_request(
                        &request_buffer[..used],
                        request_buffer.len(),
                    ) {
                        Ok(Some(request)) => break Ok(request),
                        Ok(None) => {}
                        Err(error) => break Err(error),
                    }
                }
                Err(error) => {
                    println!("HTTP read error: {error:?}");
                    break Err(provisioning::RequestError::InvalidRequest);
                }
            }
        };

        let mut restart_after_response = false;
        let response = match parsed_request {
            Ok(provisioning::Request::ShowForm) => FORM_PAGE,
            Ok(provisioning::Request::Save(config)) => {
                NEW_CONFIG.send(config).await;
                if CONFIG_SAVE_RESULT.receive().await {
                    restart_after_response = true;
                    SAVED_PAGE
                } else {
                    SAVE_FAILED
                }
            }
            Ok(provisioning::Request::NotFound) => NOT_FOUND,
            Err(error) => {
                println!("Rejected setup request: {error:?}");
                BAD_REQUEST
            }
        };

        if let Err(error) = socket.write_all(response).await {
            println!("HTTP write error: {error:?}");
        }
        let _ = socket.flush().await;
        socket.close();
        Timer::after(EmbassyDuration::from_millis(100)).await;
        socket.abort();

        if restart_after_response {
            println!("Restarting with the saved Wi-Fi configuration");
            Timer::after(EmbassyDuration::from_millis(500)).await;
            esp_hal::system::software_reset();
        }
    }
}
