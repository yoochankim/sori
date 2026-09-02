// Sori — native menu bar shell (SwiftUI, MenuBarExtra .window style).
//
// Thin client: all recording lives in `sori-core` (Rust), which this app spawns
// and talks to through `sori-cli --json` (unix-socket IPC) and `~/Sori/state.json`.
// Same idiom as Port Menu: header + rows, settings behind "…", system material.

import AppKit
import Carbon
import ServiceManagement
import SwiftUI
import UserNotifications

// MARK: - Model

struct CoreState: Decodable {
    struct Mic: Decodable { var device: String; var level_ok: Bool; var level: Float? }
    struct Sys: Decodable { var device: String; var level: Float? }
    var status: String
    var folder: String?
    var elapsed_sec: Int
    var mic: Mic
    var system: Sys
    var last_error: String?

    static let idle = CoreState(status: "idle", folder: nil, elapsed_sec: 0,
                                mic: .init(device: "—", level_ok: true, level: 0),
                                system: .init(device: "—", level: 0), last_error: nil)
    var recording: Bool { status == "recording" }
}

struct InputDevice: Decodable, Identifiable {
    var name: String; var is_default: Bool; var is_virtual: Bool
    var id: String { name }
}

struct Recording: Decodable, Identifiable {
    var folder: String; var started_at: String; var duration_sec: Int; var status: String
    var id: String { folder }
    var when: String {
        let iso = ISO8601DateFormatter(); iso.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        let d = iso.date(from: started_at) ?? ISO8601DateFormatter().date(from: started_at) ?? Date()
        let f = DateFormatter(); f.dateFormat = "MMM d, HH:mm"; return f.string(from: d)
    }
    var duration: String { duration_sec < 60 ? "\(duration_sec) sec" : "\(duration_sec / 60) min" }
}

struct CliResponse: Decodable { var ok: Bool; var error: String? }
struct CoreProbe: Decodable { var core_running: Bool? }
struct CliEnvelope<T: Decodable>: Decodable { var ok: Bool; var data: T? }

// MARK: - Core process + CLI bridge

@Observable
final class Core {
    static let shared = Core()

    var state = CoreState.idle
    var devices: [InputDevice] = []
    var micOverride: String? = nil
    var recent: [Recording] = []
    var notice: String? = nil
    var cliInstalledAt: String? = nil
    var commandInFlight = false

    private var process: Process?
    private var timer: Timer?
    private(set) var popoverOpen = false

    private var bundleBin: URL { Bundle.main.bundleURL.appendingPathComponent("Contents/MacOS") }
    private var coreURL: URL { bundleBin.appendingPathComponent("sori-core") }
    private var cliURL: URL { bundleBin.appendingPathComponent("sori-cli") }
    private var home: URL { FileManager.default.homeDirectoryForCurrentUser }
    var recordingsDir: URL { home.appendingPathComponent("Sori/recordings") }
    var logURL: URL { home.appendingPathComponent("Sori/sori.log") }
    private var stateURL: URL { home.appendingPathComponent("Sori/state.json") }
    private var settingsURL: URL { home.appendingPathComponent("Sori/settings.json") }

    // --- lifecycle ---------------------------------------------------------

