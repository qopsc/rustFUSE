import AppKit
import RustFuseKit
import SwiftUI

/// What a volume's page shows.
enum VolumeTab: String, CaseIterable {
    case overview = "Overview"
    case files = "Files"
    case bucket = "In the Bucket"
}

/// A volume's page: its overview, its files on this Mac, or the volume as
/// the bucket holds it.
struct VolumePage: View {
    @ObservedObject var model: AppModel
    let volume: VolumeConfig
    @State private var tab: VolumeTab = .overview

    var body: some View {
        Group {
            switch tab {
            case .overview:
                VolumeDetail(model: model, volume: volume)
            case .files:
                LocalBrowser(model: model, volume: volume).id(volume.key)
            case .bucket:
                if let c = model.library.connection(volume.connection) {
                    RemoteBrowser(model: model, connection: c, volume: volume.name).id(volume.key)
                } else {
                    Text("This volume's connection is missing.").foregroundStyle(.secondary)
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                }
            }
        }
        .navigationTitle(volume.key)
        .toolbar {
            ToolbarItem(placement: .principal) {
                Picker("Show", selection: $tab) {
                    ForEach(VolumeTab.allCases, id: \.self) { Text($0.rawValue).tag($0) }
                }
                .pickerStyle(.segmented)
                .fixedSize()
            }
        }
    }
}

/// The folders above the one a browser shows; clicking one goes back to it.
struct PathBar: View {
    let root: String
    let names: [String]
    let go: (Int) -> Void

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 4) {
                Button(root) { go(0) }.buttonStyle(.link)
                ForEach(Array(names.enumerated()), id: \.offset) { i, name in
                    Image(systemName: "chevron.right").font(.caption).foregroundStyle(.tertiary)
                    Button(name) { go(i + 1) }.buttonStyle(.link)
                }
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
        }
    }
}

/// A name with its kind's icon, marked when it is a conflict copy.
struct EntryName: View {
    let name: String
    let kind: Kind

    var body: some View {
        HStack(spacing: 6) {
            Image(systemName: kind == .dir ? "folder" : kind == .symlink ? "arrow.turn.up.right" : "doc")
                .foregroundStyle(kind == .dir ? Color.accentColor : Color.secondary)
                .frame(width: 16)
            Text(name).lineLimit(1).truncationMode(.middle)
            if isConflictCopy(name) {
                Image(systemName: "exclamationmark.triangle.fill")
                    .foregroundStyle(.orange)
                    .help("Conflict copy: the other machine's version of a file both changed.")
            }
        }
    }
}

/// Folders first, then by name as Finder sorts.
func finderOrder(_ a: (name: String, kind: Kind), _ b: (name: String, kind: Kind)) -> Bool {
    if (a.kind == .dir) != (b.kind == .dir) { return a.kind == .dir }
    return a.name.localizedStandardCompare(b.name) == .orderedAscending
}

/// A volume's files on this Mac as its daemon sees them: what is
/// downloaded, kept downloaded or only in the bucket, with "keep
/// downloaded" and "free up space" for volumes that download on open.
struct LocalBrowser: View {
    @ObservedObject var model: AppModel
    let volume: VolumeConfig
    @State private var trail: [Item] = []
    @State private var items: [Item] = []
    @State private var selection = Set<Item.ID>()
    @State private var loaded = false
    @State private var error: String?

    private var folderID: String { trail.last?.id ?? rootID }

    var body: some View {
        VStack(spacing: 0) {
            PathBar(root: volume.name, names: trail.map(\.name)) { depth in
                trail = Array(trail.prefix(depth))
            }
            Divider()
            Table(items, selection: $selection) {
                TableColumn("Name") { (item: Item) in EntryName(name: item.name, kind: item.kind) }
                TableColumn("Size") { (item: Item) in
                    Text(item.kind == .file ? bytes(item.size) : "").foregroundStyle(.secondary)
                }
                .width(min: 60, ideal: 80, max: 120)
                TableColumn("Status") { (item: Item) in
                    let s = status(item)
                    if !s.text.isEmpty {
                        Label(s.text, systemImage: s.symbol).foregroundStyle(.secondary)
                    }
                }
                .width(min: 120, ideal: 160, max: 220)
            }
            .contextMenu(forSelectionType: Item.ID.self) { ids in
                actions(ids)
            } primaryAction: { ids in
                open(ids)
            }
            .overlay {
                if loaded && items.isEmpty && error == nil {
                    Text("Empty folder").foregroundStyle(.secondary)
                }
            }
            HStack {
                if let error {
                    ErrorText(text: error)
                } else {
                    Text(summary).foregroundStyle(.secondary)
                }
                Spacer()
                actions(selection)
            }
            .controlSize(.small)
            .bottomBar()
        }
        // Reloads while shown, so downloads and changes from other machines
        // appear.
        .task(id: folderID) {
            selection = []
            loaded = false
            while !Task.isCancelled {
                await load()
                try? await Task.sleep(for: .seconds(3))
            }
        }
    }

