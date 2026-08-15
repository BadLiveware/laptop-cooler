#![no_std]
#![no_main]
#![deny(clippy::mem_forget)]
#![deny(clippy::large_stack_frames)]

use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::{DriveMode, Input, InputConfig, Pull},
    ledc::{
        LSGlobalClkSource, Ledc, LowSpeed,
        channel::{self, ChannelIFace},
        timer::{self, TimerIFace},
    },
    main,
    time::{Duration, Instant, Rate},
};
use esp_println::println;

esp_bootloader_esp_idf::esp_app_desc!();

const PWM_DUTY_PERCENT: u8 = 50;
const TACH_PULSES_PER_REVOLUTION: u64 = 2;
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

#[main]
fn main() -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

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
        duty_pct: PWM_DUTY_PERCENT,
        drive_mode: DriveMode::PushPull,
    })
    .expect("failed to configure fan PWM output");

    println!();
    println!("Laptop cooler fan test");
    println!("PWM: GPIO32, 25 kHz, {}% duty", PWM_DUTY_PERCENT);
    println!("Tach: GPIO27, internal pull-up, 2 pulses/revolution");

    let mut sample_started_at = Instant::now();
    let mut pulse_count = 0_u64;
    let mut tach_was_low = tach.is_low();

    loop {
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
    }
}