    func start() {
        ensureCoreAsync()
        refreshCliInstalled()
        schedule(interval: 1.0)
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.8) { self.refreshAll() }
    }

    func setPopover(open: Bool) {
        popoverOpen = open
        schedule(interval: open ? 0.25 : 1.0)
        if open {
            ensureCoreAsync()
            refreshAll()
        }
    }

    private var noticeClear: DispatchWorkItem?
    /// Transient notice that disappears on its own.
    func flash(_ text: String, seconds: Double = 2.5) {
        notice = text
        noticeClear?.cancel()
        let w = DispatchWorkItem { [weak self] in if self?.notice == text { self?.notice = nil } }
        noticeClear = w
        DispatchQueue.main.asyncAfter(deadline: .now() + seconds, execute: w)
    }

    private func schedule(interval: TimeInterval) {
        timer?.invalidate()
        timer = Timer.scheduledTimer(withTimeInterval: interval, repeats: true) { [weak self] _ in self?.pollState() }
        timer?.tolerance = interval / 4
    }

    /// True only when a core process actually answered on the socket
    /// (the CLI falls back to state.json when offline, so `ok` alone is not enough).
    private func coreAlive() -> Bool {
        let probe: CoreProbe? = cliData(["status", "--json"])
        return probe?.core_running == true
    }

    private func ensureCoreAsync() {
        DispatchQueue.global(qos: .utility).async { [weak self] in
            guard let self, !self.coreAlive() else { return }
            DispatchQueue.main.async { self.spawnCore() }
        }
    }

    private func spawnCore() {
        if let p = process, p.isRunning { return }   // spawned, still booting
        let p = Process()
        p.executableURL = coreURL
        p.arguments = ["--headless"]
        p.standardOutput = FileHandle.nullDevice
        p.standardError = FileHandle.nullDevice
        do { try p.run(); process = p } catch { notice = "Could not start recorder core: \(error.localizedDescription)" }
    }

    func shutdown() {
        guard let p = process, p.isRunning else { return }
        _ = runCli(["quit"])
        p.waitUntilExit()
    }

    // --- polling -------------------------------------------------------------

    private var micWarnedThisRecording = false
    private var lastErrorSeen: String? = nil

    private func pollState() {
        guard let data = try? Data(contentsOf: stateURL),
              let s = try? JSONDecoder().decode(CoreState.self, from: data) else { return }
        let wasRecording = state.recording
        state = s

        if !wasRecording && s.recording { micWarnedThisRecording = false }

        if wasRecording && !s.recording {
            refreshRecent()
            if let r = recent.first {
                Notifier.post(
                    id: "saved-\(r.folder)",
                    title: "Recording saved",
                    body: "\(r.duration) · \(r.when)  —  click to show in Finder",
                    folder: r.folder)
            }
        }
        if s.recording, !s.mic.level_ok, s.elapsed_sec >= 8, !micWarnedThisRecording {
            micWarnedThisRecording = true
            Notifier.post(id: "mic-silent", title: "Microphone is silent",
                          body: "No signal from \(s.mic.device). Check the input device or permission.", folder: nil)
        }
        if let e = s.last_error, e != lastErrorSeen {
            lastErrorSeen = e
            Notifier.post(id: "start-failed", title: "Recording could not start", body: e, folder: nil)
        }
        if s.last_error == nil { lastErrorSeen = nil }
    }

    func refreshAll() {
        pollState()
        refreshDevices()
        refreshRecent()
        refreshMicOverride()
    }

    private func refreshDevices() {
        if let d: [InputDevice] = cliData(["devices", "--json"]) { devices = d }
    }

    private func refreshRecent() {
        if let r: [Recording] = cliData(["list", "--limit", "5", "--json"]) { recent = r }
    }

    private func refreshMicOverride() {
        struct S: Decodable { var mic_override: String? }
        if let d = try? Data(contentsOf: settingsURL), let s = try? JSONDecoder().decode(S.self, from: d) { micOverride = s.mic_override } else { micOverride = nil }
    }

    func refreshCliInstalled() {
        for p in ["/usr/local/bin/sori", home.appendingPathComponent(".local/bin/sori").path] {
            if FileManager.default.fileExists(atPath: p) { cliInstalledAt = p; return }
        }
        cliInstalledAt = nil
    }

    // --- actions -------------------------------------------------------------

    func toggleRecording() {
        guard !commandInFlight else { return }
        commandInFlight = true
        let stopping = state.recording
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            guard let self else { return }
            if !self.coreAlive() {
                DispatchQueue.main.sync { self.spawnCore() }
                usleep(600_000)
            }
            let response = self.runCli(stopping ? ["stop"] : ["start"])
            DispatchQueue.main.async {
                self.commandInFlight = false
                if let response, !response.ok { self.notice = response.error }
                else if response == nil { self.notice = "Recorder core is not responding — see Log" }
                else if !stopping { self.notice = nil }
                self.pollState()
                self.refreshRecent()
            }
        }
    }

    func setMic(_ name: String?) {
        _ = runCli(["set-mic", name ?? "auto"])
        micOverride = name
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { self.pollState() }
    }

    func reveal(_ path: String) { NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)]) }
    func open(_ url: URL) { NSWorkspace.shared.open(url) }

    func copy(_ text: String) {
        NSPasteboard.general.clearContents(); NSPasteboard.general.setString(text, forType: .string)
        flash("Path copied")
    }

    func installCli() {
        let target = cliURL.path
        let link = "/usr/local/bin/sori"
        let fm = FileManager.default
        if (try? fm.createDirectory(atPath: "/usr/local/bin", withIntermediateDirectories: true)) != nil,
           (try? fm.removeItem(atPath: link)) != nil || !fm.fileExists(atPath: link),
           (try? fm.createSymbolicLink(atPath: link, withDestinationPath: target)) != nil {
            notice = "Installed → \(link). Try: sori status"; refreshCliInstalled(); return
        }
        // needs admin
        let encodedTarget = Data(target.utf8).base64EncodedString()
        let shell = "/bin/mkdir -p /usr/local/bin && /bin/ln -sfn \"$(/usr/bin/printf %s '\(encodedTarget)' | /usr/bin/base64 -D)\" /usr/local/bin/sori"
        let script = "do shell script \"\(shell.replacingOccurrences(of: "\"", with: "\\\""))\" with administrator privileges"
        var err: NSDictionary?
        NSAppleScript(source: script)?.executeAndReturnError(&err)
        if err == nil, fm.fileExists(atPath: link) { notice = "Installed → \(link). Try: sori status" }
        else { notice = "Install cancelled or failed" }
        refreshCliInstalled()
    }

    // --- cli plumbing ----------------------------------------------------------

    @discardableResult
    private func runCli(_ args: [String]) -> CliResponse? {
        guard let data = cliRaw(args + (args.contains("--json") ? [] : ["--json"])) else { return nil }
        return try? JSONDecoder().decode(CliResponse.self, from: data)
    }

    private func cliData<T: Decodable>(_ args: [String]) -> T? {
        guard let data = cliRaw(args), let env = try? JSONDecoder().decode(CliEnvelope<T>.self, from: data), env.ok else { return nil }
        return env.data
    }

    private func cliRaw(_ args: [String]) -> Data? {
        let p = Process()
        p.executableURL = cliURL
        p.arguments = args
        let out = Pipe()
        p.standardOutput = out
        p.standardError = FileHandle.nullDevice
        do { try p.run() } catch { return nil }
        let data = out.fileHandleForReading.readDataToEndOfFile()
        p.waitUntilExit()
        return data
    }
}

