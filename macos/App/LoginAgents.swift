import Foundation
import RustFuseKit
import os

private let log = Logger(subsystem: "dev.rustfuse", category: "agents")

/// "Keep syncing after quit": one launchd login agent per volume runs this
/// app headless (`rustFUSE --daemon <key>`), which keeps the volume's daemon
/// running, also after the app quits and from the next login on. The agent
/// reads secrets from the keychain like the app does, so none are written
/// to its plist.
enum LoginAgents {
    private static var prefix: String {
        (Bundle.main.bundleIdentifier ?? "dev.rustfuse.app") + ".volume."
    }

    static func label(_ key: String) -> String {
        prefix + key
    }

    private static var directory: URL {
        FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/LaunchAgents", isDirectory: true)
    }

    private static func plist(_ key: String) -> URL {
        directory.appendingPathComponent("\(label(key)).plist")
    }

    private static var domain: String { "gui/\(getuid())" }

    /// Installs (or replaces, restarting it) the agent for `key`.
    static func install(_ key: String) throws {
        guard let exe = Bundle.main.executablePath else {
            throw CocoaError(.fileNoSuchFile, userInfo: [NSLocalizedDescriptionKey: "no app executable"])
        }
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let agentLog = try Shared.log(key).deletingPathExtension().appendingPathExtension("agent.log")
        let desc: [String: Any] = [
            "Label": label(key),
            "ProgramArguments": [exe, "--daemon", key],
            "RunAtLoad": true,
            "KeepAlive": true,
            // Needs the user's session: the keychain and File Provider.
            "LimitLoadToSessionType": "Aqua",
            "StandardOutPath": agentLog.path,
            "StandardErrorPath": agentLog.path,
        ]
        let data = try PropertyListSerialization.data(fromPropertyList: desc, format: .xml, options: 0)
        try data.write(to: plist(key), options: .atomic)
        launchctl(["bootout", "\(domain)/\(label(key))"])
        let status = launchctl(["bootstrap", domain, plist(key).path])
        guard status == 0 else {
            throw CocoaError(
                .executableLoad, userInfo: [NSLocalizedDescriptionKey: "launchctl bootstrap failed (\(status))"])
        }
    }

    /// Stops the agent for `key` (and its daemon) and removes it.
    static func remove(_ key: String) {
        launchctl(["bootout", "\(domain)/\(label(key))"])
        try? FileManager.default.removeItem(at: plist(key))
    }

    /// Whether the agent for `key` is installed.
    static func installed(_ key: String) -> Bool {
        FileManager.default.fileExists(atPath: plist(key).path)
    }

    /// Keys of every installed agent of this app.
    static func installedKeys() -> [String] {
        let names = (try? FileManager.default.contentsOfDirectory(atPath: directory.path)) ?? []
        return names.compactMap { name in
            guard name.hasPrefix(prefix), name.hasSuffix(".plist") else { return nil }
            return String(name.dropFirst(prefix.count).dropLast(".plist".count))
        }
    }

    @discardableResult
    private static func launchctl(_ args: [String]) -> Int32 {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/bin/launchctl")
        p.arguments = args
        p.standardOutput = FileHandle.nullDevice
        p.standardError = FileHandle.nullDevice
        do {
            try p.run()
            p.waitUntilExit()
            return p.terminationStatus
        } catch {
            log.error("launchctl \(args.joined(separator: " "), privacy: .public): \(error.localizedDescription, privacy: .public)")
            return -1
        }
    }
}
