import AppKit
import FileProvider
import RustFuseKit
import os

private let log = Logger(subsystem: "dev.rustfuse", category: "app")

/// A volume's disk use as its daemon last reported it.
struct VolumeUsage: Equatable, Sendable {
    var usage: Usage
    /// The cache size, for a lazy volume.
    var limit: UInt64?
    var lazy: Bool
}

/// Everything the app shows and does: saved connections, the volumes this
/// Mac uses, settings, and the daemons that keep the volumes in sync.
///
/// Daemons run inside the app when quitting stops syncing, or as login
/// agents (`LoginAgents`) when syncing continues after quit.
@MainActor
final class AppModel: ObservableObject {
    static let shared = AppModel()

    @Published private(set) var library = Library()
    @Published private(set) var states: [String: String] = [:]
    @Published private(set) var usage: [String: VolumeUsage] = [:]
    @Published var lastError: String?
    private var daemons: [String: Daemon] = [:]
    private var poller: Timer?
    /// Sample data shown instead of asking daemons and the bucket, for
    /// screenshots (`rustFUSE --screenshots`).
    private(set) var demo: Demo?

    /// What a demo model shows: folders of items by id, bucket folders by
    /// path, conflicts and the volumes a connection opens.
    struct Demo {
        var items: [String: [Item]]
        var bucket: [String: RemoteListing]
        var conflicts: ConflictReport
        var volumes: [String]
    }

    /// A model showing `library` and `demo`; it runs nothing and saves
    /// nothing.
    init(library: Library, usage: [String: VolumeUsage], states: [String: String], demo: Demo) {
        self.library = library
        self.usage = usage
        self.states = states
        self.demo = demo
    }

    init() {
        do {
            let file = try Shared.libraryFile()
            let fresh = !FileManager.default.fileExists(atPath: file.path)
            library = try Shared.loadLibrary()
            if fresh && !library.volumes.isEmpty {
                // Migrated from the first version: its tokens were per volume.
                for v in library.volumes {
                    Secrets.migrateLegacyToken(volume: v.key, to: v.connection)
                }
                try Shared.saveLibrary(library)
            }
        } catch {
            lastError = "Reading the app's settings: \(error.localizedDescription)"
        }
    }

    var settings: AppSettings { library.settings }

    // MARK: Running

    /// Starts every volume the way the settings say, and keeps their disk
    /// use up to date.
    func launched() {
        switch settings.onQuit {
        case .keepSyncing:
            for v in library.volumes where !LoginAgents.installed(v.key) {
                startAgent(v)
            }
        case .stop:
            for v in library.volumes {
                startInApp(v)
            }
        }
        // Agents of volumes removed meanwhile, or left from a setting change.
        let wanted = settings.onQuit == .keepSyncing ? Set(library.volumes.map(\.key)) : []
        for key in LoginAgents.installedKeys() where !wanted.contains(key) {
            LoginAgents.remove(key)
        }
        for v in library.volumes where v.mode == .finder {
            Task { await registerFinder(v) }
        }
        pollUsage()
        poller = Timer.scheduledTimer(withTimeInterval: 5, repeats: true) { _ in
            Task { @MainActor in AppModel.shared.pollUsage() }
        }
    }

    /// Stops the volumes the app runs itself; login agents keep going.
    func quitting() {
        poller?.invalidate()
        let all = Array(daemons.values)
        daemons = [:]
        // In parallel: each may take a moment to unmount or save.
        DispatchQueue.concurrentPerform(iterations: all.count) { all[$0].stop() }
    }

    func state(of key: String) -> String {
        if let s = states[key] { return s }
        return settings.onQuit == .keepSyncing ? "in the background" : "stopped"
    }

    private func start(_ v: VolumeConfig) {
        switch settings.onQuit {
        case .keepSyncing: startAgent(v)
        case .stop: startInApp(v)
        }
    }

    private func stop(_ v: VolumeConfig) {
        daemons.removeValue(forKey: v.key)?.stop()
        LoginAgents.remove(v.key)
        states[v.key] = nil
    }

    private func restart(_ v: VolumeConfig) {
        stop(v)
        start(v)
    }

    private func startInApp(_ v: VolumeConfig) {
        let key = v.key
        let d = Daemon(volume: v, connection: library.connection(v.connection), settings: settings) { state in
            Task { @MainActor in AppModel.shared.states[key] = state }
        }
        daemons[key] = d
        d.start()
    }

    private func startAgent(_ v: VolumeConfig) {
        do {
            try LoginAgents.install(v.key)
            states[v.key] = "in the background"
        } catch {
            states[v.key] = "not running: \(error.localizedDescription)"
        }
    }

    // MARK: Settings