// MARK: - App

@main
struct SoriApp: App {
    @State private var core = Core.shared
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate

    var body: some Scene {
        MenuBarExtra {
            PanelView().environment(core)
        } label: {
            MenuLabel(state: core.state)
        }
        .menuBarExtraStyle(.window)
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate, UNUserNotificationCenterDelegate {
    private var hotKey: GlobalHotKey?

    func applicationDidFinishLaunching(_ notification: Notification) {
        let center = UNUserNotificationCenter.current()
        center.delegate = self
        center.requestAuthorization(options: [.alert, .sound]) { _, _ in }
        Core.shared.start()
        hotKey = GlobalHotKey()
    }
    func applicationWillTerminate(_ notification: Notification) {
        hotKey = nil
        Core.shared.shutdown()
    }

    // Show banners even though we are technically the foreground app while the popover is open.
    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification,
                                withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void) {
        completionHandler([.banner, .sound])
    }
    // Click → reveal the recording folder.
    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse,
                                withCompletionHandler completionHandler: @escaping () -> Void) {
        if let folder = response.notification.request.content.userInfo["folder"] as? String {
            NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: folder)])
        }
        completionHandler()
    }
}

private func soriHotKeyHandler(
    _ nextHandler: EventHandlerCallRef?,
    _ event: EventRef?,
    _ userData: UnsafeMutableRawPointer?
) -> OSStatus {
    guard let event else { return OSStatus(eventNotHandledErr) }
    var hotKeyID = EventHotKeyID()
    let status = GetEventParameter(
        event,
        EventParamName(kEventParamDirectObject),
        EventParamType(typeEventHotKeyID),
        nil,
        MemoryLayout<EventHotKeyID>.size,
        nil,
        &hotKeyID
    )
    guard status == noErr, hotKeyID.signature == 0x536F7269, hotKeyID.id == 1 else {
        return OSStatus(eventNotHandledErr)
    }
    DispatchQueue.main.async { Core.shared.toggleRecording() }
    return noErr
}

