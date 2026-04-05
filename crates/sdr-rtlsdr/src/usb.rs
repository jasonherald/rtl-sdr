//! Low-level USB register access functions.
//!
//! Cast-heavy code is inherent in a faithful hardware register port.

//!
//! Exact port of librtlsdr's register read/write, I2C, and demod register
//! functions. All functions operate on a `rusb::DeviceHandle`.

use std::time::Duration;

use crate::constants::CTRL_TIMEOUT;
use crate::error::RtlSdrError;
use crate::reg::Block;

/// USB control transfer request type: vendor IN.
const CTRL_IN: u8 =
    rusb::constants::LIBUSB_REQUEST_TYPE_VENDOR | rusb::constants::LIBUSB_ENDPOINT_IN;

/// USB control transfer request type: vendor OUT.
const CTRL_OUT: u8 =
    rusb::constants::LIBUSB_REQUEST_TYPE_VENDOR | rusb::constants::LIBUSB_ENDPOINT_OUT;

/// Control transfer timeout duration.
fn ctrl_timeout() -> Duration {
    Duration::from_millis(CTRL_TIMEOUT)
}

/// Read an array of bytes from a register block.
///
/// Ports `rtlsdr_read_array`.
pub fn read_array(
    handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    block: Block,
    addr: u16,
    buf: &mut [u8],
) -> Result<usize, RtlSdrError> {
    let index = (block as u16) << 8;
    let n = handle.read_control(CTRL_IN, 0, addr, index, buf, ctrl_timeout())?;
    Ok(n)
}

/// Write an array of bytes to a register block.
///
/// Ports `rtlsdr_write_array`.
pub fn write_array(
    handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    block: Block,
    addr: u16,
    buf: &[u8],
) -> Result<usize, RtlSdrError> {
    let index = ((block as u16) << 8) | 0x10;
    let n = handle.write_control(CTRL_OUT, 0, addr, index, buf, ctrl_timeout())?;
    Ok(n)
}

/// Read a 16-bit register value.
///
/// Ports `rtlsdr_read_reg`.
pub fn read_reg(
    handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    block: Block,
    addr: u16,
    len: u8,
) -> Result<u16, RtlSdrError> {
    let mut data = [0u8; 2];
    let index = (block as u16) << 8;
    handle.read_control(
        CTRL_IN,
        0,
        addr,
        index,
        &mut data[..len as usize],
        ctrl_timeout(),
    )?;
    Ok(u16::from(data[1]) << 8 | u16::from(data[0]))
}

/// Write a 16-bit register value.
///
/// Ports `rtlsdr_write_reg`.
pub fn write_reg(
    handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    block: Block,
    addr: u16,
    val: u16,
    len: u8,
) -> Result<(), RtlSdrError> {
    let mut data = [0u8; 2];
    let index = ((block as u16) << 8) | 0x10;

    if len == 1 {
        data[0] = (val & 0xff) as u8;
    } else {
        data[0] = (val >> 8) as u8;
    }
    data[1] = (val & 0xff) as u8;

    let r = handle.write_control(
        CTRL_OUT,
        0,
        addr,
        index,
        &data[..len as usize],
        ctrl_timeout(),
    )?;
    if r != len as usize {
        return Err(RtlSdrError::RegisterAccess);
    }
    Ok(())
}

/// Read a demodulator register.
///
/// Ports `rtlsdr_demod_read_reg`.
pub fn demod_read_reg(
    handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    page: u8,
    addr: u16,
    len: u8,
) -> Result<u16, RtlSdrError> {
    let mut data = [0u8; 2];
    let index = u16::from(page);
    let usb_addr = (addr << 8) | 0x20;

    handle.read_control(
        CTRL_IN,
        0,
        usb_addr,
        index,
        &mut data[..len as usize],
        ctrl_timeout(),
    )?;
    Ok(u16::from(data[1]) << 8 | u16::from(data[0]))
}

