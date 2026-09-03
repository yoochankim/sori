# Contributing to Sori

This file is for people and coding agents that modify Sori. User-facing commands and setup belong in `README.md`.

## Product boundary

Sori is a local macOS menu bar recorder. The SwiftUI shell owns the UI, notifications, login item, and global shortcut. Rust owns audio capture, recording state, WAV output, device changes, and the Unix socket CLI.

The app records microphone and system audio into separate mono WAV files. It does not capture the screen, upload recordings, or require an account.

## Source map

- `macos/SoriMenu.swift`: menu bar shell and macOS integration
- `src/bin/app.rs`: long-running Rust core and IPC coordination
- `src/bin/cli.rs`: `sori` command-line client
- `src/recorder.rs`: recording lifecycle, timeline placement, resampling, and finalization
- `src/audio.rs`: CPAL microphone adapter and system-audio adapter
- `crates/system-audio/`: CoreAudio process-tap capture
- `src/wav.rs`: crash-tolerant PCM WAV writer
- `scripts/bundle.sh`: reproducible `.app` bundle build
- `scripts/generate-third-party.py`: dependency notice generation

## Invariants

- Request only Microphone and System Audio Recording permission. Do not add ScreenCaptureKit, screen-capture APIs, or a screen-capture usage description.
- Keep audio callbacks free of allocation, locks, logging, file IO, blocking calls, and unwinding. Move work to the consumer thread through bounded queues.
- Timestamp microphone and system chunks on the same CoreAudio host clock. Preserve gap insertion, overlap trimming, final drain, and equal-duration track padding.
- Treat a runtime tap-format change as an explicit recording failure unless the new format is safely propagated through the entire pipeline.
- An explicitly selected microphone must fail if unavailable. It must not fall back to another device.
- A successful start response means capture has started. A successful stop response means both WAV files are finalized, synced, and described by a `done` metadata file.
- Keep `~/Sori`, recording folders, and runtime state private to the current account. Directories use mode `0700`; files and the Unix socket use `0600`.
- Never invent third-party copyright text. Notice overrides must be pinned to an exact package version and an exact upstream source revision.
- Do not copy another recorder's implementation. Work from platform documentation, binding contracts, and behavior tests.

## Verification

Run these checks after a code change:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets

cd crates/system-audio
cargo fmt --all -- --check
cargo clippy --offline --all-targets --all-features -- -D warnings
cargo test --offline
cd ../..

xcrun swiftc -parse-as-library -typecheck -warnings-as-errors \
  macos/SoriMenu.swift \
  -framework SwiftUI -framework AppKit -framework Carbon \
  -framework ServiceManagement -framework UserNotifications

cargo audit
./scripts/bundle.sh
codesign --verify --deep --strict target/Sori.app
plutil -lint target/Sori.app/Contents/Info.plist
cmp THIRD_PARTY_NOTICES.txt \
  target/Sori.app/Contents/Resources/THIRD_PARTY_NOTICES.txt
```

Run real recording tests only when the user knows they are happening and the relevant macOS permissions are enabled. Do not start a microphone or system-audio recording without an explicit request. Never upload or share files from `~/Sori/recordings` without separate authorization.

## Repository changes

Preserve unrelated local edits. Keep changes narrow and add a regression test for each bug. Do not change repository visibility, publish a release, push to a new remote, or alter macOS privacy settings without explicit approval.

The ForgeCat package mirrors `AGENTS.md` and the `sori-recorder` skill under `forgecat/`. If either source file changes, update its mirror and verify both pairs with `cmp` before publishing the profile.