    private var summary: String {
        guard loaded else { return "Loading…" }
        let size = items.filter { $0.kind == .file }.reduce(0) { $0 + $1.size }
        return "\(items.count) items, \(bytes(size))"
    }

    private func status(_ item: Item) -> (text: String, symbol: String) {
        if !item.synced { return ("Only on this Mac", "laptopcomputer") }
        if !volume.mode.isLazy { return ("Synced", "checkmark.circle") }
        if item.kind == .dir { return item.pinned ? ("Kept downloaded", "pin.fill") : ("", "") }
        if item.pinned { return item.downloaded ? ("Kept downloaded", "pin.fill") : ("Downloading…", "arrow.down.circle") }
        return item.downloaded ? ("Downloaded", "checkmark.circle") : ("In the bucket", "icloud")
    }

    @ViewBuilder private func actions(_ ids: Set<Item.ID>) -> some View {
        let chosen = items.filter { ids.contains($0.id) }
        if !chosen.isEmpty {
            Button("Show in Finder") { model.reveal(volume, paths: chosen.map(path)) }
            if volume.mode.isLazy {
                if chosen.allSatisfy(\.pinned) {
                    Button("Stop Keeping Downloaded") { act(ids) { try $0.keep($1, false) } }
                } else {
                    Button("Keep Downloaded") { act(ids) { try $0.keep($1, true) } }
                }
                Button("Free Up Space") { act(ids) { try $0.free($1) } }
            }
        }
    }

    private func path(_ item: Item) -> String {
        (trail.map(\.name) + [item.name]).joined(separator: "/")
    }

    /// Goes into a folder, or opens files with their apps.
    private func open(_ ids: Set<Item.ID>) {
        let chosen = items.filter { ids.contains($0.id) }
        if chosen.count == 1, let dir = chosen.first, dir.kind == .dir {
            trail.append(dir)
            return
        }
        for item in chosen where item.kind != .dir {
            model.open(volume, path: path(item))
        }
    }

    private func client() throws -> Client {
        Client(socketPath: try Shared.socket(volume.key).path)
    }

    private func load() async {
        let id = folderID
        do {
            let c = try client()
            let all = try await Task.detached { () throws -> [Item] in
                defer { c.close() }
                var out: [Item] = []
                var after: String?
                repeat {
                    let page = try c.list(id, after: after)
                    out += page.items
                    after = page.next
                } while after != nil
                return out
            }.value
            guard id == folderID else { return }
            items = all.sorted { finderOrder(($0.name, $0.kind), ($1.name, $1.kind)) }
            selection.formIntersection(items.map(\.id))
            error = nil
        } catch {
            guard id == folderID else { return }
            items = []
            self.error = message(error)
        }
        loaded = true
    }

    private func act(_ ids: Set<Item.ID>, _ f: @escaping @Sendable (Client, String) throws -> Void) {
        Task {
            do {
                let c = try client()
                try await Task.detached {
                    defer { c.close() }
                    for id in ids { try f(c, id) }
                }.value
            } catch {
                self.error = message(error)
            }
            await load()
        }
    }

    private func message(_ error: Error) -> String {
        switch error as? ProviderError {
        case .unreachable(_)?: return "The volume isn't running; see its log."
        case .daemon(_, let m)?: return m
        case .badAnswer(let m)?: return "Unexpected answer: \(m)"
        case nil: return error.localizedDescription
        }
    }
}

