#![macro_use]
#![allow(missing_docs)]
use core::future::poll_fn;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use core::task::Poll;

use embassy_hal_internal::into_ref;
use embassy_sync::waitqueue::AtomicWaker;
use embassy_time::{Duration, Instant, Timer};
use embassy_usb_driver::host::{channel, ChannelError, DeviceEvent, HostError, UsbChannel, UsbHostDriver};
use embassy_usb_driver::{EndpointType, Speed};
use stm32_metapac::common::{Reg, RW};
use stm32_metapac::usb::regs::Epr;

use super::{DmPin, DpPin, Instance};
use crate::pac::usb::regs;
use crate::pac::usb::vals::{EpType, Stat};
use crate::pac::USBRAM;
use crate::{interrupt, Peripheral};

/// The number of registers is 8, allowing up to 16 mono-
/// directional/single-buffer or up to 7 double-buffer endpoints in any combination. For
/// example the USB peripheral can be programmed to have 4 double buffer endpoints
/// and 8 single-buffer/mono-directional endpoints.
const USB_MAX_PIPES: usize = 8;

/// Interrupt handler.
pub struct USBHostInterruptHandler<I: Instance> {
    _phantom: PhantomData<I>,
}

impl<I: Instance> interrupt::typelevel::Handler<I::Interrupt> for USBHostInterruptHandler<I> {
    unsafe fn on_interrupt() {
        let regs = I::regs();
        // let x = regs.istr().read().0;
        // trace!("USB IRQ: {:08x}", x);

        let istr = regs.istr().read();

        // Detect device connect/disconnect
        if istr.reset() {
            trace!("USB IRQ: device connect/disconnect");

            // Write 0 to clear.
            let mut clear = regs::Istr(!0);
            clear.set_reset(false);
            regs.istr().write_value(clear);

            // Wake main thread.
            BUS_WAKER.wake();
        }

        if istr.ctr() {
            let index = istr.ep_id() as usize;

            let epr = regs.epr(index).read();

            let mut epr_value = invariant(epr);
            // Check and clear error flags
            if epr.err_tx() {
                epr_value.set_err_tx(false);
                warn!("err_tx");
            }
            if epr.err_rx() {
                epr_value.set_err_rx(false);
                warn!("err_rx");
            }
            // Clear ctr (transaction complete) flags
            let rx_ready = epr.ctr_rx();
            let tx_ready = epr.ctr_tx();

            epr_value.set_ctr_rx(!rx_ready);
            epr_value.set_ctr_tx(!tx_ready);
            regs.epr(index).write_value(epr_value);

            if rx_ready {
                EP_IN_WAKERS[index].wake();
            }
            if tx_ready {
                EP_OUT_WAKERS[index].wake();
            }
        }

        if istr.err() {
            debug!("USB IRQ: err");
            regs.istr().write_value(regs::Istr(!0));

            // Write 0 to clear.
            let mut clear = regs::Istr(!0);
            clear.set_err(false);
            regs.istr().write_value(clear);

            let index = istr.ep_id() as usize;
            let mut epr = invariant(regs.epr(index).read());
            // Toggle endponit to disabled
            epr.set_stat_rx(epr.stat_rx());
            epr.set_stat_tx(epr.stat_tx());
            regs.epr(index).write_value(epr);
        }
    }
}

const EP_COUNT: usize = 8;

#[cfg(any(usbram_16x1_512, usbram_16x2_512))]
const USBRAM_SIZE: usize = 512;
#[cfg(any(usbram_16x2_1024, usbram_32_1024))]
const USBRAM_SIZE: usize = 1024;
#[cfg(usbram_32_2048)]
const USBRAM_SIZE: usize = 2048;

#[cfg(not(any(usbram_32_2048, usbram_32_1024)))]
const USBRAM_ALIGN: usize = 2;
#[cfg(any(usbram_32_2048, usbram_32_1024))]
const USBRAM_ALIGN: usize = 4;

const NEW_AW: AtomicWaker = AtomicWaker::new();
static BUS_WAKER: AtomicWaker = NEW_AW;
static EP_IN_WAKERS: [AtomicWaker; EP_COUNT] = [NEW_AW; EP_COUNT];
static EP_OUT_WAKERS: [AtomicWaker; EP_COUNT] = [NEW_AW; EP_COUNT];

