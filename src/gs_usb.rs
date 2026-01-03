use binary_layout::prelude::*;
use defmt::Format;
use embassy_futures::{block_on, join::join3};
use embassy_stm32::{can::Frame, peripherals::USB};
use embassy_sync::{blocking_mutex::raw::ThreadModeRawMutex, channel::Receiver};
use embassy_usb::{
    Builder, Handler,
    control::{InResponse, OutResponse, Recipient, Request, RequestType},
    driver::{EndpointAddress, EndpointIn, EndpointOut},
    types::InterfaceNumber,
};
use embedded_can::{ExtendedId, Id, StandardId};

pub type UsbCommandRx =
    embassy_sync::channel::Receiver<'static, ThreadModeRawMutex, UsbCommand, CHANNEL_QUEUE_SIZE>;
pub type UsbResponseTx =
    embassy_sync::channel::Sender<'static, ThreadModeRawMutex, UsbResponse, USB_QUEUE_SIZE>;

/// Size of per-interface queue for commands from USB
pub const CHANNEL_QUEUE_SIZE: usize = 256;
/// Size of shared queue for responses to USB
pub const USB_QUEUE_SIZE: usize = 1024;
/// Number of CAN interfaces supported
pub const INTERFACES: usize = 3;
/// Maximal size of USB BULK packet for USB Full-Speed
const BULK_MAX_PACKET_SIZE: u16 = 64;
/// Maximal size of USB transfer for one CAN message
const BULK_BUF_SIZE: usize = 128;

/// Frequency of the CAN peripheral clock in Hz (used for bit timing calculations in Linux)
const CAN_FREQ: u32 = 24_000_000;

const GS_USB_BREQ_HOST_FORMAT: u8 = 0;
const GS_USB_BREQ_BITTIMING: u8 = 1;
const GS_USB_BREQ_MODE: u8 = 2;
const GS_USB_BREQ_BT_CONST: u8 = 4;
const GS_USB_BREQ_DEVICE_CONFIG: u8 = 5;

const GS_CAN_MODE_RESET: u32 = 0;
const GS_CAN_MODE_START: u32 = 1;

const CAN_EFF_FLAG: u32 = 0x80000000;
const CAN_EFF_MASK: u32 = 0x1FFFFFFF;

binary_layout!(gs_host_config, LittleEndian, {
    // Determines USB packets endianess, must be 0xBEEF for little endian
    byte_order: u32,
});

binary_layout!(gs_device_config, LittleEndian, {
    reserved1: u8,
    reserved2: u8,
    reserved3: u8,
    // Number of CAN interfaces - 1
    icount: u8,
    sw_version: u32,
    hw_version: u32,
});

binary_layout!(gs_device_bt_const, LittleEndian, {
    // Supported features
    feature: u32,
    // CAN clock frequency in Hz
    fclk_can: u32,

    // TSEG1 (prop_seg + phase_seg1) limits
    tseg1_min: u32,
    tseg1_max: u32,

    // TSEG2 (phase_seg2) limits
    tseg2_min: u32,
    tseg2_max: u32,

    // SJW (synchronization jump width) max
    sjw_max: u32,

    // BRP (baudrate prescaler) limits
    brp_min: u32,
    brp_max: u32,
    brp_inc: u32,
});

binary_layout!(gs_host_frame_hdr, LittleEndian, {
    // ID used for acknowledging transmitted frames, -1 for received frames
    echo_id: u32,
    // CAN ID with flags (EFF/standard)
    can_id: u32,
    // Payload size in bytes
    can_dlc: u8,
    // CAN interface number
    channel: u8,
    flags: u8,
    reserved: u8,
});

binary_layout!(gs_host_frame, LittleEndian, {
    hdr: gs_host_frame_hdr::NestedView,
    data: [u8; 8],
});

binary_layout!(gs_device_bittiming, LittleEndian, {
    prop_seg: u32,
    phase_seg1: u32,
    phase_seg2: u32,
    sjw: u32,
    brp: u32,
});

binary_layout!(gs_device_mode, LittleEndian, {
    // Reset or Start
    mode: u32,
    flags: u32,
});

/// Get the structure view if it fits into the buffer
macro_rules! assert_view_fits {
    ($module:ident, $buf:expr) => {{
        let len: usize = ($module::SIZE).unwrap();
        defmt::assert!(*$buf.len() >= len);
        ($module::View::new($buf), len)
    }};
}

/// Commands sent from USB to CAN task
#[derive(Format)]
pub enum UsbCommand {
    /// Reset the channel into configuration mode
    Reset,
    /// Start receiving/transmitting CAN frames
    Start,
    /// Set nominal bit timing parameters (only in configuration mode)
    SetNominalBitTiming(BitTiming),
    /// Transmit CAN frame (only in normal mode)
    TxFrame {
        frame: Frame,
        /// ID to send back to USB on successful transmission
        echo_id: u32,
    },
}

