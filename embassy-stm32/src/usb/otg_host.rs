//
// TODO UsbHost
// that wraps embassy-usb-synopsys-otg UsbHostBus
// and has an interrupt handler
//
// embassy-stm32/src/usb/otg.rs
// has a new_fs_host method that creates it
// and does some of the init stuff in Bus ? maybe

use core::marker::PhantomData;

use embassy_usb_synopsys_otg::host::UsbHostBus;

use super::Instance;
use crate::interrupt;
use crate::interrupt::typelevel::Interrupt;

/// Interrupt handler.        
pub struct UsbHostInterruptHandler<T: Instance> {
    _phantom: PhantomData<T>,
}

impl<T: Instance> interrupt::typelevel::Handler<T::Interrupt> for UsbHostInterruptHandler<T> {
    unsafe fn on_interrupt() {
        /*
        let r = T::regs();
        let state = T::state();
        on_interrupt_impl(r, state, T::ENDPOINT_COUNT);
        */
        // TODO
        trace!("IRQ");
        let regs = T::regs();
        UsbHostBus::on_interrupt_or_poll(regs);
    }
}

pub struct UsbHost {
    pub bus: UsbHostBus,
}
