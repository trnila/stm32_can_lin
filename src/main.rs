#![no_std]
#![no_main]

use crate::can::can_usb_bridge;
use crate::gs_usb::CHANNEL_QUEUE_SIZE;
use crate::gs_usb::USB_QUEUE_SIZE;
use crate::gs_usb::UsbCommand;
use crate::gs_usb::UsbCommandQueues;
use crate::gs_usb::UsbResponse;
use crate::gs_usb::usb_init;
use embassy_executor::Spawner;
use embassy_stm32::bind_interrupts;
use embassy_stm32::can::CanConfigurator;
use embassy_stm32::gpio::Level;
use embassy_stm32::gpio::Output;
use embassy_stm32::gpio::Speed;
use embassy_stm32::peripherals;
use embassy_stm32::rcc;
use embassy_stm32::time::mhz;
use embassy_stm32::usb::Driver;
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use embassy_sync::channel::Channel;
use {defmt_rtt as _, panic_probe as _};

mod can;
mod gs_usb;

static USB_CMDS_CH0: Channel<ThreadModeRawMutex, UsbCommand, CHANNEL_QUEUE_SIZE> = Channel::new();
static USB_CMDS_CH1: Channel<ThreadModeRawMutex, UsbCommand, CHANNEL_QUEUE_SIZE> = Channel::new();
static USB_CMDS_CH2: Channel<ThreadModeRawMutex, UsbCommand, CHANNEL_QUEUE_SIZE> = Channel::new();
static USB_RESPS: Channel<ThreadModeRawMutex, UsbResponse, USB_QUEUE_SIZE> = Channel::new();

bind_interrupts!(struct Irqs {
    USB_LP => embassy_stm32::usb::InterruptHandler<peripherals::USB>;
    FDCAN1_IT0 => embassy_stm32::can::IT0InterruptHandler<peripherals::FDCAN1>;
    FDCAN1_IT1 => embassy_stm32::can::IT1InterruptHandler<peripherals::FDCAN1>;
    FDCAN2_IT0 => embassy_stm32::can::IT0InterruptHandler<peripherals::FDCAN2>;
    FDCAN2_IT1 => embassy_stm32::can::IT1InterruptHandler<peripherals::FDCAN2>;
    FDCAN3_IT0 => embassy_stm32::can::IT0InterruptHandler<peripherals::FDCAN3>;
    FDCAN3_IT1 => embassy_stm32::can::IT1InterruptHandler<peripherals::FDCAN3>;
});

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_stm32::init({
        let mut config = embassy_stm32::Config::default();
        config.rcc.hsi = false;
        config.rcc.hse = Some(rcc::Hse {
            freq: mhz(24),
            mode: rcc::HseMode::Oscillator,
        });
        config.rcc.sys = rcc::Sysclk::Pll1R;
        config.rcc.pll = Some(rcc::Pll {
            source: rcc::PllSource::Hse,
            prediv: rcc::PllPreDiv::Div6,
            mul: rcc::PllMul::Mul85,
            divr: Some(rcc::PllRDiv::Div2),
            divq: Some(rcc::PllQDiv::Div2),
            divp: Some(rcc::PllPDiv::Div2),
        });
        config.rcc.ahb_pre = rcc::AHBPrescaler::Div1;
        config.rcc.apb1_pre = rcc::APBPrescaler::Div2;
        config.rcc.mux.fdcansel = rcc::mux::Fdcansel::Hse;
        config
    });

    // FDCAN0
    spawner.spawn(
        can_usb_bridge(
            0,
            CanConfigurator::new(p.FDCAN2, p.PB12, p.PB13, Irqs),
            Output::new(p.PC10, Level::Low, Speed::Low),
            Output::new(p.PC11, Level::Low, Speed::Low),
            USB_CMDS_CH0.receiver(),
            USB_RESPS.sender(),
        )
        .unwrap(),
    );

    // FDCAN1
    spawner.spawn(
        can_usb_bridge(
            1,
            CanConfigurator::new(p.FDCAN3, p.PA8, p.PA15, Irqs),
            Output::new(p.PC12, Level::Low, Speed::Low),
            Output::new(p.PD2, Level::Low, Speed::Low),
            USB_CMDS_CH1.receiver(),
            USB_RESPS.sender(),
        )
        .unwrap(),
    );

    // FDCAN2
    spawner.spawn(
        can_usb_bridge(
            2,
            CanConfigurator::new(p.FDCAN1, p.PB8, p.PB9, Irqs),
            Output::new(p.PC13, Level::Low, Speed::Low),
            Output::new(p.PC14, Level::Low, Speed::Low),
            USB_CMDS_CH2.receiver(),
            USB_RESPS.sender(),
        )
        .unwrap(),
    );

    let usb = Driver::new(p.USB, Irqs, p.PA12, p.PA11);
    spawner.spawn(
        usb_init(
            usb,
            USB_RESPS.receiver(),
            UsbCommandQueues {
                channel: [
                    USB_CMDS_CH0.sender(),
                    USB_CMDS_CH1.sender(),
                    USB_CMDS_CH2.sender(),
                ],
            },
        )
        .unwrap(),
    );
}
