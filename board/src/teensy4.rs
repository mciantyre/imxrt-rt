//! Teensy4 support.

use crate::ral;

#[cfg(target_arch = "arm")]
use teensy4_fcb as _;
#[cfg(target_arch = "arm")]
use teensy4_panic as _;

const LED_OFFSET: u32 = 3;

pub mod rtic_support {
    pub use crate::ral::*;
}

/// Prepare the board for the examples.
///
/// Call this first. Panics if something went wrong.
pub fn prepare(timer_delay_microseconds: u32) -> Option<crate::Resources> {
    let iomuxc = unsafe { ral::iomuxc::IOMUXC::instance() };
    // Set the GPIO pad to a GPIO function (ALT 5)
    ral::write_reg!(ral::iomuxc, iomuxc, SW_MUX_CTL_PAD_GPIO_B0_03, 5);
    // Increase drive strength, but leave other fields at their current value...
    ral::modify_reg!(
        ral::iomuxc,
        iomuxc,
        SW_PAD_CTL_PAD_GPIO_B0_03,
        DSE: DSE_7_R0_7
    );

    let pit = crate::prepare_pit(timer_delay_microseconds)?;
    let gpio2 = unsafe { ral::gpio::GPIO2::instance() };
    Some(crate::Resources {
        led: crate::Led::new(LED_OFFSET, &gpio2),
        pit,
    })
}

/// Dummy DCD section containing a single NOP command (for testing linker scripts).
#[cfg(feature = "__dcd")]
#[unsafe(link_section = ".dcd")]
#[unsafe(no_mangle)]
#[used]
pub static DEVICE_CONFIGURATION_DATA: [u8; 8] = [0xD2, 0x00, 0x08, 0x41, 0xC0, 0x00, 0x04, 0x00];

/// Ditto but incorrect size (not a multiple of 4 bytes). The linker script should catch this error
/// and fail the build.
#[cfg(feature = "__dcd_missize")]
#[unsafe(link_section = ".dcd")]
#[unsafe(no_mangle)]
#[used]
pub static DEVICE_CONFIGURATION_DATA: [u8; 7] = [0xD2, 0x00, 0x08, 0x41, 0xC0, 0x00, 0x04];
