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
use embassy_usb_synopsys_otg::host::UsbHostBus;
use embassy_usb_synopsys_otg::OtgInstance;
use stm32_metapac::common::{Reg, RW};

use crate::usb::{Driver, Instance, MAX_EP_COUNT};
use crate::{interrupt, Peripheral};

/// Interrupt handler.
pub struct UsbHostInterruptHandler<T: Instance> {
    _phantom: PhantomData<T>,
}

impl<T: Instance> interrupt::typelevel::Handler<T::Interrupt> for UsbHostInterruptHandler<T> {
    unsafe fn on_interrupt() {
        let regs = T::regs();
        warn!("USB HOST IRQ");
        UsbHostBus::on_interrupt_or_poll(regs);
    }
}

pub struct UsbHost<'d, T: Instance> {
    phantom: PhantomData<&'d mut T>,
    // TODO don't need all the state in OtgInstance, just for now
    otg: OtgInstance<'d, MAX_EP_COUNT>,
    pub bus: UsbHostBus,
}

impl<'d, T: Instance> UsbHost<'d, T> {
    pub fn new(otg: OtgInstance<'d, MAX_EP_COUNT>) -> Self {
        super::super::common_init::<T>();

        // Enable ULPI clock if external PHY is used
        let phy_type = otg.phy_type;
        let ulpien = !phy_type.internal();

        critical_section::with(|_| {
            let rcc = crate::pac::RCC;
            if T::HIGH_SPEED {
                trace!("Enable HS PHY ULPIEN {}", ulpien);
                rcc.ahb1enr().modify(|w| w.set_usb_otg_hs_ulpien(ulpien));
                rcc.ahb1lpenr().modify(|w| w.set_usb_otg_hs_ulpilpen(ulpien));
            } else {
                rcc.ahb1enr().modify(|w| w.set_usb_otg_fs_ulpien(ulpien));
                rcc.ahb1lpenr().modify(|w| w.set_usb_otg_fs_ulpilpen(ulpien));
            }
        });

        let core_id = otg.regs.cid().read().0;
        trace!("Core id {:08x}", core_id);

        // TODO - I modified UsbHostBus, assumes HS PHY
        let bus = UsbHostBus::new(otg.regs);

        Self {
            phantom: PhantomData,
            otg,
            bus,
        }
    }

    /*
    pub fn poll(&mut self) {
        UsbHostBus::on_interrupt_or_poll(self.otg.regs);
    }
    */
}