fn convert_type(t: EndpointType) -> EpType {
    match t {
        EndpointType::Bulk => EpType::BULK,
        EndpointType::Control => EpType::CONTROL,
        EndpointType::Interrupt => EpType::INTERRUPT,
        EndpointType::Isochronous => EpType::ISO,
    }
}

fn invariant(mut r: regs::Epr) -> regs::Epr {
    r.set_ctr_rx(true); // don't clear
    r.set_ctr_tx(true); // don't clear
    r.set_dtog_rx(false); // don't toggle
    r.set_dtog_tx(false); // don't toggle
    r.set_stat_rx(Stat::from_bits(0));
    r.set_stat_tx(Stat::from_bits(0));
    r
}

fn align_len_up(len: u16) -> u16 {
    ((len as usize + USBRAM_ALIGN - 1) / USBRAM_ALIGN * USBRAM_ALIGN) as u16
}

/// Calculates the register field values for configuring receive buffer descriptor.
/// Returns `(actual_len, len_bits)`
///
/// `actual_len` length in bytes rounded up to USBRAM_ALIGN
/// `len_bits` should be placed on the upper 16 bits of the register value
fn calc_receive_len_bits(len: u16) -> (u16, u16) {
    match len {
        // NOTE: this could be 2..=62 with 16bit USBRAM, but not with 32bit. Limit it to 60 for simplicity.
        2..=60 => (align_len_up(len), align_len_up(len) / 2 << 10),
        61..=1024 => ((len + 31) / 32 * 32, (((len + 31) / 32 - 1) << 10) | 0x8000),
        _ => panic!("invalid OUT length {}", len),
    }
}

#[cfg(any(usbram_32_2048, usbram_32_1024))]
mod btable {
    use super::*;

    pub(super) fn write_in<I: Instance>(_index: usize, _addr: u16) {}

    /// Writes to Transmit Buffer Descriptor for Channel/endpoint `index``
    /// For Device this is an IN endpoint for Host an OUT endpoint
    pub(super) fn write_transmit_buffer_descriptor<I: Instance>(index: usize, addr: u16, len: u16) {
        // Address offset: index*8 [bytes] thus index*2 in 32 bit words
        USBRAM.mem(index * 2).write_value((addr as u32) | ((len as u32) << 16));
    }

    /// Writes to Receive Buffer Descriptor for Channel/endpoint `index``
    /// For Device this is an OUT endpoint for Host an IN endpoint
    pub(super) fn write_receive_buffer_descriptor<I: Instance>(index: usize, addr: u16, max_len_bits: u16) {
        // Address offset: index*8 + 4 [bytes] thus index*2 + 1 in 32 bit words
        USBRAM
            .mem(index * 2 + 1)
            .write_value((addr as u32) | ((max_len_bits as u32) << 16));
    }

    pub(super) fn read_out_len<I: Instance>(index: usize) -> u16 {
        (USBRAM.mem(index * 2 + 1).read() >> 16) as u16
    }
}

// Maybe replace with struct that only knows its index
struct EndpointBuffer<I: Instance> {
    addr: u16,
    len: u16,
    _phantom: PhantomData<I>,
}

impl<I: Instance> EndpointBuffer<I> {
    fn new(addr: u16, len: u16) -> Self {
        EndpointBuffer {
            addr,
            len,
            _phantom: PhantomData,
        }
    }

    fn read(&mut self, buf: &mut [u8]) {
        assert!(buf.len() <= self.len as usize);
        for i in 0..(buf.len() + USBRAM_ALIGN - 1) / USBRAM_ALIGN {
            let val = USBRAM.mem(self.addr as usize / USBRAM_ALIGN + i).read();
            let n = USBRAM_ALIGN.min(buf.len() - i * USBRAM_ALIGN);
            buf[i * USBRAM_ALIGN..][..n].copy_from_slice(&val.to_le_bytes()[..n]);
        }
    }

