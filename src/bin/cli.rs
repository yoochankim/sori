//! `sori` — remote control for the Sori menu bar app.
//!
//!   sori start [--mic NAME]      start recording
//!   sori stop                    stop, print the folder
//!   sori status                  what is happening right now
//!   sori list [--limit N]        recent recordings
//!   sori devices                 input devices (virtual ones flagged)
//!   sori set-mic <NAME|auto>     pick the microphone (auto = follow system default)
//!   sori quit                    stop recording (if any) and quit the core
//!
//! Add `--json` to any command for machine-readable output.
//! Exit codes: 0 ok · 1 command failed · 2 app not running · 64 usage.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use sori_app::{AppState, Request, Response};

fn usage() -> ! {
    eprintln!(
        "usage: sori <start [--mic NAME] | stop | status | list [--limit N] | devices | set-mic <NAME|auto> | quit> [--json]"
    );
    std::process::exit(64);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        usage();
    }
    let json = args.iter().any(|a| a == "--json");
    let flag_value = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };

    let req = match args[0].as_str() {
        "start" => Request::Start {
            mic: flag_value("--mic"),
        },
        "stop" => Request::Stop,
        "status" => Request::Status,
        "list" => Request::List {
            limit: flag_value("--limit")
                .and_then(|v| v.parse().ok())
                .unwrap_or(5),
        },
        "devices" => Request::Devices,
        "set-mic" => {
            let name = args.get(1).cloned().unwrap_or_else(|| usage());
            Request::SetMic {
                mic: if name == "auto" { None } else { Some(name) },
            }
        }
        "quit" => Request::Quit,
        _ => usage(),
    };

    let resp = match send(&req) {
        Ok(r) => r,
        Err(_) => match offline(&req) {
            Some(r) => r,
            None => {
                let r = Response::err("Sori app is not running");
                print(&r, json, &req);
                std::process::exit(2);
            }
        },
    };

    print(&resp, json, &req);
    std::process::exit(if resp.ok { 0 } else { 1 });
}

fn send(req: &Request) -> std::io::Result<Response> {
    let mut stream = UnixStream::connect(sori_app::socket_path())?;
    let timeout = match req {
        Request::Start { .. } | Request::Stop | Request::Quit => 40,
        _ => 3,
    };
    stream.set_read_timeout(Some(std::time::Duration::from_secs(timeout)))?;
    let mut line = serde_json::to_string(req).unwrap();
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    reader.read_line(&mut buf)?;
    serde_json::from_str(&buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Read-only commands still work when the app is not running.
fn offline(req: &Request) -> Option<Response> {
    match req {
        Request::Status => {
            // Offline: report what state.json says, but make it unmistakable that no core answered.
            let mut v =
                serde_json::to_value(AppState::load().unwrap_or_else(|| AppState::idle("", "")))
                    .unwrap_or_default();
            if let Some(o) = v.as_object_mut() {
                o.insert("core_running".into(), serde_json::Value::Bool(false));
            }
            Some(Response::ok(v))
        }
        Request::List { limit } => Some(Response::ok(sori_app::list_recordings(*limit))),
        Request::Devices => Some(Response::ok(sori_app::devices::list_inputs())),
        _ => None,
    }
}

fn print(resp: &Response, json: bool, req: &Request) {
    if json {
        println!("{}", serde_json::to_string(resp).unwrap());
        return;
    }
    if !resp.ok {
        eprintln!("error: {}", resp.error.clone().unwrap_or_default());
        return;
    }
    let data = resp.data.clone().unwrap_or(serde_json::Value::Null);
    match req {
        Request::Start { .. } => {
            println!("recording → {}", data["folder"].as_str().unwrap_or("?"));
        }
        Request::Stop => {
            println!("{}", data["folder"].as_str().unwrap_or("?"));
        }
        Request::Status => {
            let status = data["status"].as_str().unwrap_or("?");
            if data["core_running"].as_bool() == Some(false) {
                println!("offline  (last state: {status})");
                return;
            }
            if status == "recording" {
                println!(
                    "recording  {}  ({})",
                    sori_app::format_elapsed(data["elapsed_sec"].as_u64().unwrap_or(0)),
                    data["folder"].as_str().unwrap_or("?")
                );
            } else {
                println!("{status}");
            }
            println!(
                "mic:     {}{}",
                data["mic"]["device"].as_str().unwrap_or("?"),
                if data["mic"]["level_ok"].as_bool() == Some(false) {
                    "  [SILENT]"
                } else {
                    ""
                }
            );
            println!(
                "system:  {}",
                data["system"]["device"].as_str().unwrap_or("?")
            );
        }
        Request::List { .. } => {
            if let Some(items) = data.as_array() {
                if items.is_empty() {
                    println!("no recordings yet");
                }
                for it in items {
                    let started = it["started_at"].as_str().unwrap_or("?");
                    let dur = it["duration_sec"].as_u64().unwrap_or(0);
                    let status = it["status"].as_str().unwrap_or("?");
                    println!(
                        "{:<25} {:>8}  {:<9} {}",
                        &started[..started.len().min(16)],
                        sori_app::format_minutes(dur),
                        status,
                        it["folder"].as_str().unwrap_or("?")
                    );
                }
            }
        }
        Request::SetMic { .. } => {
            println!(
                "mic → {}{}",
                data["mic"].as_str().unwrap_or("?"),
                if data["automatic"].as_bool() == Some(true) {
                    "  (automatic)"
                } else {
                    ""
                }
            );
        }
        Request::Quit => println!("quitting"),
        Request::Devices => {
            if let Some(items) = data.as_array() {
                for it in items {
                    println!(
                        "{} {}{}",
                        if it["is_default"].as_bool() == Some(true) {
                            "*"
                        } else {
                            " "
                        },
                        it["name"].as_str().unwrap_or("?"),
                        if it["is_virtual"].as_bool() == Some(true) {
                            "   (virtual — skipped in automatic mode)"
                        } else {
                            ""
                        }
                    );
                }
            }
        }
    }
}
