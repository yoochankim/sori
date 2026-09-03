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

This repository includes an `AGENTS.md` identity for AI agents that use Sori. The reusable CLI workflow is in [`skills/sori-recorder/SKILL.md`](skills/sori-recorder/SKILL.md).

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

An agent should not start recording without an explicit request. You can request recording and a later action in the same message, but the agent should not infer any action you did not name.

### Continue after recording

Sori produces audio files, not transcripts. Once a recording is finalized, the agent reports its folder and duration. If you have not said what should happen next, it can offer a short choice once: leave the recording untouched, transcribe it, summarize it, or extract decisions and action items.

The follow-up work comes from another skill or tool available to your agent. The Sori profile hands that tool the exact recording folder without granting access to other recordings. If a tool needs to upload audio, the agent should name the service and ask before sending the files unless you already approved it.

Example requests:

> Record this meeting. When I ask you to stop, keep the files local and ask whether I want a transcript or action items.

> Record this meeting. When I ask you to stop, transcribe the completed recording locally and summarize the decisions and action items.

> Show me my recordings and let me choose one. Transcribe only the recording I select.

> Stop the recording and leave the audio untouched.

### ForgeCat profile

The included `forgecat/profile.yml` packages the Sori agent identity and `sori-recorder` skill together. The profile can offer follow-up options and hand a selected recording to another tool, but it does not include a transcription engine. It assumes that the Sori app and CLI are already installed on the Mac.

```sh
forgecat install @yoochankim/sori
```

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
