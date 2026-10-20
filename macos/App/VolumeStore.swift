import AppKit
import FileProvider
import RustFuseKit

/// The volumes this Mac runs, saved in the app group container, each with
/// its File Provider domain and its daemon.
@MainActor
final class VolumeStore: ObservableObject {
    static let shared = VolumeStore()

    @Published private(set) var volumes: [Volume] = []
    @Published private var states: [String: String] = [:]
    @Published var lastError: String?
    private var daemons: [String: Daemon] = [:]

    init() {
        do {
            let url = try Shared.volumesFile()
            if FileManager.default.fileExists(atPath: url.path) {
                volumes = try JSONDecoder().decode([Volume].self, from: Data(contentsOf: url))
            }
        } catch {
            lastError = "Reading the volume list: \(error.localizedDescription)"
        }
    }

    func state(of name: String) -> String {
        states[name] ?? "stopped"
    }

    func startAll() {
        for v in volumes {
            Task { await register(v.name) }
            start(v.name)
        }
    }

    func stopAll() {
        daemons.values.forEach { $0.stop() }
        daemons = [:]
    }

    /// Downloads the volume's metadata (`rustfuse attach --lazy`), adds its
    /// File Provider domain and starts its daemon.
    func add(name: String, hub: String, token: String, caPath: String) async -> Bool {
        lastError = nil
        guard Shared.validName(name) else {
            lastError = "Use up to 40 letters, digits, '.', '_' or '-' for the name."
            return false
        }
        guard !volumes.contains(where: { $0.name == name }) else {
            lastError = "\(name) is already added."
            return false
        }
        do {
            let dir = try Shared.volumeDir(name)
            var args = ["attach", "--lazy", dir.path, "--volume", name, "--hub", hub]
            if !caPath.isEmpty {
                // Kept with the volume: the daemon reads it on every start.
                let ca = try Shared.volumeDir(name).deletingLastPathComponent().appendingPathComponent("\(name).ca.pem")
                try? FileManager.default.removeItem(at: ca)
                try FileManager.default.copyItem(at: URL(fileURLWithPath: (caPath as NSString).expandingTildeInPath), to: ca)
                args += ["--hub-ca", ca.path]
            }
            let token = token.isEmpty ? nil : token
            _ = try await runTool(args, token: token)
            if let token {
                try Keychain.save(token, for: name)
            }
            volumes.append(Volume(name: name, hub: hub))
            try save()
            start(name)
            await register(name)
            return true
        } catch {
            lastError = "Adding \(name): \(error.localizedDescription)"
            return false
        }
    }

    /// Stops the volume and removes it from Finder. Its synced directory is
    /// left in the app group container, so nothing not yet synced is lost.
    func remove(_ name: String) async {
        daemons.removeValue(forKey: name)?.stop()
        states[name] = nil
        do {
            try await NSFileProviderManager.remove(domain(name))
        } catch {
            lastError = "Removing \(name) from Finder: \(error.localizedDescription)"
        }
        Keychain.delete(name)
        volumes.removeAll { $0.name == name }
        do {
            try save()
        } catch {
            lastError = "Saving the volume list: \(error.localizedDescription)"
        }
    }

    func showLog(_ name: String) {
        if let url = try? Shared.log(name) {
            NSWorkspace.shared.open(url)
        }
    }

    private func start(_ name: String) {
        let d = Daemon(name: name) { [weak self] state in
            Task { @MainActor in self?.states[name] = state }
        }
        daemons[name] = d
        d.start()
    }

    private func register(_ name: String) async {
        do {
            try await NSFileProviderManager.add(domain(name))
        } catch {
            lastError = "Adding \(name) to Finder: \(error.localizedDescription)"
        }
    }

    private func domain(_ name: String) -> NSFileProviderDomain {
        NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(rawValue: name), displayName: name)
    }

    private func save() throws {
        try JSONEncoder().encode(volumes).write(to: Shared.volumesFile(), options: .atomic)
    }
}