    fn write(&mut self, buf: &[u8]) {
        assert!(buf.len() <= self.len as usize);
        for i in 0..(buf.len() + USBRAM_ALIGN - 1) / USBRAM_ALIGN {
            let mut val = [0u8; USBRAM_ALIGN];
            let n = USBRAM_ALIGN.min(buf.len() - i * USBRAM_ALIGN);
            val[..n].copy_from_slice(&buf[i * USBRAM_ALIGN..][..n]);

            #[cfg(not(any(usbram_32_2048, usbram_32_1024)))]
            let val = u16::from_le_bytes(val);
            #[cfg(any(usbram_32_2048, usbram_32_1024))]
            let val = u32::from_le_bytes(val);
            USBRAM.mem(self.addr as usize / USBRAM_ALIGN + i).write_value(val);
        }
    }
}

/// First bit is used to indicate control pipes
/// bitfield for keeping track of used channels
static ALLOCATED_PIPES: AtomicU32 = AtomicU32::new(0);
static EP_MEM_FREE: AtomicU16 = AtomicU16::new(0);

/// USB host driver.
pub struct UsbHost<'d, I: Instance> {
    phantom: PhantomData<&'d mut I>,
    // first free address in EP mem, in bytes.
    // ep_mem_free: u16,
}

impl<'d, I: Instance> UsbHost<'d, I> {
    /// Create a new USB driver.
    pub fn new(
        _usb: impl Peripheral<P = I> + 'd,
        _irq: impl interrupt::typelevel::Binding<I::Interrupt, USBHostInterruptHandler<I>> + 'd,
        dp: impl Peripheral<P = impl DpPin<I>> + 'd,
        dm: impl Peripheral<P = impl DmPin<I>> + 'd,
    ) -> Self {
        into_ref!(dp, dm);

        super::super::common_init::<I>();

        let regs = I::regs();

        regs.cntr().write(|w| {
            w.set_pdwn(false);
            w.set_fres(true);
            w.set_host(true);
        });

        // Wait for voltage reference
        #[cfg(feature = "time")]
        embassy_time::block_for(embassy_time::Duration::from_millis(100));
        #[cfg(not(feature = "time"))]
        cortex_m::asm::delay(unsafe { crate::rcc::get_freqs() }.sys.unwrap().0 / 10);

        #[cfg(not(usb_v4))]
        regs.btable().write(|w| w.set_btable(0));

        #[cfg(not(stm32l1))]
        {
            use crate::gpio::{AfType, OutputType, Speed};
            dp.set_as_af(dp.af_num(), AfType::output(OutputType::PushPull, Speed::VeryHigh));
            dm.set_as_af(dm.af_num(), AfType::output(OutputType::PushPull, Speed::VeryHigh));
        }
        #[cfg(stm32l1)]
        let _ = (dp, dm); // suppress "unused" warnings.

        EP_MEM_FREE.store(EP_COUNT as u16 * 8, Ordering::Relaxed);
        Self {
            phantom: PhantomData,
            // ep_mem_free: EP_COUNT as u16 * 8, // for each EP, 4 regs, so 8 bytes
            // control_channel_in: Channel::new(0, 0, 0, 0),
            // control_channel_out: Channel::new(0, 0, 0, 0),
            // channels_used: 0,
            // channels_out_used: 0,
        }
    }

    /// Start the USB peripheral
    pub fn start(&mut self) {
        let regs = I::regs();

        regs.cntr().write(|w| {
            w.set_host(true);
            w.set_pdwn(false);
            w.set_fres(false);
            // Masks
            w.set_resetm(true);
            w.set_suspm(false);
            w.set_wkupm(false);
            w.set_ctrm(true);
            w.set_errm(false);
        });

        // Enable pull downs on DP and DM lines for host mode
        #[cfg(any(usb_v3, usb_v4))]
        regs.bcdr().write(|w| w.set_dppu(true));

        #[cfg(stm32l1)]
        crate::pac::SYSCFG.pmc().modify(|w| w.set_usb_pu(true));
    }

    pub fn get_status(&self) -> u32 {
        let regs = I::regs();

        let istr = regs.istr().read();

        istr.0
    }

    fn alloc_channel_mem(&self, len: u16) -> Result<u16, ()> {
        assert!(len as usize % USBRAM_ALIGN == 0);
        let addr = EP_MEM_FREE.load(Ordering::Relaxed);
        if addr + len > USBRAM_SIZE as _ {
            // panic!("Endpoint memory full");
            error!("Endpoint memory full");
            return Err(());
        }
        EP_MEM_FREE.store(addr + len, Ordering::Relaxed);
        Ok(addr)
    }
}