final class GlobalHotKey {
    private var hotKeyRef: EventHotKeyRef?
    private var handlerRef: EventHandlerRef?

    init?() {
        var eventType = EventTypeSpec(
            eventClass: OSType(kEventClassKeyboard),
            eventKind: UInt32(kEventHotKeyPressed)
        )
        guard InstallEventHandler(
            GetApplicationEventTarget(),
            soriHotKeyHandler,
            1,
            &eventType,
            nil,
            &handlerRef
        ) == noErr else { return nil }

        let hotKeyID = EventHotKeyID(signature: 0x536F7269, id: 1)
        guard RegisterEventHotKey(
            UInt32(kVK_ANSI_R),
            UInt32(controlKey | shiftKey),
            hotKeyID,
            GetApplicationEventTarget(),
            0,
            &hotKeyRef
        ) == noErr else {
            if let handlerRef { RemoveEventHandler(handlerRef) }
            return nil
        }
    }

    deinit {
        if let hotKeyRef { UnregisterEventHotKey(hotKeyRef) }
        if let handlerRef { RemoveEventHandler(handlerRef) }
    }
}

enum Notifier {
    static func post(id: String, title: String, body: String, folder: String?) {
        let c = UNMutableNotificationContent()
        c.title = title
        c.body = body
        c.sound = .default
        if let f = folder { c.userInfo = ["folder": f] }
        let req = UNNotificationRequest(identifier: id, content: c, trigger: nil)
        UNUserNotificationCenter.current().add(req) { _ in }
    }
}

struct MenuLabel: View {
    let state: CoreState
    var body: some View {
        HStack(spacing: 4) {
            Image(systemName: state.recording ? "record.circle.fill" : "record.circle")
                .font(.system(size: 12, weight: .medium))
                .foregroundStyle(state.recording ? (state.mic.level_ok ? Color.red : Color.yellow) : Color.primary)
            if state.recording {
                Text(elapsed(state.elapsed_sec)).fontDesign(.monospaced).font(.system(size: 12))
            }
        }
    }
}

func elapsed(_ s: Int) -> String {
    s >= 3600 ? String(format: "%d:%02d:%02d", s / 3600, (s % 3600) / 60, s % 60) : String(format: "%d:%02d", s / 60, s % 60)
}

// MARK: - Panel

struct PanelView: View {
    @Environment(Core.self) private var core

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HeaderView()
            Divider()
            RecordRow()
            Divider()
            TracksView()
            Divider()
            RecentView()
            if let n = core.notice ?? core.state.last_error.map({ "Could not start: \($0)" }) {
                Divider()
                Text(n).font(.caption).foregroundStyle(core.state.last_error != nil ? .red : .secondary)
                    .padding(.horizontal, 16).padding(.vertical, 8)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .frame(width: 340)
        .onAppear { core.setPopover(open: true) }
        .onDisappear { core.setPopover(open: false) }
    }
}

// MARK: Header

struct HeaderView: View {
    @Environment(Core.self) private var core
    @State private var showMenu = false
    @State private var loginError: String?

    private var version: String { Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "–" }

    private var launchAtLogin: Binding<Bool> {
        Binding(get: { SMAppService.mainApp.status == .enabled },
                set: { on in
                    do { if on { try SMAppService.mainApp.register() } else { try SMAppService.mainApp.unregister() } }
                    catch { loginError = error.localizedDescription }
                })
    }