/// Responses sent from CAN task to USB
pub enum UsbResponse {
    /// Acknowledgement of transmitted frame
    EchoId { channel: u8, echo_id: u32 },
    /// Received CAN frame
    RxFrame { channel: u8, frame: Frame },
}

#[derive(Debug, Format)]
pub struct BitTiming {
    pub prop_seg: u32,
    pub phase_seg1: u32,
    pub phase_seg2: u32,
    pub sjw: u32,
    pub brp: u32,
}

/// Per-interface queues for commands from USB
#[derive(Clone)]
pub struct UsbCommandQueues {
    pub channel: [embassy_sync::channel::Sender<
        'static,
        ThreadModeRawMutex,
        UsbCommand,
        CHANNEL_QUEUE_SIZE,
    >; INTERFACES],
}

/// USB control request handler for gs_usb
struct GsUsbControlHandler {
    if_num: InterfaceNumber,
    command_queues: UsbCommandQueues,
}

impl Handler for GsUsbControlHandler {
    fn control_out<'a>(&'a mut self, req: Request, buf: &'a [u8]) -> Option<OutResponse> {
        if req.request_type != RequestType::Vendor || req.recipient != Recipient::Interface {
            return None;
        }

        if req.index != self.if_num.0 as u16 {
            return None;
        }

        let can_channel = req.value as usize;
        let command_queue = self.command_queues.channel[can_channel];
        Some(match req.request {
            GS_USB_BREQ_HOST_FORMAT => {
                let (view, _len) = assert_view_fits!(gs_host_config, &buf);
                // Only little endian communication is supported
                if view.byte_order().read() == 0xbeef {
                    OutResponse::Accepted
                } else {
                    OutResponse::Rejected
                }
            }
            GS_USB_BREQ_BITTIMING => {
                let (view, _len) = assert_view_fits!(gs_device_bittiming, &buf);
                let bit_timing = BitTiming {
                    prop_seg: view.prop_seg().read(),
                    phase_seg1: view.phase_seg1().read(),
                    phase_seg2: view.phase_seg2().read(),
                    sjw: view.sjw().read(),
                    brp: view.brp().read(),
                };
                block_on(command_queue.send(UsbCommand::SetNominalBitTiming(bit_timing)));
                OutResponse::Accepted
            }
            GS_USB_BREQ_MODE => {
                let (view, _len) = assert_view_fits!(gs_device_mode, &buf);
                block_on(command_queue.send(match view.mode().read() {
                    GS_CAN_MODE_RESET => UsbCommand::Reset,
                    GS_CAN_MODE_START => UsbCommand::Start,
                    _ => unreachable!(),
                }));
                OutResponse::Accepted
            }
            _ => OutResponse::Rejected,
        })
    }

    fn control_in<'a>(&'a mut self, req: Request, mut buf: &'a mut [u8]) -> Option<InResponse<'a>> {
        if req.request_type != RequestType::Vendor || req.recipient != Recipient::Interface {
            return None;
        }

        if req.index != self.if_num.0 as u16 {
            return None;
        }

        match req.request {
            GS_USB_BREQ_DEVICE_CONFIG => {
                let (mut view, len) = assert_view_fits!(gs_device_config, &mut buf);
                view.icount_mut().write((INTERFACES - 1) as u8);
                view.sw_version_mut().write(18);
                view.hw_version_mut().write(11);
                Some(InResponse::Accepted(&buf[..len]))
            }
            GS_USB_BREQ_BT_CONST => {
                let (mut view, len) = assert_view_fits!(gs_device_bt_const, &mut buf);
                view.fclk_can_mut().write(CAN_FREQ);

                view.tseg1_min_mut().write(1);
                view.tseg1_max_mut().write(256);

                view.tseg2_min_mut().write(1);
                view.tseg2_max_mut().write(128);

                view.sjw_max_mut().write(128);

                view.brp_min_mut().write(1);
                view.brp_max_mut().write(512);
                view.brp_inc_mut().write(1);
                Some(InResponse::Accepted(&buf[..len]))
            }
            _ => Some(InResponse::Rejected),
        }
    }
}

