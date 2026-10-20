import AppKit
import RustFuseKit
import SwiftUI

/// One volume: where it is, how much disk it uses against its cache size,
/// and what can be done with it.
struct VolumeDetail: View {
    @ObservedObject var model: AppModel
    let volume: VolumeConfig
    @State private var cacheGB: Double = 20
    @State private var confirmRemove = false

    var body: some View {
        Form {
            Section {
                LabeledContent("Volume", value: volume.name)
                LabeledContent("Connection", value: model.library.connection(volume.connection)?.name ?? "missing")
                LabeledContent("Shown as", value: describe(volume.mode))
                LabeledContent("Location", value: location)
                LabeledContent("Status", value: model.state(of: volume.key))
            }
            Section("Disk use") {
                if let u = model.usage[volume.key] {
                    UsageView(usage: u)
                } else {
                    Text("Waiting for the volume to answer…").foregroundStyle(.secondary)
                }
            }
            if volume.mode.isLazy {
                Section {
                    HStack {
                        TextField("Cache size", value: $cacheGB, format: .number.precision(.fractionLength(0...1)))
                            .frame(width: 80)
                        Text("GB")
                        Spacer()
                        Button("Apply") {
                            model.setCacheSize(volume.key, UInt64(max(cacheGB, 0.1) * Double(gigabyte)))
                        }
                        .disabled(UInt64(max(cacheGB, 0.1) * Double(gigabyte)) == currentCache)
                    }
                } header: {
                    Text("Cache size")
                } footer: {
                    Text(
                        "Downloaded files beyond this are turned back into placeholders, least recently used first. Files you keep downloaded and git data don't count. Applying restarts the volume."
                    )
                    .foregroundStyle(.secondary)
                }
            }
            ConflictsSection(model: model, volume: volume)
            Section {
                HStack {
                    Button("Show in Finder") { model.showInFinder(volume) }
                    Button("Show Log") { model.showLog(volume.key) }
                    Spacer()
                    Button("Remove…", role: .destructive) { confirmRemove = true }
                }
            }
        }
        .formStyle(.grouped)
        .onAppear { cacheGB = Double(currentCache) / Double(gigabyte) }
        .onChange(of: volume.key) { _, _ in cacheGB = Double(currentCache) / Double(gigabyte) }
        .confirmationDialog("Remove \(volume.key)?", isPresented: $confirmRemove) {
            Button("Remove", role: .destructive) {
                Task { await model.remove(volume: volume.key) }
            }
        } message: {
            Text("It stops syncing on this Mac. Its files stay where they are, and the volume stays in the bucket.")
        }
    }

    private var currentCache: UInt64 {
        volume.cacheSize ?? model.settings.defaultCacheSize
    }

    private var location: String {
        switch volume.mode {
        case .finder: return "Finder › Locations › \(finderDomain(volume).displayName)"
        case .folder, .mount: return volume.path ?? "?"
        }
    }
}

/// A volume's disk use: what is on this Mac, split by kind, and the cache
/// against its limit.
struct UsageView: View {
    let usage: VolumeUsage

    var body: some View {
        let u = usage.usage
        VStack(alignment: .leading, spacing: 8) {
            if usage.lazy {
                if let limit = usage.limit {
                    Gauge(value: Double(min(u.cached, limit)), in: 0...Double(max(limit, 1))) {
                        Text("Cache")
                    } currentValueLabel: {
                        Text("\(bytes(u.cached)) of \(bytes(limit))")
                    }
                    .tint(u.cached > limit ? Color.orange : Color.accentColor)
                }
                LabeledContent("On this Mac", value: "\(bytes(u.local)) of \(bytes(u.total))")
                LabeledContent("Git data (always kept)", value: bytes(u.git))
                LabeledContent("Kept downloaded", value: bytes(u.pinned))
                LabeledContent("Cache", value: bytes(u.cached))
            } else {
                LabeledContent("On this Mac", value: bytes(u.local))
            }
            LabeledContent("Files", value: u.files.formatted())
        }
    }
}