// struct EndpointBuffer

/// USB endpoint. Only implements single buffer mode.
pub struct Channel<'d, I: Instance, D: channel::Direction, T: channel::Type> {
    _phantom: PhantomData<(&'d mut I, D, T)>,
    /// Register index (there are 8 in total)
    index: usize,
    max_packet_size_in: u16,
    max_packet_size_out: u16,
    buf_in: Option<EndpointBuffer<I>>,
    buf_out: Option<EndpointBuffer<I>>,
}

impl<'d, I: Instance, D: channel::Direction, T: channel::Type> Channel<'d, I, D, T> {
    fn new(
        index: usize,
        buf_in: Option<EndpointBuffer<I>>,
        buf_out: Option<EndpointBuffer<I>>,
        max_packet_size_in: u16,
        max_packet_size_out: u16,
    ) -> Self {
        Self {
            _phantom: PhantomData,
            index,
            max_packet_size_in,
            max_packet_size_out,
            buf_in,
            buf_out,
        }
    }

    fn reg(&self) -> Reg<Epr, RW> {
        I::regs().epr(self.index)
    }

    pub fn activate_rx(&mut self) {
        let epr = self.reg();
        let epr_val = epr.read();
        let current_stat_rx = epr_val.stat_rx().to_bits();
        let mut epr_val = invariant(epr_val);
        // stat_rx can only be toggled by writing a 1.
        // We want to set it to Valid (0b11)
        let stat_mask = Stat::from_bits(!current_stat_rx & 0x3);
        epr_val.set_stat_rx(stat_mask);
        epr.write_value(epr_val);
    }

    pub fn activate_tx(&mut self) {
        let epr = self.reg();
        let epr_val = epr.read();
        let current_stat_tx = epr_val.stat_tx().to_bits();
        let mut epr_val = invariant(epr_val);
        // stat_tx can only be toggled by writing a 1.
        // We want to set it to Valid (0b11)
        let stat_mask = Stat::from_bits(!current_stat_tx & 0x3);
        epr_val.set_stat_tx(stat_mask);
        epr.write_value(epr_val);
    }

    pub fn disable_rx(&mut self) {
        let epr = self.reg();
        let epr_val = epr.read();
        let current_stat_rx = epr_val.stat_rx();
        let mut epr_val = invariant(epr_val);
        // stat_rx can only be toggled by writing a 1.
        // We want to set it to Disabled (0b00).
        epr_val.set_stat_rx(current_stat_rx);
        epr.write_value(epr_val);
    }

    fn disable_tx(&mut self) {
        let epr = self.reg();
        let epr_val = epr.read();
        let current_stat_tx = epr_val.stat_tx();
        let mut epr_val = invariant(epr_val);
        // stat_tx can only be toggled by writing a 1.
        // We want to set it to InActive (0b00).
        epr_val.set_stat_tx(current_stat_tx);
        epr.write_value(epr_val);
    }

    fn read_data(&mut self, buf: &mut [u8]) -> Result<usize, ChannelError> {
        let index = self.index;
        let rx_len = btable::read_out_len::<I>(index) as usize & 0x3FF;
        trace!("READ DONE, rx_len = {}", rx_len);
        if rx_len > buf.len() {
            return Err(ChannelError::BufferOverflow);
        }
        self.buf_in.as_mut().unwrap().read(&mut buf[..rx_len]);
        Ok(rx_len)
    }

    fn write_data(&mut self, buf: &[u8]) {
        let index = self.index;
        if let Some(buf_out) = self.buf_out.as_mut() {
            buf_out.write(buf);
            btable::write_transmit_buffer_descriptor::<I>(index, buf_out.addr, buf.len() as _);
        }
    }

