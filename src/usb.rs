// SPDX-License-Identifier: MIT
//! The USB access the cloner needs. Linux goes through usbfs directly (see
//! linux.rs), so it needs no libusb; the other systems use libusb via rusb.

#[cfg(target_os = "linux")]
pub use crate::linux::UsbDevice as Device;

/// Whether a device with one of these ids is attached.
#[cfg(target_os = "linux")]
pub fn present(ids: &[(u16, u16)]) -> bool {
    crate::linux::find_usb(ids).is_some()
}

#[cfg(not(target_os = "linux"))]
pub use libusb::{present, Device};

#[cfg(not(target_os = "linux"))]
mod libusb {
    use rusb::{DeviceHandle, GlobalContext};
    use std::time::Duration;

    type R<T> = Result<T, String>;

    fn err(e: rusb::Error) -> String {
        e.to_string()
    }

    pub fn present(ids: &[(u16, u16)]) -> bool {
        rusb::devices()
            .map(|l| {
                l.iter().any(|d| d.device_descriptor().map(|x| ids.contains(&(x.vendor_id(), x.product_id()))).unwrap_or(false))
            })
            .unwrap_or(false)
    }

    pub struct Device(DeviceHandle<GlobalContext>);

    impl Device {
        pub fn open(vid: u16, pid: u16) -> R<Self> {
            let h = rusb::open_device_with_vid_pid(vid, pid).ok_or_else(|| err(rusb::Error::NoDevice))?;
            let _ = h.set_auto_detach_kernel_driver(true);
            if h.active_configuration().ok() != Some(1) {
                let _ = h.set_active_configuration(1);
            }
            Ok(Self(h))
        }

        pub fn claim(&self, iface: u8) -> R<()> {
            self.0.claim_interface(iface).map_err(err)
        }

        pub fn release(&self, iface: u8) {
            let _ = self.0.release_interface(iface);
        }

        pub fn control_out(&self, request_type: u8, request: u8, value: u16, index: u16, data: &[u8], timeout: Duration) -> R<usize> {
            self.0.write_control(request_type, request, value, index, data, timeout).map_err(err)
        }

        pub fn control_in(&self, request_type: u8, request: u8, value: u16, index: u16, buf: &mut [u8], timeout: Duration) -> R<usize> {
            self.0.read_control(request_type, request, value, index, buf, timeout).map_err(err)
        }

        pub fn bulk_out(&self, ep: u8, data: &[u8], timeout: Duration) -> R<usize> {
            self.0.write_bulk(ep, data, timeout).map_err(err)
        }

        pub fn bulk_in(&self, ep: u8, buf: &mut [u8], timeout: Duration) -> R<usize> {
            self.0.read_bulk(ep, buf, timeout).map_err(err)
        }
    }
}
