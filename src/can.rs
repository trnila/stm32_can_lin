//! CAN to USB bridge task
//! Each CAN channel is handled in its own task and is having dedicated queue for CAN frames and commands from USB
//! Received and acknowledged frames are sent back to USB via a shared queue for all CAN channel
use crate::gs_usb::FrameType;
use crate::gs_usb::UsbCommand;
use crate::gs_usb::UsbCommandRx;
use crate::gs_usb::UsbResponse;
use crate::gs_usb::UsbResponseTx;
use core::num::NonZero;
use defmt::error;
use embassy_futures::select::Either::{First, Second};
use embassy_futures::select::select;
use embassy_stm32::can;
use embassy_stm32::can::Can;
use embassy_stm32::can::CanConfigurator;
use embassy_stm32::can::Frame;
use embassy_stm32::can::config::FdCanConfig;
use embassy_stm32::gpio::Output;

enum CanState {
    /// CAN is in configuration mode, transceiver is in standby
    Configurable(CanConfigurator<'static>),
    /// Normal operation mode with receiving and transmitting frames
    Normal(Can<'static>),
}

/// Task configuring CAN channel based on commands received from USB
/// and bridging CAN frames to/from USB.
#[embassy_executor::task(pool_size = 3)]
pub async fn can_usb_bridge(
    channel: u8,
    can: CanConfigurator<'static>,
    _stby: Output<'static>,
    mut term_en: Output<'static>,
    from_usb: UsbCommandRx,
    to_usb: UsbResponseTx,
) {
    // Start in configuration mode, gs_usb linux driver configures bitrate and starts normal mode via USB.
    let mut can_state = CanState::Configurable(can);
    loop {
        can_state = match can_state {
            CanState::Configurable(can_configurator) => {
                run_configurable(can_configurator, &mut term_en, from_usb).await
            }
            CanState::Normal(can) => run_normal(channel, can, from_usb, to_usb).await,
        };
    }
}

/// Run CAN in normal operation mode, bridging frames to/from USB.
/// Function returns when a Stop command is received from USB, switching back to configurable mode.
async fn run_normal(
    channel: u8,
    mut can: Can<'static>,
    from_usb: UsbCommandRx,
    to_usb: UsbResponseTx,
) -> CanState {
    loop {
        match select(from_usb.receive(), can.read_fd()).await {
            First(cmd) => match cmd {
                UsbCommand::Reset => {
                    return CanState::Configurable(can.into_config_mode());
                }
                UsbCommand::TxFrame { frame, echo_id } => {
                    match frame {
                        crate::gs_usb::FrameType::Classic(frame) => {
                            can.write(&frame).await;
                        }
                        crate::gs_usb::FrameType::FD(fd_frame) => {
                            can.write_fd(&fd_frame).await;
                        }
                    };

                    // send echo ID back to USB to acknowledge transmission
                    to_usb.send(UsbResponse::EchoId { channel, echo_id }).await;
                }
                cmd => {
                    error!("Unknown command in normal state: {}", cmd);
                }
            },
            Second(frame) => {
                match frame {
                    Ok(frame) => {
                        let (frame, _ts) = frame.parts();
                        let frame = if frame.header().fdcan() {
                            FrameType::FD(frame)
                        } else {
                            FrameType::Classic(Frame::new(*frame.header(), frame.data()).unwrap())
                        };
                        to_usb.send(UsbResponse::RxFrame { channel, frame }).await;
                    }
                    Err(err) => error!("Channel{} RX error: {}", channel, err),
                };
            }
        }
    }
}

/// Run CAN in configurable mode, processing commands from USB to set bitrate and start normal operation.
/// No CAN frames are processed in this mode.
async fn run_configurable(
    mut can: CanConfigurator<'static>,
    term_en: &mut Output<'static>,
    from_usb: UsbCommandRx,
) -> CanState {
    let mut config = FdCanConfig::default()
        .set_automatic_retransmit(false)
        .set_automatic_bus_off_recovery(false)
        .set_frame_transmit(can::config::FrameTransmissionConfig::AllowFdCanAndBRS);

    loop {
        match from_usb.receive().await {
            UsbCommand::Start => {
                can.set_config(config);
                return CanState::Normal(can.start(can::OperatingMode::NormalOperationMode));
            }
            UsbCommand::SetNominalBitTiming(bit_timing) => {
                config = config.set_nominal_bit_timing(can::config::NominalBitTiming {
                    prescaler: NonZero::new(bit_timing.brp as u16).unwrap(),
                    seg1: NonZero::new((bit_timing.prop_seg + bit_timing.phase_seg1) as u8)
                        .unwrap(),
                    seg2: NonZero::new(bit_timing.phase_seg2 as u8).unwrap(),
                    sync_jump_width: NonZero::new(bit_timing.sjw as u8).unwrap(),
                });
            }
            UsbCommand::SetDataBitTiming(bit_timing) => {
                config = config.set_data_bit_timing(can::config::DataBitTiming {
                    prescaler: NonZero::new(bit_timing.brp as u16).unwrap(),
                    seg1: NonZero::new((bit_timing.prop_seg + bit_timing.phase_seg1) as u8)
                        .unwrap(),
                    seg2: NonZero::new(bit_timing.phase_seg2 as u8).unwrap(),
                    sync_jump_width: NonZero::new(bit_timing.sjw as u8).unwrap(),
                    transceiver_delay_compensation: false,
                });
            }
            UsbCommand::SetTermination(enabled) => {
                if enabled {
                    term_en.set_high();
                } else {
                    term_en.set_low();
                }
            }
            cmd => {
                error!("Unknown command in configurable state: {}", cmd);
            }
        }
    }
}