/// A volume as the bucket holds it, read through the connection without
/// syncing it: browse folders, download files and folders.
struct RemoteBrowser: View {
    @ObservedObject var model: AppModel
    let connection: Connection
    let volume: String
    @State private var trail: [String] = []
    @State private var listing: RemoteListing?
    @State private var selection = Set<RemoteEntry.ID>()
    @State private var loading = false
    @State private var busy: String?
    @State private var done: (text: String, folder: URL)?
    @State private var error: String?

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                PathBar(root: volume, names: trail) { depth in trail = Array(trail.prefix(depth)) }
                Spacer()
                if loading { ProgressView().controlSize(.small).padding(.trailing, 12) }
            }
            Divider()
            Table(entries, selection: $selection) {
                TableColumn("Name") { (e: RemoteEntry) in EntryName(name: e.name, kind: e.kind) }
                TableColumn("Size") { (e: RemoteEntry) in
                    Text(e.kind == .file ? bytes(e.size) : "").foregroundStyle(.secondary)
                }
                .width(min: 60, ideal: 80, max: 120)
                TableColumn("Modified") { (e: RemoteEntry) in
                    Text(Date(timeIntervalSince1970: Double(e.mtimeNs) / 1e9).formatted(date: .abbreviated, time: .shortened))
                        .foregroundStyle(.secondary)
                }
                .width(min: 120, ideal: 150, max: 200)
            }
            .contextMenu(forSelectionType: RemoteEntry.ID.self) { names in
                if !names.isEmpty {
                    Button("Download…") { download(names) }
                }
            } primaryAction: { names in
                if names.count == 1, let name = names.first, entries.first(where: { $0.name == name })?.kind == .dir {
                    trail.append(name)
                } else {
                    download(names)
                }
            }
            .overlay {
                if listing != nil && entries.isEmpty && error == nil {
                    Text("Empty folder").foregroundStyle(.secondary)
                }
            }
            HStack {
                if let error {
                    ErrorText(text: error)
                } else if let busy {
                    ProgressView().controlSize(.small)
                    Text(busy).foregroundStyle(.secondary)
                } else if let done {
                    Text(done.text).foregroundStyle(.secondary)
                    Button("Show in Finder") { NSWorkspace.shared.open(done.folder) }.buttonStyle(.link)
                } else if let listing {
                    Text("\(listing.entries.count) items, at changeset \(listing.seq) of \(connection.name)")
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Button("Refresh") { Task { await load() } }
                Button("Download…") { download(selection) }
                    .mainButton()
                    .disabled(selection.isEmpty || busy != nil)
            }
            .controlSize(.small)
            .bottomBar()
        }
        .task(id: trail) {
            selection = []
            await load()
        }
    }

    private var entries: [RemoteEntry] {
        (listing?.entries ?? []).sorted { finderOrder(($0.name, $0.kind), ($1.name, $1.kind)) }
    }

    private func load() async {
        let at = trail
        loading = true
        defer { loading = false }
        do {
            let l = try await model.listRemote(connection, volume: volume, path: at.joined(separator: "/"))
            guard at == trail else { return }
            listing = l
            error = nil
        } catch {
            guard at == trail else { return }
            listing = nil
            self.error = error.localizedDescription
        }
    }

    /// Asks where, then downloads each chosen file or folder there; one
    /// already there is left alone.
    private func download(_ names: Set<RemoteEntry.ID>) {
        guard !names.isEmpty, let dest = chooseFolder(prompt: "Download Here") else { return }
        let folder = URL(fileURLWithPath: dest, isDirectory: true)
        let base = trail
        Task {
            error = nil
            done = nil
            var skipped: [String] = []
            var count = 0
            for name in names.sorted() {
                let target = folder.appendingPathComponent(name)
                if FileManager.default.fileExists(atPath: target.path) {
                    skipped.append(name)
                    continue
                }
                busy = "Downloading \(name)…"
                do {
                    try await model.download(connection, volume: volume, path: (base + [name]).joined(separator: "/"), to: target)
                    count += 1
                } catch {
                    self.error = "Downloading \(name): \(error.localizedDescription)"
                    break
                }
            }
            busy = nil
            if error == nil {
                if !skipped.isEmpty {
                    error = "Already in \(folder.lastPathComponent), not downloaded: \(skipped.joined(separator: ", "))"
                }
                if count > 0 {
                    done = ("Downloaded \(count == 1 ? "1 item" : "\(count) items") to \(folder.lastPathComponent).", folder)
                }
            }
        }
    }
}

/// The conflict copies and diverged branches in a volume on this Mac, each
/// with what to do about it.
struct ConflictsSection: View {
    @ObservedObject var model: AppModel
    let volume: VolumeConfig
    @State private var report: ConflictReport?
    @State private var error: String?

    var body: some View {
        Section {
            if let r = report {
                if r.files.isEmpty && r.branches.isEmpty {
                    Text("No conflicts.").foregroundStyle(.secondary)
                }
                ForEach(r.files, id: \.self) { f in
                    HStack {
                        Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
                        Text(f).lineLimit(1).truncationMode(.middle).textSelection(.enabled)
                        Spacer()
                        Button("Show in Finder") { model.reveal(volume, paths: [f]) }
                    }
                }
                ForEach(r.branches, id: \.self) { b in
                    HStack {
                        Image(systemName: "arrow.triangle.branch").foregroundStyle(.orange)
                        Text("\(b.branch) in \(b.repo == "." ? volume.name : b.repo)")
                            .lineLimit(1).truncationMode(.middle).textSelection(.enabled)
                        Spacer()
                        Button("Copy Merge Command") {
                            NSPasteboard.general.clearContents()
                            NSPasteboard.general.setString("git merge \(b.branch)", forType: .string)
                        }
                    }
                }
            } else if error == nil {
                ProgressView().controlSize(.small)
            }
            ErrorText(text: error)
        } header: {
            HStack {
                Text("Conflicts")
                Spacer()
                Button("Check Again") { Task { await load() } }.controlSize(.small)
            }
        } footer: {
            Text(
                "When a file changed on two machines at once, the other machine's version is kept next to it as a conflict copy: compare the two, keep what you want and delete the copy. A branch that moved on two machines is kept as a second branch to merge."
            )
            .foregroundStyle(.secondary)
        }
        .task(id: volume.key) { await load() }
    }

    private func load() async {
        do {
            report = try await model.conflicts(of: volume)
            error = nil
        } catch {
            report = nil
            self.error = error.localizedDescription
        }
    }
}