    async fn write(&mut self, buf: &[u8]) -> Result<usize, ChannelError> {
        self.write_data(buf);

        let index = self.index;
        let timeout_ms = 1000;

        self.activate_tx();

        let regs = I::regs();

        let t0 = Instant::now();

        poll_fn(|cx| {
            EP_OUT_WAKERS[index].register(cx.waker());

            // Detect disconnect
            let istr = regs.istr().read();
            if !istr.dcon_stat() {
                self.disable_tx();
                return Poll::Ready(Err(ChannelError::Disconnected));
            }

            if t0.elapsed() > Duration::from_millis(timeout_ms as u64) {
                // Timeout, we need to stop the current transaction.
                self.disable_tx();
                return Poll::Ready(Err(ChannelError::Timeout));
            }

            let stat = self.reg().read().stat_tx();
            match stat {
                Stat::DISABLED => Poll::Ready(Ok((buf.len()))),
                Stat::STALL => Poll::Ready(Err(ChannelError::Stall)),
                Stat::NAK | Stat::VALID => Poll::Pending,
            }
        })
        .await
    }

    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, ChannelError> {
        let index = self.index;

        let timeout_ms = 1000;

        self.activate_rx();

        let regs = I::regs();

        let mut count: usize = 0;

        let t0 = Instant::now();

        poll_fn(|cx| {
            EP_IN_WAKERS[index].register(cx.waker());

            // Detect disconnect
            let istr = regs.istr().read();
            if !istr.dcon_stat() {
                self.disable_rx();
                return Poll::Ready(Err(ChannelError::Disconnected));
            }

            if t0.elapsed() > Duration::from_millis(timeout_ms as u64) {
                self.disable_rx();
                return Poll::Ready(Err(ChannelError::Timeout));
            }

            let stat = self.reg().read().stat_rx();
            match stat {
                Stat::DISABLED => {
                    // Data available for read
                    let idest = &mut buf[count..];
                    let n = self.read_data(idest)?;
                    count += n;
                    // If transfer is smaller than max_packet_size, we are done
                    // If we have read buf.len() bytes, we are done
                    if count == buf.len() || n < self.max_packet_size_in as usize {
                        Poll::Ready(Ok(count))
                    } else {
                        // More data expected: issue another read.
                        self.activate_rx();
                        Poll::Pending
                    }
                }
                Stat::STALL => {
                    // error
                    Poll::Ready(Err(ChannelError::Stall))
                }
                Stat::NAK => Poll::Pending,
                Stat::VALID => {
                    // not started yet? Try again
                    Poll::Pending
                }
            }
        })
        .await
    }
}

// impl<'d, I: Instance> Channel<'d, D, In> {
// }

impl<'d, I: Instance, T: channel::Type, D: channel::Direction> UsbChannel<T, D> for Channel<'d, I, D, T> {
    async fn control_in(
        &mut self,
        setup: &embassy_usb_driver::host::SetupPacket,
        buf: &mut [u8],
    ) -> Result<usize, ChannelError>
    where
        T: channel::IsControl,
        D: channel::IsIn,
    {
        let epr0 = I::regs().epr(0);

        // setup stage
        let mut epr_val = invariant(epr0.read());
        epr_val.set_setup(true);
        epr0.write_value(epr_val);

        self.write(setup.as_bytes()).await?;

        // data stage
        let count = self.read(buf).await?;

        // status stage

        // Send 0 bytes
        let zero: [u8; 0] = [0u8; 0];
        self.write(&zero).await?;

        Ok(count)
    }

    async fn control_out(
        &mut self,
        setup: &embassy_usb_driver::host::SetupPacket,
        buf: &[u8],
    ) -> Result<usize, ChannelError>
    where
        T: channel::IsControl,
        D: channel::IsOut,
    {
        let epr0 = I::regs().epr(0);

        // setup stage
        let mut epr_val = invariant(epr0.read());
        epr_val.set_setup(true);
        epr0.write_value(epr_val);
        self.write(setup.as_bytes()).await?;

        if buf.is_empty() {
            // do nothing
        } else {
            self.write(buf).await?;
        }

        // Status stage
        let mut status = [0u8; 0];
        self.read(&mut status).await?;

        Ok(buf.len())
    }

    fn retarget_channel(
        &mut self,
        addr: u8,
        endpoint: &embassy_usb_driver::EndpointInfo,
        pre: bool,
    ) -> Result<(), embassy_usb_driver::host::HostError> {
        trace!(
            "retarget_channel: addr: {:?} ep_type: {:?} index: {}",
            addr,
            endpoint.ep_type,
            self.index
        );
        let eptype = endpoint.ep_type;
        let index = self.index;

        // configure channel register
        let epr_reg = I::regs().epr(index);
        let mut epr = invariant(epr_reg.read());
        epr.set_devaddr(addr);
        epr.set_ep_type(convert_type(eptype));
        epr.set_ea(index as _);
        epr_reg.write_value(epr);

        Ok(())
    }

