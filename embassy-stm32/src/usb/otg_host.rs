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
use embassy_usb_synopsys_otg::host::UsbHostBus;
use stm32_metapac::common::{Reg, RW};

use crate::usb::{Driver, Instance};
use crate::{interrupt, Peripheral};

/// Interrupt handler.
pub struct USBHostInterruptHandler<I: Instance> {
    _phantom: PhantomData<I>,
}

impl<I: Instance> interrupt::typelevel::Handler<I::Interrupt> for USBHostInterruptHandler<I> {
    unsafe fn on_interrupt() {
        let regs = I::regs();
        //trace!("USB IRQ");
        UsbHostBus::on_interrupt_or_poll(regs);
    }
}

/// USB host driver.
pub struct UsbHost<'d, I: Instance> {
    driver: Driver<'d, I>,
    // TODO probably just modify UsbHostBus to do the stuff in this impl
    pub bus: UsbHostBus,
}

impl<'d, I: Instance> UsbHost<'d, I> {
    /// Create a new USB Host driver.
    pub fn new(driver: Driver<'d, I>) -> Self {
        super::super::common_init::<I>();

        // Enable ULPI clock if external PHY is used
        let phy_type = driver.inner.instance.phy_type;
        let ulpien = !phy_type.internal();

        critical_section::with(|_| {
            let rcc = crate::pac::RCC;
            if I::HIGH_SPEED {
                trace!("Enable HS PHY ULPIEN {}", ulpien);
                rcc.ahb1enr().modify(|w| w.set_usb_otg_hs_ulpien(ulpien));
                rcc.ahb1lpenr().modify(|w| w.set_usb_otg_hs_ulpilpen(ulpien));
            } else {
                rcc.ahb1enr().modify(|w| w.set_usb_otg_fs_ulpien(ulpien));
                rcc.ahb1lpenr().modify(|w| w.set_usb_otg_fs_ulpilpen(ulpien));
            }
        });

        let r = I::regs();
        let core_id = r.cid().read().0;
        trace!("Core id {:08x}", core_id);

        // TODO - I modified UsbHostBus, assumes HS PHY
        let bus = UsbHostBus::new(r);

        Self { driver, bus }
    }
}