    var body: some View {
        HStack(spacing: 10) {
            Text("Sori").font(.headline)
            Spacer()
            HStack(spacing: 2) {
                IconButton(systemName: "folder", help: "Recordings folder") { core.open(core.recordingsDir) }
                IconButton(systemName: "ellipsis", help: "Settings", active: showMenu) { showMenu.toggle() }
                    .popover(isPresented: $showMenu, arrowEdge: .bottom) {
                        VStack(alignment: .leading, spacing: 10) {
                            Text("Sori \(version)").font(.caption).foregroundStyle(.tertiary)
                            Toggle("Launch at Login", isOn: launchAtLogin).toggleStyle(.switch).controlSize(.mini)
                            Divider()
                            HStack {
                                Text("CLI tool").fixedSize()
                                Spacer(minLength: 12)
                                if let p = core.cliInstalledAt {
                                    Text(p).font(.caption).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle).frame(maxWidth: 140)
                                } else {
                                    Button("Install") { core.installCli() }.controlSize(.small)
                                }
                            }
                            Text("Shortcut  ⌃⇧R  start / stop").font(.caption).foregroundStyle(.tertiary)
                            Divider()
                            Button("Open Log") { core.open(core.logURL); showMenu = false }.buttonStyle(.plain)
                            Button("Quit Sori") { NSApplication.shared.terminate(nil) }.buttonStyle(.plain).foregroundStyle(.red)
                        }
                        .padding(12)
                        .frame(width: 270)
                    }
            }
        }
        .padding(.horizontal, 16).padding(.vertical, 10)
        .alert("Couldn't change Launch at Login", isPresented: Binding(get: { loginError != nil }, set: { if !$0 { loginError = nil } })) {
            Button("OK") { loginError = nil }
        } message: { Text(loginError ?? "") }
    }
}

struct IconButton: View {
    let systemName: String
    let help: String
    var active: Bool = false
    let action: () -> Void
    @State private var hover = false
    var body: some View {
        Button(action: action) {
            Image(systemName: systemName)
                .font(.system(size: 11, weight: .medium))
                .frame(width: 22, height: 20)
                .background(RoundedRectangle(cornerRadius: 5).fill(hover || active ? Color.primary.opacity(0.10) : .clear))
                .foregroundStyle(.secondary)
        }
        .buttonStyle(.plain)
        .help(help)
        .onHover { hover = $0 }
    }
}

// MARK: Record row

struct RecordRow: View {
    @Environment(Core.self) private var core
    @State private var hover = false
    var body: some View {
        let rec = core.state.recording
        let busy = core.commandInFlight
        Button { core.toggleRecording() } label: {
            HStack(spacing: 10) {
                Image(systemName: rec ? "stop.fill" : "record.circle")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(rec ? .white : .red)
                    .frame(width: 14)
                Text(busy ? (rec ? "Stopping…" : "Starting…") : (rec ? "Stop Recording" : "Start Recording"))
                    .fontWeight(.medium)
                Spacer()
                Text(rec ? elapsed(core.state.elapsed_sec) : "⌃⇧R")
                    .font(.system(size: 12, design: rec ? .monospaced : .default))
                    .foregroundStyle(rec ? .white.opacity(0.9) : .secondary)
            }
            .padding(.horizontal, 12).padding(.vertical, 8)
            .background(RoundedRectangle(cornerRadius: 8).fill(rec ? Color.red.opacity(hover ? 0.95 : 0.85) : (hover ? Color.primary.opacity(0.08) : .clear)))
            .foregroundStyle(rec ? .white : .primary)
        }
        .buttonStyle(.plain)
        .disabled(busy)
        .onHover { hover = $0 }
        .padding(.horizontal, 8).padding(.vertical, 6)
    }
}

// MARK: Tracks

struct TracksView: View {
    @Environment(Core.self) private var core
    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            // "silent" only after the same 8 s grace the core uses — not in the first seconds of every recording
            TrackRow(label: "Microphone", level: core.state.mic.level ?? 0, bad: core.state.recording && !core.state.mic.level_ok && core.state.elapsed_sec >= 8, recording: core.state.recording) {
                MicPicker()
            }
            TrackRow(label: "System audio", level: core.state.system.level ?? 0, bad: false, recording: core.state.recording) {
                Text(core.state.system.device).font(.system(size: 12)).foregroundStyle(.secondary).lineLimit(1).truncationMode(.tail)
            }
        }
        .padding(.horizontal, 16).padding(.vertical, 10)
    }
}