    func update(_ change: (inout AppSettings) -> Void) {
        let before = library.settings
        change(&library.settings)
        guard library.settings != before else { return }
        save()
        if library.settings.onQuit != before.onQuit {
            // Hand every volume over: stop it one way, start it the other.
            for v in library.volumes {
                daemons.removeValue(forKey: v.key)?.stop()
                LoginAgents.remove(v.key)
                states[v.key] = nil
                start(v)
            }
        } else if library.settings.mountBackend != before.mountBackend {
            for v in library.volumes where v.mode == .mount {
                restart(v)
            }
        }
    }

    // MARK: Connections

    /// Saves a new or changed connection. Secrets left empty keep what is
    /// saved; a CA certificate is copied into the app's container.
    func save(connection c: Connection, token: String, s3Secret: String, caSource: String) throws {
        var c = c
        if !caSource.isEmpty {
            let dst = try Shared.caFile(c.id)
            let src = URL(fileURLWithPath: (caSource as NSString).expandingTildeInPath)
            if src.standardizedFileURL != dst.standardizedFileURL {
                try? FileManager.default.removeItem(at: dst)
                try FileManager.default.copyItem(at: src, to: dst)
            }
            c.caFile = dst.path
        }
        if !token.isEmpty { try Secrets.save(token, .hubToken, for: c.id) }
        if c.s3 == nil {
            Secrets.delete(.s3Secret, for: c.id)
        } else if !s3Secret.isEmpty {
            try Secrets.save(s3Secret, .s3Secret, for: c.id)
        }
        let changed = library.connection(c.id).map { $0 != c } ?? false
        if let i = library.connections.firstIndex(where: { $0.id == c.id }) {
            library.connections[i] = c
        } else {
            library.connections.append(c)
        }
        save()
        // Volumes pick up a changed hub or bucket on restart.
        if changed || !token.isEmpty || !s3Secret.isEmpty {
            for v in library.volumes where v.connection == c.id {
                restart(v)
            }
        }
    }

    func remove(connection id: UUID) {
        guard !library.volumes.contains(where: { $0.connection == id }) else {
            lastError = "Remove the volumes that use this connection first."
            return
        }
        library.connections.removeAll { $0.id == id }
        Secrets.delete(.hubToken, for: id)
        Secrets.delete(.s3Secret, for: id)
        if let ca = try? Shared.caFile(id) { try? FileManager.default.removeItem(at: ca) }
        save()
    }

    /// The volumes a connection's token opens.
    func volumes(of c: Connection) async throws -> [String] {
        if let demo { return demo.volumes }
        let out = try await runTool(volumesArguments(c), connection: c)
        return try JSONDecoder().decode([String].self, from: Data(out.utf8))
    }

    // MARK: Volumes

    /// Adds volume `name` from connection `c`: downloads it (everything in
    /// folder mode, the tree otherwise) and starts syncing it.
    func addVolume(name: String, connection c: Connection, mode: Mode, path: String?, cacheSize: UInt64?) async throws {
        var v = VolumeConfig(
            key: library.newKey(for: name), name: name, connection: c.id, mode: mode,
            path: path.map { ($0 as NSString).expandingTildeInPath },
            cacheSize: mode.isLazy ? (cacheSize ?? settings.defaultCacheSize) : nil)
        if mode != .finder, (v.path ?? "").isEmpty {
            throw failure(mode == .folder ? "Choose where to put the folder." : "Choose where to mount it.")
        }
        if mode == .folder, let p = v.path, FileManager.default.fileExists(atPath: p) {
            throw failure("\(p) already exists; choose a new folder.")
        }
        if mode == .mount && !macfuseAvailable() {
            throw failure("Mounting needs macFUSE.")
        }
        let dir = syncedDir(v, container: try Shared.container())
        if mode != .folder {
            try FileManager.default.createDirectory(at: dir.deletingLastPathComponent(), withIntermediateDirectories: true)
        }
        _ = try await runTool(attachArguments(v, c, dir: dir), connection: c)
        v.path = v.path.map { URL(fileURLWithPath: $0).standardizedFileURL.path }
        library.volumes.append(v)
        save()
        start(v)
        if mode == .finder { await registerFinder(v) }
    }

    /// Creates volume `name` from the folder at `path`, which then syncs in
    /// place (folder mode).
    func createVolume(name: String, connection c: Connection, folder path: String) async throws {
        guard Shared.validName(name) else {
            throw failure("Use up to 40 letters, digits, '.', '_' or '-' for the name, not starting with '.'.")
        }
        let dir = URL(fileURLWithPath: (path as NSString).expandingTildeInPath).standardizedFileURL
        _ = try await runTool(initArguments(name: name, c, dir: dir), connection: c)
        let v = VolumeConfig(key: library.newKey(for: name), name: name, connection: c.id, mode: .folder, path: dir.path)
        library.volumes.append(v)
        save()
        start(v)
    }

    /// Stops the volume and forgets it. Its files stay where they are (the
    /// app's container for Finder and mount volumes), so nothing not yet
    /// synced is lost.
    func remove(volume key: String) async {
        guard let v = library.volume(key) else { return }
        stop(v)
        if v.mode == .finder {
            do {
                try await NSFileProviderManager.remove(finderDomain(v))
            } catch {
                lastError = "Removing \(v.name) from Finder: \(error.localizedDescription)"
            }
        }
        library.volumes.removeAll { $0.key == key }
        usage[key] = nil
        save()
    }

