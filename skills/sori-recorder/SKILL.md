---
name: sori-recorder
description: Operate the installed Sori macOS recorder through its CLI. Use when the user asks to start or stop recording, check recording status or audio levels, choose a microphone, list recordings, or verify completed Sori files. Do not use for changing Sori source code.
---

# Sori recorder

Use Sori's local CLI and request JSON output when reading results programmatically.

## Boundaries

- A direct request to record is authorization to start. Do not start from calendar context, ambient conversation, or an inferred meeting.
- Uploading, sharing, attaching, or transcribing audio requires a separate user request.
- Do not change macOS privacy permissions, switch microphones, or delete recordings unless the user asks.
- Keep exact error messages. Do not report success before the CLI returns `ok: true`.

## Start and monitor

Check state before starting:

```sh
sori status --json
```

If Sori is already recording, report the active folder instead of starting another recording. Otherwise run:

```sh
sori start --json
```

Keep the returned folder path. While recording, use `sori status --json` for elapsed time, selected devices, and audio levels. If the microphone is silent, report the selected device without changing it.

If start times out, run `sori status --json` before retrying. A late start must not create a second recording.

## Stop and verify

Run:

```sh
sori stop --json
```

Wait for `ok: true`. The stop response means both WAV files were finalized. Read `meta.json` in the returned folder and verify that `status` is `done`, then report the folder and duration.

Do not open or process `mic.wav` or `system.wav` unless the user requested work on the audio.

## Find recordings and choose a microphone

Use:

```sh
sori list --json
sori devices --json
```

Use `sori set-mic <NAME> --json` only after the user chooses that device. Use `sori set-mic auto --json` when the user wants Sori to follow the default physical microphone.

Recordings are under `~/Sori/recordings`. Each folder contains `mic.wav`, `system.wav`, and `meta.json`. If a command fails, check `sori status --json` before another state-changing command.
