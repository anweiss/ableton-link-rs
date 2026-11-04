//! Output-sink setup shared by the audio examples.

use rodio::cpal::{
    self,
    traits::{DeviceTrait, HostTrait},
    BufferSize,
};
use rodio::{DeviceSinkBuilder, DeviceSinkError, MixerDeviceSink};

/// Opens the default output device the way `DeviceSinkBuilder::open_default_sink`
/// does (default device first, then any non-null device with config fallback),
/// but leaves the hardware buffer size to the OS.
///
/// rodio 0.23's `DeviceSinkBuilder::from_device` swaps the system-default
/// buffer for a fixed ~50 ms one, which on macOS can also change the
/// device-wide buffer for every other application. rodio 0.21, which these
/// examples were written against, kept the system default.
pub fn open_default_sink() -> Result<MixerDeviceSink, DeviceSinkError> {
    let host = cpal::default_host();
    host.default_output_device()
        .ok_or(DeviceSinkError::NoDevice)
        .and_then(DeviceSinkBuilder::from_device)
        .map(with_system_buffer_size)
        .and_then(|builder| builder.open_stream())
        .or_else(|original_err| {
            let devices = match host.output_devices() {
                Ok(devices) => devices,
                Err(err) => {
                    eprintln!("error getting list of output devices: {err}");
                    return Err(original_err);
                }
            };
            devices
                .filter(|dev| {
                    dev.description()
                        .map(|desc| desc.driver().is_some_and(|driver| driver != "null"))
                        .unwrap_or(false)
                })
                .find_map(|dev| {
                    DeviceSinkBuilder::from_device(dev)
                        .map(with_system_buffer_size)
                        .and_then(|builder| builder.open_sink_or_fallback())
                        .ok()
                })
                .ok_or(original_err)
        })
}

fn with_system_buffer_size<E>(builder: DeviceSinkBuilder<E>) -> DeviceSinkBuilder<E>
where
    E: FnMut(cpal::Error) + Send + 'static,
{
    builder.with_buffer_size(BufferSize::Default)
}

#[cfg(test)]
mod tests {
    use super::*;

    // `DeviceSinkBuilder` exposes its config only through `Debug`, and a real
    // device is not available on CI runners.
    fn debug_config(builder: &DeviceSinkBuilder) -> String {
        format!("{builder:?}")
    }

    #[test]
    fn replaces_a_fixed_buffer_with_the_system_default() {
        let fixed = DeviceSinkBuilder::default().with_buffer_size(BufferSize::Fixed(2048));
        assert!(debug_config(&fixed).contains("buffer_size: Fixed(2048)"));

        let builder = with_system_buffer_size(fixed);
        let config = debug_config(&builder);
        assert!(config.contains("buffer_size: Default"), "{config}");
    }
}
