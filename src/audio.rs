//! Peak level of the default render endpoint (what the speakers are playing right now).
//! Polled by the UI thread only while the waveform is visible; nothing runs otherwise.

use std::time::{Duration, Instant};
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};

use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED};

#[derive(Default)]
pub struct Meter {
    enumerator: Option<IMMDeviceEnumerator>,
    meter: Option<IAudioMeterInformation>,
    /// After a failed lookup (no playback device) wait before trying again.
    retry_at: Option<Instant>,
}

impl Meter {
    /// Peak sample of the last few ms, 0..1; 0 when there is no usable endpoint.
    pub fn peak(&mut self) -> f32 {
        if self.meter.is_none() && self.retry_at.is_none_or(|t| Instant::now() >= t) {
            self.meter = self.acquire();
            self.retry_at = self.meter.is_none().then(|| Instant::now() + Duration::from_secs(2));
        }
        let Some(m) = &self.meter else { return 0.0 };
        // SAFETY: plain query on a live COM object.
        match unsafe { m.GetPeakValue() } {
            Ok(p) => p.clamp(0.0, 1.0),
            Err(_) => {
                self.meter = None; // device gone: re-acquire on the next poll
                0.0
            }
        }
    }

    /// Forget the endpoint so the next poll looks up the (possibly changed) default device.
    pub fn reset(&mut self) {
        self.meter = None;
    }

    fn acquire(&mut self) -> Option<IAudioMeterInformation> {
        // SAFETY: plain COM calls on the UI thread; initialising an already initialised apartment is harmless.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            if self.enumerator.is_none() {
                self.enumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok();
            }
            let dev = self.enumerator.as_ref()?.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
            dev.Activate::<IAudioMeterInformation>(CLSCTX_ALL, None).ok()
        }
    }
}
