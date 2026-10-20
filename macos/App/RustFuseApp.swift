import AppKit
import RustFuseKit
import ServiceManagement
import SwiftUI

/// A menu bar app: it registers each volume with File Provider (it then
/// shows under Locations in Finder) and keeps a `rustfuse sync` running for
/// it, which the File Provider extension talks to.
@main
struct RustFuseApp: App {
    @NSApplicationDelegateAdaptor private var delegate: AppDelegate
    @StateObject private var store = VolumeStore.shared

    var body: some Scene {
        MenuBarExtra("rustFUSE", systemImage: "externaldrive.connected.to.line.below") {
            MenuContent(store: store)
        }
        Window("rustFUSE Volumes", id: "volumes") {
            VolumesView(store: store)
        }
        .windowResizability(.contentSize)
    }
}

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        VolumeStore.shared.startAll()
    }

    func applicationWillTerminate(_ notification: Notification) {
        VolumeStore.shared.stopAll()
    }
}

struct MenuContent: View {
    @ObservedObject var store: VolumeStore
    @Environment(\.openWindow) private var openWindow
    @State private var atLogin = SMAppService.mainApp.status == .enabled

    var body: some View {
        if store.volumes.isEmpty {
            Text("No volumes")
        }
        ForEach(store.volumes) { v in
            Text("\(v.name): \(store.state(of: v.name))")
        }
        Divider()
        Button("Volumes…") {
            openWindow(id: "volumes")
            NSApp.activate(ignoringOtherApps: true)
        }
        Toggle("Open at Login", isOn: $atLogin)
            .onChange(of: atLogin) { on in
                do {
                    if on {
                        try SMAppService.mainApp.register()
                    } else {
                        try SMAppService.mainApp.unregister()
                    }
                } catch {
                    store.lastError = "Open at Login: \(error.localizedDescription)"
                }
                atLogin = SMAppService.mainApp.status == .enabled
            }
        Divider()
        Button("Quit rustFUSE") { NSApp.terminate(nil) }
    }
}

struct VolumesView: View {
    @ObservedObject var store: VolumeStore
    @State private var name = ""
    @State private var hub = ""
    @State private var token = ""
    @State private var caPath = ""
    @State private var busy = false

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Volumes").font(.headline)
            if store.volumes.isEmpty {
                Text("None yet. Add one below; it appears under Locations in Finder.")
                    .foregroundStyle(.secondary)
            }
            ForEach(store.volumes) { v in
                HStack {
                    VStack(alignment: .leading) {
                        Text(v.name)
                        Text("\(v.hub) · \(store.state(of: v.name))").font(.caption).foregroundStyle(.secondary)
                    }
                    Spacer()
                    Button("Show Log") { store.showLog(v.name) }
                    Button("Remove") { Task { await store.remove(v.name) } }
                }
            }
            Divider()
            Text("Add a volume").font(.headline)
            Form {
                TextField("Volume name", text: $name)
                TextField("Hub (host[:port] or tls://host[:port])", text: $hub)
                SecureField("Hub token", text: $token)
                TextField("Hub CA certificate (PEM path, optional)", text: $caPath)
            }
            HStack {
                Button(busy ? "Adding…" : "Add") {
                    busy = true
                    Task {
                        let ok = await store.add(name: name, hub: hub, token: token, caPath: caPath)
                        busy = false
                        if ok {
                            name = ""
                            hub = ""
                            token = ""
                            caPath = ""
                        }
                    }
                }
                .disabled(busy || name.isEmpty || hub.isEmpty)
                Text("Content downloads when a file is first opened; git data is kept downloaded.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            if let error = store.lastError {
                Text(error).foregroundStyle(.red).textSelection(.enabled)
            }
        }
        .padding()
        .frame(width: 520)
    }
}