    func setCacheSize(_ key: String, _ bytes: UInt64) {
        guard let i = library.volumes.firstIndex(where: { $0.key == key }), library.volumes[i].cacheSize != bytes else {
            return
        }
        library.volumes[i].cacheSize = bytes
        save()
        restart(library.volumes[i])
    }

    func showLog(_ key: String) {
        if let url = try? Shared.log(key) {
            NSWorkspace.shared.open(url)
        }
    }

    func showInFinder(_ v: VolumeConfig) {
        Task {
            if let url = await visibleURL(v, path: "") { NSWorkspace.shared.open(url) }
        }
    }

    /// Where `path` (relative to the volume's top) is for the user: in the
    /// folder or mount, or in Finder's location for a Finder volume.
    func visibleURL(_ v: VolumeConfig, path: String) async -> URL? {
        let root: URL?
        switch v.mode {
        case .folder, .mount:
            root = v.path.map { URL(fileURLWithPath: $0, isDirectory: true) }
        case .finder:
            root = await withCheckedContinuation { cont in
                guard let manager = NSFileProviderManager(for: finderDomain(v)) else {
                    cont.resume(returning: nil)
                    return
                }
                manager.getUserVisibleURL(for: .rootContainer) { url, _ in cont.resume(returning: url) }
            }
        }
        return path.isEmpty ? root : root?.appendingPathComponent(path)
    }

    /// Selects `paths` of `v` in a Finder window.
    func reveal(_ v: VolumeConfig, paths: [String]) {
        Task {
            var urls: [URL] = []
            for p in paths {
                if let url = await visibleURL(v, path: p) { urls.append(url) }
            }
            NSWorkspace.shared.activateFileViewerSelecting(urls)
        }
    }

    /// Opens `path` of `v` with its app, which downloads it if needed.
    func open(_ v: VolumeConfig, path: String) {
        Task {
            if let url = await visibleURL(v, path: path) { NSWorkspace.shared.open(url) }
        }
    }

    /// The conflict copies and diverged branches in `v` on this Mac.
    func conflicts(of v: VolumeConfig) async throws -> ConflictReport {
        if let demo { return demo.conflicts }
        let dir = syncedDir(v, container: try Shared.container())
        let out = try await runTool(conflictsArguments(dir: dir), connection: nil)
        return try JSONDecoder().decode(ConflictReport.self, from: Data(out.utf8))
    }

    // MARK: Bucket

    /// The folder `path` of volume `volume` as the bucket holds it.
    func listRemote(_ c: Connection, volume: String, path: String) async throws -> RemoteListing {
        if let demo {
            guard let listing = demo.bucket[path] else { throw failure("No folder \(path) in the sample.") }
            return listing
        }
        let out = try await runTool(lsArguments(volume: volume, path: path, c), connection: c)
        return try JSONDecoder().decode(RemoteListing.self, from: Data(out.utf8))
    }

    /// Downloads the file or folder `path` of `volume` to `dest`.
    func download(_ c: Connection, volume: String, path: String, to dest: URL) async throws {
        _ = try await runTool(getArguments(volume: volume, path: path, dest: dest, c), connection: c)
    }

    /// Asks each volume's daemon how much disk it uses. A daemon that
    /// answers is running, also one a login agent runs.
    func pollUsage() {
        for v in library.volumes {
            guard let socket = try? Shared.socket(v.key) else { continue }
            let key = v.key
            Task.detached {
                let client = Client(socketPath: socket.path)
                defer { client.close() }
                let answer = try? client.usage()
                await MainActor.run {
                    let model = AppModel.shared
                    if let a = answer {
                        model.usage[key] = VolumeUsage(usage: a.usage, limit: a.limit, lazy: a.lazy)
                        if model.states[key] == nil || model.states[key] == "in the background" {
                            model.states[key] = "running"
                        }
                    } else if model.settings.onQuit == .keepSyncing, model.daemons[key] == nil {
                        model.states[key] = "in the background, not answering"
                    }
                }
            }
        }
    }

    // MARK: Helpers

    private func registerFinder(_ v: VolumeConfig) async {
        do {
            try await NSFileProviderManager.add(finderDomain(v))
        } catch {
            lastError = "Adding \(v.name) to Finder: \(error.localizedDescription)"
        }
    }

    private func save() {
        guard demo == nil else { return }
        do {
            try Shared.saveLibrary(library)
        } catch {
            lastError = "Saving the app's settings: \(error.localizedDescription)"
            log.error("saving the library: \(error.localizedDescription, privacy: .public)")
        }
    }

    private func failure(_ message: String) -> Error {
        CocoaError(.validationMultipleErrors, userInfo: [NSLocalizedDescriptionKey: message])
    }
}
