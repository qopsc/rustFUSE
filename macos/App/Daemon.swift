import FileProvider
import Foundation
import RustFuseKit
import os

private let log = Logger(subsystem: "dev.rustfuse", category: "daemon")

/// The bundled `rustfuse` command line tool.
func rustfuseTool() -> URL {
    Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/rustfuse")
}

/// Runs `rustfuse` to completion; returns its combined output, or throws it
/// if it failed.
func runTool(_ args: [String], token: String?) async throws -> String {
    try await withCheckedThrowingContinuation { cont in
        let p = Process()
        p.executableURL = rustfuseTool()
        p.arguments = args
        p.environment = toolEnvironment(token: token)
        let pipe = Pipe()
        p.standardOutput = pipe
        p.standardError = pipe
        do {
            try p.run()
        } catch {
            cont.resume(throwing: error)
            return
        }
        // Read while it runs, so a chatty tool cannot fill the pipe and stall.
        DispatchQueue.global().async {
            let out = String(decoding: pipe.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
            p.waitUntilExit()
            if p.terminationStatus == 0 {
                cont.resume(returning: out)
            } else {
                cont.resume(throwing: CocoaError(.executableLoad, userInfo: [NSLocalizedDescriptionKey: out]))
            }
        }
    }
}

func toolEnvironment(token: String?) -> [String: String] {
    var env = ProcessInfo.processInfo.environment
    env["RUSTFUSE_HUB_TOKEN"] = token
    env["RUST_LOG"] = env["RUST_LOG"] ?? "info"
    return env
}

/// Keeps one volume's `rustfuse sync --provider-socket` running, restarting
/// it if it exits, and tells File Provider whenever the volume changes.
final class Daemon: @unchecked Sendable {
    let name: String
    private let queue: DispatchQueue
    private var process: Process?
    private var stopped = false
    private var restartDelay: TimeInterval = 1
    private var watcher: DispatchSourceTimer?
    private var lastAnchor: String?
    private let onState: @Sendable (String) -> Void

    init(name: String, onState: @escaping @Sendable (String) -> Void) {
        self.name = name
        self.onState = onState
        queue = DispatchQueue(label: "dev.rustfuse.daemon.\(name)")
    }

    func start() {
        queue.async { [self] in
            stopped = false
            launch()
            watch()
        }
    }

    func stop() {
        queue.sync {
            stopped = true
            watcher?.cancel()
            watcher = nil
            process?.terminate()
            process?.waitUntilExit()
            process = nil
        }
    }

    private func launch() {
        guard !stopped else { return }
        do {
            let dir = try Shared.volumeDir(name)
            let socket = try Shared.socket(name)
            let logURL = try Shared.log(name)
            if !FileManager.default.fileExists(atPath: logURL.path) {
                FileManager.default.createFile(atPath: logURL.path, contents: nil)
            }
            let logFile = try FileHandle(forWritingTo: logURL)
            logFile.seekToEndOfFile()
            let p = Process()
            p.executableURL = rustfuseTool()
            p.arguments = ["sync", dir.path, "--provider-socket", socket.path]
            p.environment = toolEnvironment(token: Keychain.token(for: name))
            p.standardOutput = logFile
            p.standardError = logFile
            let started = Date()
            p.terminationHandler = { [weak self] p in
                guard let self else { return }
                self.queue.async {
                    guard !self.stopped else { return }
                    // Back off while it keeps failing; a run that lasted a
                    // while starts the delay over.
                    self.restartDelay =
                        Date().timeIntervalSince(started) > 60 ? 1 : min(self.restartDelay * 2, 60)
                    self.onState("stopped (exit \(p.terminationStatus)), restarting")
                    log.error("\(self.name, privacy: .public) exited with \(p.terminationStatus)")
                    self.queue.asyncAfter(deadline: .now() + self.restartDelay) { self.launch() }
                }
            }
            try p.run()
            process = p
            onState("running")
        } catch {
            onState("not running: \(error.localizedDescription)")
            queue.asyncAfter(deadline: .now() + 10) { [self] in launch() }
        }
    }

    /// Polls the daemon's change journal and signals the working set when it
    /// moves, so the extension picks up changes from other machines.
    private func watch() {
        guard let socket = try? Shared.socket(name) else { return }
        let client = Client(socketPath: socket.path)
        let domain = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(rawValue: name), displayName: name)
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + 1, repeating: 1)
        timer.setEventHandler { [weak self] in
            guard let self, let anchor = try? client.anchor(), anchor != self.lastAnchor else { return }
            self.lastAnchor = anchor
            NSFileProviderManager(for: domain)?.signalEnumerator(for: .workingSet) { error in
                if let error {
                    log.error("signalling \(self.name, privacy: .public): \(error.localizedDescription, privacy: .public)")
                }
            }
        }
        timer.resume()
        watcher = timer
    }
}