struct TrackRow<Trailing: View>: View {
    let label: String
    let level: Float
    let bad: Bool
    let recording: Bool
    @ViewBuilder let trailing: () -> Trailing

    private var pct: CGFloat {
        let db = 20 * log10(max(Double(level), 1e-4))
        return CGFloat(min(1, max(0, (db + 60) / 60)))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack(spacing: 8) {
                Text(label).font(.system(size: 12)).foregroundStyle(.secondary).fixedSize().layoutPriority(2)
                Spacer(minLength: 6)
                if bad { Text("silent").font(.system(size: 10, weight: .medium)).foregroundStyle(.yellow).fixedSize().layoutPriority(2) }
                trailing()
            }
            GeometryReader { g in
                ZStack(alignment: .leading) {
                    Capsule().fill(Color.primary.opacity(0.10))
                    Capsule().fill(bad ? Color.yellow : (recording ? Color.green : Color.primary.opacity(0.25)))
                        .frame(width: recording ? max(3, g.size.width * pct) : 0)
                        .animation(.linear(duration: 0.12), value: pct)
                }
            }
            .frame(height: 4)
        }
    }
}

struct MicPicker: View {
    @Environment(Core.self) private var core
    var body: some View {
        Menu {
            Button { core.setMic(nil) } label: {
                Label("Automatic (system default)", systemImage: core.micOverride == nil ? "checkmark" : "")
            }
            Divider()
            ForEach(core.devices) { d in
                Button { core.setMic(d.name) } label: {
                    HStack {
                        Text(d.name + (d.is_virtual ? "  (virtual)" : ""))
                        if core.micOverride == d.name { Image(systemName: "checkmark") }
                    }
                }
            }
        } label: {
            HStack(spacing: 4) {
                Text((core.micOverride == nil ? "Auto · " : "") + core.state.mic.device)
                    .font(.system(size: 12)).lineLimit(1).truncationMode(.middle)
                Image(systemName: "chevron.up.chevron.down").font(.system(size: 8, weight: .semibold))
            }
            .foregroundStyle(.secondary)
        }
        .menuStyle(.borderlessButton)
        .menuIndicator(.hidden)
        .frame(maxWidth: 190, alignment: .trailing)
    }
}

// MARK: Recent

struct RecentView: View {
    @Environment(Core.self) private var core
    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text("Recent").font(.system(size: 11, weight: .semibold)).foregroundStyle(.secondary)
                .padding(.horizontal, 16).padding(.top, 8).padding(.bottom, 4)
            if core.state.recording, let f = core.state.folder {
                RecentRow(dot: .red, title: "Recording now", meta: URL(fileURLWithPath: f).lastPathComponent, path: f)
            } else if core.recent.isEmpty {
                Text("No recordings yet").font(.system(size: 12)).foregroundStyle(.secondary)
                    .padding(.horizontal, 16).padding(.bottom, 8)
            } else {
                ForEach(core.recent) { r in
                    RecentRow(dot: r.status == "done" ? .green : .yellow, title: r.when,
                              meta: r.duration + (r.status == "done" ? "" : " · \(r.status)"), path: r.folder)
                }
            }
        }
        .padding(.bottom, 6)
    }
}

struct RecentRow: View {
    @Environment(Core.self) private var core
    let dot: Color; let title: String; let meta: String; let path: String
    @State private var hover = false
    var body: some View {
        HStack(spacing: 8) {
            Circle().fill(dot).frame(width: 6, height: 6)
            Text(title).font(.system(size: 13))
            Spacer()
            if hover {
                Button("Finder") { core.reveal(path) }.controlSize(.mini)
                Button("Copy path") { core.copy(path) }.controlSize(.mini)
            } else {
                Text(meta).font(.system(size: 11.5)).foregroundStyle(.secondary)
            }
        }
        .padding(.horizontal, 8).padding(.vertical, 5)
        .background(RoundedRectangle(cornerRadius: 7).fill(hover ? Color.primary.opacity(0.07) : .clear))
        .padding(.horizontal, 8)
        .onHover { hover = $0 }
    }
}