/// Pumps CAN frames from USB into per-interface queues
async fn usb_to_can<T: EndpointOut>(mut read_ep: T, tx: UsbCommandQueues) {
    let mut usb_buf = [0; BULK_BUF_SIZE];
    loop {
        read_ep.wait_enabled().await;
        loop {
            match read_ep.read_transfer(&mut usb_buf).await {
                Ok(n) => {
                    // Check the USB packet contains the full CAN frame
                    assert!(gs_host_frame_hdr::SIZE.unwrap() <= n);
                    let view = gs_host_frame::View::new(usb_buf);
                    let dlc = view.hdr().can_dlc().read() as usize;
                    assert!(gs_host_frame_hdr::SIZE.unwrap() + dlc <= n);

                    // Extract into CAN frame and push to can_to_usb task queue
                    let can_id = {
                        let raw_can_id = view.hdr().can_id().read();
                        if (raw_can_id & CAN_EFF_FLAG) > 0 {
                            Id::Extended(ExtendedId::new(raw_can_id & CAN_EFF_MASK).unwrap())
                        } else {
                            Id::Standard(StandardId::new(raw_can_id as u16).unwrap())
                        }
                    };
                    let frame = Frame::new_data(can_id, &view.data()[..dlc]).unwrap();
                    let echo_id = view.hdr().echo_id().read();
                    let channel = view.hdr().channel().read() as usize;
                    tx.channel[channel]
                        .send(UsbCommand::TxFrame { frame, echo_id })
                        .await;
                }
                Err(err) => {
                    defmt::error!("USB RX failed: {}", err);
                    break;
                }
            }
        }
    }
}

/// Pumps CAN frames from a shared queue to USB
async fn can_to_usb<T: EndpointIn>(
    mut write_ep: T,
    rx: Receiver<'static, ThreadModeRawMutex, UsbResponse, USB_QUEUE_SIZE>,
) {
    let mut usb_buf = [0; BULK_BUF_SIZE];
    loop {
        let frame = rx.receive().await;

        // Determine and check it fits into USB packet
        let usb_len = gs_host_frame_hdr::SIZE.unwrap()
            + match frame {
                UsbResponse::RxFrame { frame, .. } => {
                    assert!(frame.data().len() == frame.header().len() as usize);
                    frame.data().len()
                }
                UsbResponse::EchoId { .. } => 0,
            };
        assert!(usb_len <= usb_buf.len());

        let mut view = gs_host_frame::View::new(&mut usb_buf);
        let mut hdr = view.hdr_mut();
        match frame {
            UsbResponse::EchoId { channel, echo_id } => {
                // Only echo_id and channel_id are relavant to confirm the frame was sent on the bus
                hdr.echo_id_mut().write(echo_id);
                hdr.can_id_mut().write(0);
                hdr.can_dlc_mut().write(0);
                hdr.channel_mut().write(channel);
                hdr.flags_mut().write(0);
                hdr.reserved_mut().write(0);
            }
            UsbResponse::RxFrame { channel, frame } => {
                hdr.echo_id_mut().write(u32::MAX);
                hdr.can_id_mut().write(match frame.id() {
                    embedded_can::Id::Standard(standard_id) => standard_id.as_raw() as u32,
                    embedded_can::Id::Extended(extended_id) => CAN_EFF_FLAG | extended_id.as_raw(),
                });
                hdr.can_dlc_mut().write(frame.header().len());
                hdr.channel_mut().write(channel);
                hdr.flags_mut().write(0);
                hdr.reserved_mut().write(0);
                view.data_mut()[..frame.header().len() as usize].copy_from_slice(frame.data());
            }
        }

        write_ep
            .write_transfer(&usb_buf[..usb_len], false)
            .await
            .unwrap();
    }
}

#[embassy_executor::task]
pub async fn usb_init(
    driver: embassy_stm32::usb::Driver<'static, USB>,
    response_queue: embassy_sync::channel::Receiver<
        'static,
        ThreadModeRawMutex,
        UsbResponse,
        USB_QUEUE_SIZE,
    >,
    command_queues: UsbCommandQueues,
) {
    let mut config = embassy_usb::Config::new(0x1209, 0x2323);
    config.manufacturer = Some("trnila");
    config.product = Some("STM32 CAN LIN");
    config.serial_number = Some("1");

    let mut config_descriptor = [0; 256];
    let mut bos_descriptor = [0; 256];
    let mut msos_descriptor = [0; 256];
    let mut control_buf = [0; 64];

    let mut handler = GsUsbControlHandler {
        if_num: InterfaceNumber(0),
        command_queues: command_queues.clone(),
    };

    let mut builder = Builder::new(
        driver,
        config,
        &mut config_descriptor,
        &mut bos_descriptor,
        &mut msos_descriptor,
        &mut control_buf,
    );

    let mut function = builder.function(0xFF, 0, 0);
    let mut interface = function.interface();
    handler.if_num = interface.interface_number();
    let mut alt = interface.alt_setting(0xFF, 0, 0, None);
    let read_ep = alt.endpoint_bulk_out(
        Some(EndpointAddress::from_parts(
            1,
            embassy_usb::driver::Direction::Out,
        )),
        BULK_MAX_PACKET_SIZE,
    );
    let write_ep = alt.endpoint_bulk_in(
        Some(EndpointAddress::from_parts(
            1,
            embassy_usb::driver::Direction::In,
        )),
        BULK_MAX_PACKET_SIZE,
    );
    drop(function);
    builder.handler(&mut handler);

    let mut usb = builder.build();
    join3(
        usb.run(),
        usb_to_can(read_ep, command_queues),
        can_to_usb(write_ep, response_queue),
    )
    .await;
}