/// Write a demodulator register.
///
/// Ports `rtlsdr_demod_write_reg`. Includes the dummy read that the
/// C implementation performs after each write.
pub fn demod_write_reg(
    handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    page: u8,
    addr: u16,
    val: u16,
    len: u8,
) -> Result<(), RtlSdrError> {
    let mut data = [0u8; 2];
    let index = 0x10 | u16::from(page);
    let usb_addr = (addr << 8) | 0x20;

    if len == 1 {
        data[0] = (val & 0xff) as u8;
    } else {
        data[0] = (val >> 8) as u8;
    }
    data[1] = (val & 0xff) as u8;

    let r = handle.write_control(
        CTRL_OUT,
        0,
        usb_addr,
        index,
        &data[..len as usize],
        ctrl_timeout(),
    )?;

    // Dummy read after write (matches C implementation)
    let _ = demod_read_reg(handle, 0x0a, 0x01, 1);

    if r != len as usize {
        return Err(RtlSdrError::RegisterAccess);
    }
    Ok(())
}

/// Write a byte to an I2C device register.
///
/// Ports `rtlsdr_i2c_write_reg`.
pub fn i2c_write_reg(
    handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    i2c_addr: u8,
    reg: u8,
    val: u8,
) -> Result<(), RtlSdrError> {
    let data = [reg, val];
    write_array(handle, Block::Iic, u16::from(i2c_addr), &data)?;
    Ok(())
}

/// Read a byte from an I2C device register.
///
/// Ports `rtlsdr_i2c_read_reg`.
pub fn i2c_read_reg(
    handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    i2c_addr: u8,
    reg: u8,
) -> Result<u8, RtlSdrError> {
    write_array(handle, Block::Iic, u16::from(i2c_addr), &[reg])?;
    let mut data = [0u8; 1];
    read_array(handle, Block::Iic, u16::from(i2c_addr), &mut data)?;
    Ok(data[0])
}

/// Write multiple bytes to an I2C device.
///
/// Ports `rtlsdr_i2c_write`.
pub fn i2c_write(
    handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    i2c_addr: u8,
    buf: &[u8],
) -> Result<usize, RtlSdrError> {
    write_array(handle, Block::Iic, u16::from(i2c_addr), buf)
}

/// Read multiple bytes from an I2C device.
///
/// Ports `rtlsdr_i2c_read`.
pub fn i2c_read(
    handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    i2c_addr: u8,
    buf: &mut [u8],
) -> Result<usize, RtlSdrError> {
    read_array(handle, Block::Iic, u16::from(i2c_addr), buf)
}

/// Set a GPIO bit.
///
/// Ports `rtlsdr_set_gpio_bit`.
pub fn set_gpio_bit(
    handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    gpio: u8,
    val: bool,
) -> Result<(), RtlSdrError> {
    let gpio_mask = 1u16 << gpio;
    let r = read_reg(handle, Block::Sys, crate::reg::sys_reg::GPO, 1)?;
    let new_val = if val { r | gpio_mask } else { r & !gpio_mask };
    write_reg(handle, Block::Sys, crate::reg::sys_reg::GPO, new_val, 1)
}

/// Set a GPIO pin as output.
///
/// Ports `rtlsdr_set_gpio_output`.
pub fn set_gpio_output(
    handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    gpio: u8,
) -> Result<(), RtlSdrError> {
    let gpio_mask = 1u16 << gpio;
    let r = read_reg(handle, Block::Sys, crate::reg::sys_reg::GPD, 1)?;
    write_reg(
        handle,
        Block::Sys,
        crate::reg::sys_reg::GPD,
        r & !gpio_mask,
        1,
    )?;
    let r = read_reg(handle, Block::Sys, crate::reg::sys_reg::GPOE, 1)?;
    write_reg(
        handle,
        Block::Sys,
        crate::reg::sys_reg::GPOE,
        r | gpio_mask,
        1,
    )
}

/// Enable/disable the I2C repeater for tuner communication.
///
/// Ports `rtlsdr_set_i2c_repeater`.
pub fn set_i2c_repeater(
    handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    on: bool,
) -> Result<(), RtlSdrError> {
    demod_write_reg(handle, 1, 0x01, if on { 0x18 } else { 0x10 }, 1)
}

