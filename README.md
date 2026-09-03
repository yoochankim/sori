# Sori

Sori is a local meeting recorder for macOS. It records your microphone and the audio played by your Mac into separate WAV files from a small menu bar app.

There is no account, cloud service, or telemetry. Recordings stay in `~/Sori/recordings` unless you move them yourself or add an `on-finish` hook.

## What it does

- Starts and stops from the menu bar or `Control+Shift+R`
- Records microphone and system audio as separate, time-aligned mono WAV files
- Follows the default microphone and can switch devices during a recording
- Shows live input levels and warns when the microphone stays silent
- Flushes WAV headers every five seconds so interrupted recordings remain readable
- Includes a local CLI and an optional post-recording hook

Sori requires macOS 14.4 or later and currently builds for Apple silicon.

## Build

Install Rust, Python 3, and the Xcode command-line tools, then run:

```sh
./scripts/bundle.sh
open target/Sori.app
```

The first recording asks for Microphone and System Audio Recording permission. Sori captures audio from every app playing sound during the recording. It never requests screen-recording access.

## CLI

Install the command from the menu bar settings, then use:

```sh
sori start
sori stop
sori status
sori devices
sori list
```

Add `--json` to any command for machine-readable output.

## Use Sori with an AI agent

This repository includes an `AGENTS.md` identity and operating guide for AI agents that use Sori.

Sori's CLI can return JSON, so a local AI agent can control recording and verify the result. For example:

> Start a Sori recording. Do not upload or share the audio. When I ask you to stop, wait for finalization and confirm that `meta.json` says `done`.

The agent can use this sequence:

```sh
sori status --json
sori start --json
sori stop --json
sori list --json
```

`sori start` returns the new recording folder. `sori stop` returns only after both WAV files and `meta.json` have been finalized. Recordings stay in `~/Sori/recordings` with permissions limited to the current macOS account.

An agent should not start recording without an explicit request. It should not upload, transcribe, or share a recording unless you ask it to do so.

## Files

Each recording has its own folder:

```text
~/Sori/recordings/2026-09-02-1423/
  mic.wav
  system.wav
  meta.json
```

If `~/Sori/on-finish` exists and is executable, Sori runs it with the completed recording folder as its first argument.

## Architecture

The visible app is a thin SwiftUI `MenuBarExtra` that also owns the global shortcut. A Rust child process handles audio capture, WAV writing, device changes, and the Unix-socket CLI.

Microphone input uses `cpal`. System audio uses a clean-room CoreAudio process-tap crate built from Apple SDK headers and the `objc2` bindings. Capture callbacks place PCM in bounded queues; file IO stays on the recorder thread.

## License

MIT

Dependency license texts are in `THIRD_PARTY_NOTICES.txt` and are included in the app bundle.