    async fn request_in(&mut self, buf: &mut [u8]) -> Result<usize, ChannelError>
    where
        D: channel::IsIn,
    {
        self.read(buf).await
    }

    async fn request_out(&mut self, buf: &[u8]) -> Result<usize, ChannelError>
    where
        D: channel::IsOut,
    {
        self.write(buf).await
    }
}

impl<'d, I: Instance> UsbHostDriver for UsbHost<'d, I> {
    type Channel<T: channel::Type, D: channel::Direction> = Channel<'d, I, D, T>;

    fn alloc_channel<T: channel::Type, D: channel::Direction>(
        &self,
        addr: u8,
        endpoint: &embassy_usb_driver::EndpointInfo,
        pre: bool,
    ) -> Result<Self::Channel<T, D>, embassy_usb_driver::host::HostError> {
        let new_index = if T::ep_type() == EndpointType::Control {
            // Only a single control channel is available
            0
        } else {
            loop {
                let pipes = ALLOCATED_PIPES.load(Ordering::Relaxed);

                // Ignore index 0
                let new_index = (pipes | 1).trailing_ones();
                if new_index as usize >= USB_MAX_PIPES {
                    Err(HostError::OutOfChannels)?;
                }

                ALLOCATED_PIPES.store(pipes | 1 << new_index, Ordering::Relaxed);

                // TODO make this thread safe using atomics or critical section?
                // cortex m0 does not have compare_exchange_weak, only load and store
                // if ALLOCATED_PIPES
                //     .compare_exchange_weak(
                //         pipes,
                //         pipes | 1 << new_index,
                //         core::sync::atomic::Ordering::Acquire,
                //         core::sync::atomic::Ordering::Relaxed,
                //     )
                //     .is_ok()
                // {
                //     break new_index;
                // }
                break new_index;
            }
        };

        let max_packet_size = endpoint.max_packet_size;

        let buffer_in = if D::is_in() {
            let (len, len_bits) = calc_receive_len_bits(max_packet_size);
            let Ok(buffer_addr) = self.alloc_channel_mem(len) else {
                return Err(HostError::OutOfSlots);
            };

            btable::write_receive_buffer_descriptor::<I>(new_index as usize, buffer_addr, len_bits);

            Some(EndpointBuffer::new(buffer_addr, len))
        } else {
            None
        };

        let buffer_out = if D::is_out() {
            let len = align_len_up(max_packet_size);
            let Ok(buffer_addr) = self.alloc_channel_mem(len) else {
                return Err(HostError::OutOfSlots);
            };

            // ep_in_len is written when actually TXing packets.
            btable::write_in::<I>(new_index as usize, buffer_addr);

            Some(EndpointBuffer::new(buffer_addr, len))
        } else {
            None
        };

        let mut channel = Channel::<I, D, T>::new(
            new_index as usize,
            buffer_in,
            buffer_out,
            endpoint.max_packet_size,
            endpoint.max_packet_size,
        );

        channel.retarget_channel(addr, endpoint, pre)?;
        Ok(channel)
    }

    async fn bus_reset(&self) {
        let regs = I::regs();

        trace!("Bus reset");
        // Set bus in reset state
        regs.cntr().modify(|w| {
            w.set_fres(true);
        });

        // USB Spec says wait 50ms
        Timer::after_millis(50).await;

        // Clear reset state; device will be in default state
        regs.cntr().modify(|w| {
            w.set_fres(false);
        });
    }

    async fn wait_for_device_event(&self) -> embassy_usb_driver::host::DeviceEvent {
        poll_fn(|cx| {
            let istr = I::regs().istr().read();

            BUS_WAKER.register(cx.waker());

            if istr.dcon_stat() {
                let speed = if istr.ls_dcon() { Speed::Low } else { Speed::Full };
                // device has been detected
                return Poll::Ready(DeviceEvent::Connected(speed));
            } else {
                Poll::Pending
            }
            //
            // return Poll::Ready(DeviceEvent::Disconnected);
        })
        .await
    }
}