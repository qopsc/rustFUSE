import AppKit
import RustFuseKit
import ServiceManagement
import SwiftUI

/// `rustFUSE --daemon <key>` runs one volume headless for its login agent;
/// otherwise this is the app.
@main
enum Entry {
    static func main() {
        let args = CommandLine.arguments
        if args.count >= 3 && args[1] == "--daemon" {
            runHeadless(key: args[2])
        }
        RustFuseApp.main()
    }
}

/// A menu bar item and a main window: volumes (shown in Finder, as a
/// folder, or as a mount), the connections they come from, and settings.
struct RustFuseApp: App {
    @NSApplicationDelegateAdaptor private var delegate: AppDelegate
    @StateObject private var model = AppModel.shared

    var body: some Scene {
        MenuBarExtra("rustFUSE", systemImage: "externaldrive.connected.to.line.below") {
            MenuContent(model: model)
        }
        Window("rustFUSE", id: "main") {
            MainWindow(model: model)
        }
        .defaultSize(width: 820, height: 540)
        Settings {
            SettingsView(model: model)
        }
    }
}

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        AppModel.shared.launched()
    }

    func applicationWillTerminate(_ notification: Notification) {
        AppModel.shared.quitting()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }
}

struct MenuContent: View {
    @ObservedObject var model: AppModel
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        if model.library.volumes.isEmpty {
            Text("No volumes")
        }
        ForEach(model.library.volumes) { v in
            Button("\(v.key): \(summary(v))") {
                model.showInFinder(v)
            }
        }
        Divider()
        Button("Open rustFUSE…") {
            openWindow(id: "main")
            NSApp.activate()
        }
        SettingsLink {
            Text("Settings…")
        }
        .keyboardShortcut(",")
        Divider()
        Button(model.settings.onQuit == .keepSyncing ? "Quit (volumes keep syncing)" : "Quit rustFUSE") {
            NSApp.terminate(nil)
        }
        .keyboardShortcut("q")
    }

    private func summary(_ v: VolumeConfig) -> String {
        let state = model.state(of: v.key)
        guard let u = model.usage[v.key] else { return state }
        if u.lazy, let limit = u.limit {
            return "\(state), cache \(bytes(u.usage.cached)) of \(bytes(limit))"
        }
        return "\(state), \(bytes(u.usage.local))"
    }
}

struct SettingsView: View {
    @ObservedObject var model: AppModel
    @State private var atLogin = SMAppService.mainApp.status == .enabled
    @State private var cacheGB: Double = 20

    var body: some View {
        Form {
            Section {
                Picker(
                    "When rustFUSE quits",
                    selection: Binding(get: { model.settings.onQuit }, set: { v in model.update { $0.onQuit = v } })
                ) {
                    Text("Stop syncing").tag(OnQuit.stop)
                    Text("Keep syncing in the background").tag(OnQuit.keepSyncing)
                }
                Text(
                    model.settings.onQuit == .stop
                        ? "Volumes sync while the app runs. Quitting stops them and unmounts mounts."
                        : "Each volume runs as a login agent: it keeps syncing after you quit and starts when you log in."
                )
                .font(.caption).foregroundStyle(.secondary)
                Toggle("Open rustFUSE at login", isOn: $atLogin)
                    .onChange(of: atLogin) { _, on in setOpenAtLogin(on) }
            }
            Section {
                HStack {
                    TextField(
                        "Cache size for new volumes", value: $cacheGB, format: .number.precision(.fractionLength(0...1)))
                    Text("GB")
                }
                .onSubmit {
                    model.update { $0.defaultCacheSize = UInt64(max(cacheGB, 0.1) * Double(gigabyte)) }
                }
            } footer: {
                Text("For volumes shown in Finder or as a mount; each volume's own size is set in its page.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section {
                Picker(
                    "Mount with",
                    selection: Binding(
                        get: { model.settings.mountBackend }, set: { v in model.update { $0.mountBackend = v } })
                ) {
                    Text("FSKit (no kernel extension)").tag(MountBackend.fskit)
                    Text("macFUSE kernel extension").tag(MountBackend.kernel)
                }
                if macfuseAvailable() {
                    Text("macFUSE is installed.").font(.caption).foregroundStyle(.secondary)
                } else {
                    HStack {
                        Text("Mounting needs macFUSE.").font(.caption).foregroundStyle(.secondary)
                        Link("Install macFUSE", destination: URL(string: "https://macfuse.github.io")!).font(.caption)
                    }
                }
            } footer: {
                Text("FSKit mounts belong under /Volumes. Changing this restarts mounted volumes.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            ErrorText(text: model.lastError)
        }
        .formStyle(.grouped)
        .frame(width: 480)
        .onAppear { cacheGB = Double(model.settings.defaultCacheSize) / Double(gigabyte) }
        .onDisappear {
            model.update { $0.defaultCacheSize = UInt64(max(cacheGB, 0.1) * Double(gigabyte)) }
        }
    }

    private func setOpenAtLogin(_ on: Bool) {
        do {
            if on {
                try SMAppService.mainApp.register()
            } else {
                try SMAppService.mainApp.unregister()
            }
        } catch {
            model.lastError = "Open at login: \(error.localizedDescription)"
        }
        atLogin = SMAppService.mainApp.status == .enabled
    }
}