/// Set FIR filter coefficients.
///
/// Ports `rtlsdr_set_fir`. Encodes 8 int8 + 8 int12 coefficients
/// into 20 bytes and writes to demod registers.
pub fn set_fir(
    handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    fir: &[i32; crate::constants::FIR_LEN],
) -> Result<(), RtlSdrError> {
    let mut fir_bytes = [0u8; 20];

    // First 8 coefficients: int8
    for i in 0..8 {
        let val = fir[i];
        if val < -128 || val > 127 {
            return Err(RtlSdrError::InvalidParameter(format!(
                "FIR coefficient {i} out of int8 range: {val}"
            )));
        }
        fir_bytes[i] = val as u8;
    }

    // Next 8 coefficients: int12, packed into 12 bytes
    for i in (0..8).step_by(2) {
        let val0 = fir[8 + i];
        let val1 = fir[8 + i + 1];
        if val0 < -2048 || val0 > 2047 || val1 < -2048 || val1 > 2047 {
            return Err(RtlSdrError::InvalidParameter(format!(
                "FIR coefficient {} or {} out of int12 range",
                8 + i,
                8 + i + 1
            )));
        }
        fir_bytes[8 + i * 3 / 2] = (val0 >> 4) as u8;
        fir_bytes[8 + i * 3 / 2 + 1] = ((val0 << 4) | ((val1 >> 8) & 0x0f)) as u8;
        fir_bytes[8 + i * 3 / 2 + 2] = val1 as u8;
    }

    for (i, &byte) in fir_bytes.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        demod_write_reg(handle, 1, 0x1c + i as u16, u16::from(byte), 1)?;
    }

    Ok(())
}

/// Initialize the RTL2832 baseband.
///
/// Ports `rtlsdr_init_baseband`. Sets up USB, powers on demod,
/// resets, configures ADC, FIR, AGC, and Zero-IF mode.
pub fn init_baseband(
    handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    fir: &[i32; crate::constants::FIR_LEN],
) -> Result<(), RtlSdrError> {
    use crate::reg::{sys_reg, usb_reg};

    // Initialize USB
    write_reg(handle, Block::Usb, usb_reg::USB_SYSCTL, 0x09, 1)?;
    write_reg(handle, Block::Usb, usb_reg::USB_EPA_MAXPKT, 0x0002, 2)?;
    write_reg(handle, Block::Usb, usb_reg::USB_EPA_CTL, 0x1002, 2)?;

    // Power on demod
    write_reg(handle, Block::Sys, sys_reg::DEMOD_CTL_1, 0x22, 1)?;
    write_reg(handle, Block::Sys, sys_reg::DEMOD_CTL, 0xe8, 1)?;

    // Reset demod (bit 3, soft_rst)
    demod_write_reg(handle, 1, 0x01, 0x14, 1)?;
    demod_write_reg(handle, 1, 0x01, 0x10, 1)?;

    // Disable spectrum inversion and adjacent channel rejection
    demod_write_reg(handle, 1, 0x15, 0x00, 1)?;
    demod_write_reg(handle, 1, 0x16, 0x0000, 2)?;

    // Clear both DDC shift and IF frequency registers
    for i in 0..6 {
        demod_write_reg(handle, 1, 0x16 + i, 0x00, 1)?;
    }

    // Set FIR coefficients
    set_fir(handle, fir)?;

    // Enable SDR mode, disable DAGC (bit 5)
    demod_write_reg(handle, 0, 0x19, 0x05, 1)?;

    // Init FSM state-holding register
    demod_write_reg(handle, 1, 0x93, 0xf0, 1)?;
    demod_write_reg(handle, 1, 0x94, 0x0f, 1)?;

    // Disable AGC (en_dagc, bit 0)
    demod_write_reg(handle, 1, 0x11, 0x00, 1)?;

    // Disable RF and IF AGC loop
    demod_write_reg(handle, 1, 0x04, 0x00, 1)?;

    // Disable PID filter (enable_PID = 0)
    demod_write_reg(handle, 0, 0x61, 0x60, 1)?;

    // opt_adc_iq = 0, default ADC_I/ADC_Q datapath
    demod_write_reg(handle, 0, 0x06, 0x80, 1)?;

    // Enable Zero-IF mode (en_bbin bit), DC cancellation (en_dc_est),
    // IQ estimation/compensation (en_iq_comp, en_iq_est)
    demod_write_reg(handle, 1, 0xb1, 0x1b, 1)?;

    // Disable 4.096 MHz clock output on pin TP_CK0
    demod_write_reg(handle, 0, 0x0d, 0x83, 1)?;

    Ok(())
}

/// Deinitialize the baseband — power off demod and ADCs.
///
/// Ports `rtlsdr_deinit_baseband`.
pub fn deinit_baseband(
    handle: &rusb::DeviceHandle<rusb::GlobalContext>,
) -> Result<(), RtlSdrError> {
    write_reg(handle, Block::Sys, crate::reg::sys_reg::DEMOD_CTL, 0x20, 1)
}
