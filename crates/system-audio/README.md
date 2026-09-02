# sori-system-audio-cleanroom
A standalone, clean-room Rust library for capturing the global macOS output mix with documented CoreAudio process taps. It uses no ScreenCaptureKit or other screen-capture API.
## Platform and permission
- macOS 14.4 or newer.
- The containing app must include a non-empty `NSAudioCaptureUsageDescription` in its `Info.plist`.
- Do not add a screen-capture usage description for this library. It requests only audio capture through CoreAudio.
- A bare command-line executable has no embedded `Info.plist`; for distribution, put the executable in an app bundle with the key above or embed a suitable plist during linking.
## Use
```rust,no_run
use sori_system_audio_cleanroom::SystemCapture;
let capture = SystemCapture::open()?;
while let Some(chunk) = capture.pop_chunk() {
    // chunk.samples is mono f32 at capture.sample_rate().
}
capture.stop()?;
# Ok::<(), anyhow::Error>(())
```
`open` is ready when `AudioDeviceStart` returns successfully. It does not wait for a callback, because a silent system must not be reported as a startup failure. Any data-format or timestamp problem observed by the real-time callback is exposed by `take_error`.
The callback uses a fixed 31-block queue with 4,096 mono frames per block. If the consumer falls behind, `take_dropped` reports the exact number of mono frames that could not be queued since the previous call.
