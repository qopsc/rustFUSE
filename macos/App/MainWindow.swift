import AppKit
import RustFuseKit
import SwiftUI

/// What the sidebar can show.
enum SidebarItem: Hashable {
    case volume(String)
    case connection(UUID)
    /// A volume of a connection, browsed in the bucket.
    case bucket(UUID, String)
}

/// The sheets the main window opens.
enum Sheet: Identifiable {
    case addVolume(connection: UUID?, name: String?)
    case newVolume
    case connection(Connection?)

    var id: String {
        switch self {
        case .addVolume(let c, let n): return "add-\(c?.uuidString ?? "")-\(n ?? "")"
        case .newVolume: return "new"
        case .connection(let c): return "conn-\(c?.id.uuidString ?? "new")"
        }
    }
}

struct MainWindow: View {
    @ObservedObject var model: AppModel
    @State private var selection: SidebarItem?
    @State private var sheet: Sheet?
    /// The view a volume's page opens on.
    private let tab: VolumeTab

    init(model: AppModel, selection: SidebarItem? = nil, tab: VolumeTab = .overview) {
        self.model = model
        self.tab = tab
        _selection = State(initialValue: selection)
    }

    var body: some View {
        NavigationSplitView {
            List(selection: $selection) {
                Section("Volumes") {
                    ForEach(model.library.volumes) { v in
                        Label(v.key, systemImage: icon(v.mode)).tag(SidebarItem.volume(v.key))
                    }
                }
                Section("Connections") {
                    ForEach(model.library.connections) { c in
                        Label(c.name, systemImage: "server.rack").tag(SidebarItem.connection(c.id))
                    }
                }
            }
            .navigationSplitViewColumnWidth(min: 180, ideal: 210)
            .toolbar {
                ToolbarItem {
                    Menu {
                        Button("Add Volume…") { sheet = .addVolume(connection: nil, name: nil) }
                            .disabled(model.library.connections.isEmpty)
                        Button("New Volume from a Folder…") { sheet = .newVolume }
                            .disabled(model.library.connections.isEmpty)
                        Divider()
                        Button("Add Connection…") { sheet = .connection(nil) }
                    } label: {
                        Label("Add", systemImage: "plus")
                    }
                }
            }
        } detail: {
            detail
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        }
        .sheet(item: $sheet) { s in
            switch s {
            case .addVolume(let c, let n):
                AddVolumeSheet(model: model, connectionID: c ?? model.library.connections.first?.id, preset: n)
            case .newVolume:
                NewVolumeSheet(model: model)
            case .connection(let c):
                ConnectionSheet(model: model, editing: c)
            }
        }
        .onAppear {
            // In the Dock while the window is open.
            NSApp.setActivationPolicy(.regular)
            if selection == nil {
                selection = model.library.volumes.first.map { .volume($0.key) }
                    ?? model.library.connections.first.map { .connection($0.id) }
            }
        }
        .onDisappear { NSApp.setActivationPolicy(.accessory) }
    }

    @ViewBuilder private var detail: some View {
        switch selection {
        case .volume(let key):
            if let v = model.library.volume(key) {
                VolumePage(model: model, volume: v, tab: tab)
            } else {
                empty
            }
        case .connection(let id):
            if let c = model.library.connection(id) {
                ConnectionDetail(model: model, connection: c) { name in
                    sheet = .addVolume(connection: id, name: name)
                } browse: { name in
                    selection = .bucket(id, name)
                } edit: {
                    sheet = .connection(c)
                }
            } else {
                empty
            }
        case .bucket(let id, let name):
            if let c = model.library.connection(id) {
                RemoteBrowser(model: model, connection: c, volume: name)
                    .id("\(id)-\(name)")
                    .navigationTitle("\(name) in \(c.name)")
            } else {
                empty
            }
        case nil:
            empty
        }
    }

    private var empty: some View {
        VStack(spacing: 12) {
            if model.library.connections.isEmpty {
                Text("Start by adding a connection: your hub's address and token.")
                Button("Add Connection…") { sheet = .connection(nil) }
            } else {
                Text("Choose a volume or connection, or add a volume.")
                Button("Add Volume…") { sheet = .addVolume(connection: nil, name: nil) }
            }
        }
        .foregroundStyle(.secondary)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

func icon(_ mode: Mode) -> String {
    switch mode {
    case .finder: return "icloud"
    case .folder: return "folder"
    case .mount: return "externaldrive"
    }
}

func describe(_ mode: Mode) -> String {
    switch mode {
    case .finder: return "In Finder (downloads on open)"
    case .folder: return "Folder (full copy)"
    case .mount: return "Mount (downloads on open)"
    }
}

func bytes(_ n: UInt64) -> String {
    ByteCountFormatter.string(fromByteCount: Int64(clamping: n), countStyle: .file)
}

let gigabyte: UInt64 = 1 << 30

/// An error line under a form, selectable so it can be copied.
struct ErrorText: View {
    let text: String?

    var body: some View {
        if let text {
            Text(text).foregroundStyle(.red).textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
        }
    }
}

/// Asks for a folder.
@MainActor
func chooseFolder(prompt: String, canCreate: Bool = true) -> String? {
    let panel = NSOpenPanel()
    panel.canChooseDirectories = true
    panel.canChooseFiles = false
    panel.canCreateDirectories = canCreate
    panel.prompt = prompt
    return panel.runModal() == .OK ? panel.url?.path : nil
}

/// Asks for a file.
@MainActor
func chooseFile(prompt: String) -> String? {
    let panel = NSOpenPanel()
    panel.canChooseDirectories = false
    panel.canChooseFiles = true
    panel.prompt = prompt
    return panel.runModal() == .OK ? panel.url?.path : nil
}

// Liquid Glass on macOS 26 and later, used sparingly: the system already
// gives the sidebar, toolbar and controls their glass. Earlier versions
// keep the plain look.
extension View {
    /// A bar along the bottom of a browser: floating glass, or a plain bar
    /// under a divider.
    @ViewBuilder func bottomBar() -> some View {
        let bar = padding(.horizontal, 12).padding(.vertical, 8)
        if #available(macOS 26, *) {
            bar.glassEffect(.regular, in: .rect(cornerRadius: 12)).padding(8)
        } else {
            VStack(spacing: 0) {
                Divider()
                bar
            }
        }
    }

    /// A summary card, such as a volume's disk use.
    @ViewBuilder func glassCard() -> some View {
        if #available(macOS 26, *) {
            padding(12).glassEffect(.regular, in: .rect(cornerRadius: 14))
        } else {
            self
        }
    }

    /// The one main action of a sheet or bar.
    @ViewBuilder func mainButton() -> some View {
        if #available(macOS 26, *) {
            buttonStyle(.glassProminent)
        } else {
            self
        }
    }
}