/// Adds a volume from a connection: which one, shown how, and where.
struct AddVolumeSheet: View {
    @ObservedObject var model: AppModel
    @State var connectionID: UUID?
    let preset: String?
    @Environment(\.dismiss) private var dismiss
    @State private var names: [String] = []
    @State private var loading = false
    @State private var name = ""
    @State private var mode: Mode = .finder
    @State private var path = ""
    @State private var cacheGB: Double = 20
    @State private var busy = false
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Add a Volume").font(.headline)
            Form {
                Picker("Connection", selection: $connectionID) {
                    ForEach(model.library.connections) { c in
                        Text(c.name).tag(Optional(c.id))
                    }
                }
                Picker("Volume", selection: $name) {
                    if names.isEmpty {
                        Text(loading ? "Loading…" : "None found").tag("")
                    }
                    ForEach(names, id: \.self) { Text($0).tag($0) }
                }
                .disabled(names.isEmpty)
                Picker("Show it", selection: $mode) {
                    Text("In Finder").tag(Mode.finder)
                    Text("As a folder").tag(Mode.folder)
                    Text(macfuseAvailable() ? "As a mount" : "As a mount (needs macFUSE)").tag(Mode.mount)
                }
                .pickerStyle(.radioGroup)
                Text(modeHelp).font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                if mode == .mount && !macfuseAvailable() {
                    Link("Install macFUSE", destination: URL(string: "https://macfuse.github.io")!)
                }
                if mode != .finder {
                    HStack {
                        TextField(mode == .folder ? "Folder" : "Mount point", text: $path)
                        Button("Choose…") {
                            if let parent = chooseFolder(prompt: "Choose") {
                                path = URL(fileURLWithPath: parent).appendingPathComponent(name.isEmpty ? "volume" : name).path
                            }
                        }
                    }
                }
                if mode.isLazy {
                    HStack {
                        TextField("Cache size", value: $cacheGB, format: .number.precision(.fractionLength(0...1)))
                            .frame(width: 80)
                        Text("GB")
                    }
                }
            }
            ErrorText(text: error)
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction)
                Button(busy ? "Adding…" : "Add") { add() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(busy || name.isEmpty || (mode == .mount && !macfuseAvailable()))
            }
        }
        .padding(20)
        .frame(width: 520)
        .task(id: connectionID) { await load() }
        .onChange(of: name) { _, _ in suggestPath() }
        .onChange(of: mode) { _, _ in suggestPath() }
        .onAppear { cacheGB = Double(model.settings.defaultCacheSize) / Double(gigabyte) }
    }

    private var modeHelp: String {
        switch mode {
        case .finder:
            return "Appears in Finder under Locations, like iCloud Drive. Files download when first opened; git data is always kept."
        case .folder:
            return "A normal folder anywhere, holding a full copy of the volume. Best for builds and tools that read everything."
        case .mount:
            return "Mounted with macFUSE anywhere you choose. Files download when first opened; git data is always kept."
        }
    }

    private func suggestPath() {
        guard !name.isEmpty else { return }
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        switch mode {
        case .finder: path = ""
        case .folder: path = "\(home)/rustFUSE/\(name)"
        case .mount:
            path = model.settings.mountBackend == .fskit ? "/Volumes/\(name)" : "\(home)/rustFUSE/\(name)"
        }
    }

    private func load() async {
        guard let c = connectionID.flatMap(model.library.connection) else { return }
        loading = true
        error = nil
        defer { loading = false }
        do {
            names = try await model.volumes(of: c)
            if let preset, names.contains(preset) {
                name = preset
            } else if !names.contains(name) {
                name = names.first ?? ""
            }
        } catch {
            names = []
            name = ""
            self.error = "Listing volumes: \(error.localizedDescription)"
        }
    }

    private func add() {
        guard let c = connectionID.flatMap(model.library.connection) else { return }
        busy = true
        error = nil
        Task {
            do {
                try await model.addVolume(
                    name: name, connection: c, mode: mode, path: mode == .finder ? nil : path,
                    cacheSize: UInt64(max(cacheGB, 0.1) * Double(gigabyte)))
                dismiss()
            } catch {
                self.error = error.localizedDescription
            }
            busy = false
        }
    }
}

/// Creates a new volume from a folder on this Mac, which then syncs in
/// place.
struct NewVolumeSheet: View {
    @ObservedObject var model: AppModel
    @Environment(\.dismiss) private var dismiss
    @State private var connectionID: UUID?
    @State private var name = ""
    @State private var folder = ""
    @State private var busy = false
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("New Volume from a Folder").font(.headline)
            Form {
                Picker("Connection", selection: $connectionID) {
                    ForEach(model.library.connections) { c in
                        Text(c.name).tag(Optional(c.id))
                    }
                }
                HStack {
                    TextField("Folder", text: $folder)
                    Button("Choose…") {
                        if let f = chooseFolder(prompt: "Use This Folder") {
                            folder = f
                            if name.isEmpty { name = URL(fileURLWithPath: f).lastPathComponent }
                        }
                    }
                }
                TextField("Volume name", text: $name)
            }
            Text("Uploads the folder as a new volume in the bucket. The folder stays where it is and keeps syncing.")
                .font(.caption).foregroundStyle(.secondary)
            ErrorText(text: error)
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction)
                Button(busy ? "Uploading…" : "Create") { create() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(busy || name.isEmpty || folder.isEmpty || connectionID == nil)
            }
        }
        .padding(20)
        .frame(width: 480)
        .onAppear { connectionID = model.library.connections.first?.id }
    }

    private func create() {
        guard let c = connectionID.flatMap(model.library.connection) else { return }
        busy = true
        error = nil
        Task {
            do {
                try await model.createVolume(name: name, connection: c, folder: folder)
                dismiss()
            } catch {
                self.error = error.localizedDescription
            }
            busy = false
        }
    }
}
