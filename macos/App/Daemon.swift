import FileProvider
import Foundation
import RustFuseKit
import os

private let log = Logger(subsystem: "dev.rustfuse", category: "daemon")

/// The bundled `rustfuse` command line tool.
func rustfuseTool() -> URL {
    Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/rustfuse")
}

/// `rustfuse` built with macFUSE support, bundled when the app was built on
/// a Mac with macFUSE installed. It only starts where macFUSE is installed.
func macfuseTool() -> URL {
    Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/rustfuse-macfuse")
}

/// Whether mount mode can work here: macFUSE installed, and a daemon that
/// can use it in the bundle.
func macfuseAvailable() -> Bool {
    FileManager.default.fileExists(atPath: "/Library/Filesystems/macfuse.fs")
        && FileManager.default.isExecutableFile(atPath: macfuseTool().path)
}

/// The environment carrying `connection`'s secrets.
func toolEnv(for connection: Connection?) -> [String: String] {
    toolEnvironment(
        base: ProcessInfo.processInfo.environment,
        token: connection.flatMap { Secrets.get(.hubToken, for: $0.id) },
        s3Secret: connection.flatMap { Secrets.get(.s3Secret, for: $0.id) },
        connection)
}

/// Runs `rustfuse` to completion with `connection`'s secrets; returns its
/// standard output, or throws its output if it failed.
func runTool(_ args: [String], connection: Connection?) async throws -> String {
    try await runProcess(rustfuseTool(), args, env: toolEnv(for: connection))
}

/// Runs `tool` to completion; returns its standard output, or throws its
/// error output if it failed. Output goes through temporary files, so a
/// chatty tool cannot fill a pipe and stall.
func runProcess(_ tool: URL, _ args: [String], env: [String: String]) async throws -> String {
    let tmp = FileManager.default.temporaryDirectory.appendingPathComponent("rustfuse-\(UUID().uuidString)")
    let outURL = tmp.appendingPathExtension("out")
    let errURL = tmp.appendingPathExtension("err")
    defer {
        try? FileManager.default.removeItem(at: outURL)
        try? FileManager.default.removeItem(at: errURL)
    }
    FileManager.default.createFile(atPath: outURL.path, contents: nil)
    FileManager.default.createFile(atPath: errURL.path, contents: nil)
    let status: Int32 = try await withCheckedThrowingContinuation { cont in
        let p = Process()
        p.executableURL = tool
        p.arguments = args
        p.environment = env
        p.terminationHandler = { cont.resume(returning: $0.terminationStatus) }
        do {
            p.standardOutput = try FileHandle(forWritingTo: outURL)
            p.standardError = try FileHandle(forWritingTo: errURL)
            try p.run()
        } catch {
            cont.resume(throwing: error)
        }
    }
    let out = (try? String(contentsOf: outURL, encoding: .utf8)) ?? ""
    guard status == 0 else {
        let err = (try? String(contentsOf: errURL, encoding: .utf8)) ?? ""
        let message = (err.isEmpty ? out : err).trimmingCharacters(in: .whitespacesAndNewlines)
        throw CocoaError(.executableLoad, userInfo: [NSLocalizedDescriptionKey: message])
    }
    return out
}

/// Keeps one volume's daemon running (`rustfuse sync`, or `rustfuse mount`
/// in mount mode), restarting it if it exits. For a Finder volume it also
/// tells File Provider whenever the volume changes.
final class Daemon: @unchecked Sendable {
    let volume: VolumeConfig
    private let connection: Connection?
    private let settings: AppSettings
    private let queue: DispatchQueue
    private var process: Process?
    private var stopped = false
    private var restartDelay: TimeInterval = 1
    private var watcher: DispatchSourceTimer?
    private var lastAnchor: String?
    private let onState: @Sendable (String) -> Void

    init(
        volume: VolumeConfig, connection: Connection?, settings: AppSettings,
        onState: @escaping @Sendable (String) -> Void = { _ in }
    ) {
        self.volume = volume
        self.connection = connection
        self.settings = settings
        self.onState = onState
        queue = DispatchQueue(label: "dev.rustfuse.daemon.\(volume.key)")
    }

    func start() {
        queue.async { [self] in
            stopped = false
            launch()
            if volume.mode == .finder { watch() }
        }
    }

    /// Stops the daemon and waits for it to finish (a mount unmounts), at
    /// most ten seconds before killing it.
    func stop() {
        queue.sync {
            stopped = true
            watcher?.cancel()
            watcher = nil
            guard let p = process else { return }
            process = nil
            p.terminate()
            let deadline = Date().addingTimeInterval(10)
            while p.isRunning && Date() < deadline {
                Thread.sleep(forTimeInterval: 0.05)
            }
            if p.isRunning {
                kill(p.processIdentifier, SIGKILL)
                log.error("\(self.volume.key, privacy: .public) did not stop in time")
            }
            onState("stopped")
        }
    }

    private func launch() {
        guard !stopped else { return }
        let tool = volume.mode == .mount ? macfuseTool() : rustfuseTool()
        if volume.mode == .mount && !macfuseAvailable() {
            onState("needs macFUSE")
            queue.asyncAfter(deadline: .now() + 30) { [self] in launch() }
            return
        }
        do {
            let container = try Shared.container()
            let dir = syncedDir(volume, container: container)
            let socket = try Shared.socket(volume.key)
            let logURL = try Shared.log(volume.key)
            if !FileManager.default.fileExists(atPath: logURL.path) {
                FileManager.default.createFile(atPath: logURL.path, contents: nil)
            }
            let logFile = try FileHandle(forWritingTo: logURL)
            logFile.seekToEndOfFile()
            if volume.mode == .mount, let mountpoint = volume.path {
                try? FileManager.default.createDirectory(atPath: mountpoint, withIntermediateDirectories: true)
            }
            let p = Process()
            p.executableURL = tool
            p.arguments = daemonArguments(volume, dir: dir, socket: socket, settings: settings)
            p.environment = toolEnv(for: connection)
            p.standardOutput = logFile
            p.standardError = logFile
            let started = Date()
            p.terminationHandler = { [weak self] p in
                guard let self else { return }
                let status = p.terminationStatus
                self.queue.async {
                    guard !self.stopped else { return }
                    // Back off while it keeps failing; a run that lasted a
                    // while starts the delay over.
                    self.restartDelay =
                        Date().timeIntervalSince(started) > 60 ? 1 : min(self.restartDelay * 2, 60)
                    self.onState("stopped (exit \(status)), restarting")
                    log.error("\(self.volume.key, privacy: .public) exited with \(status)")
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
        guard let socket = try? Shared.socket(volume.key) else { return }
        let client = Client(socketPath: socket.path)
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + 1, repeating: 1)
        timer.setEventHandler { [weak self] in
            guard let self, let anchor = try? client.anchor(), anchor != self.lastAnchor else { return }
            self.lastAnchor = anchor
            NSFileProviderManager(for: finderDomain(self.volume))?.signalEnumerator(for: .workingSet) { error in
                if let error {
                    log.error("signalling \(self.volume.key, privacy: .public): \(error.localizedDescription, privacy: .public)")
                }
            }
        }
        timer.resume()
        watcher = timer
    }
}

/// The File Provider domain of a Finder volume: its key, shown by its name.
func finderDomain(_ v: VolumeConfig) -> NSFileProviderDomain {
    NSFileProviderDomain(
        identifier: NSFileProviderDomainIdentifier(rawValue: v.key),
        displayName: v.key == v.name ? v.name : "\(v.name) (\(v.key))")
}

/// `rustFUSE --daemon <key>`, as the background login agent runs it: keeps
/// that volume's daemon running without the app's window or menu, until
/// launchd stops it.
func runHeadless(key: String) -> Never {
    guard let lib = try? Shared.loadLibrary(), let v = lib.volume(key) else {
        log.error("no volume \(key, privacy: .public) in the library")
        exit(1)
    }
    let daemon = Daemon(volume: v, connection: lib.connection(v.connection), settings: lib.settings) { state in
        log.info("\(key, privacy: .public): \(state, privacy: .public)")
    }
    // Kept alive by never returning.
    var sources: [DispatchSourceSignal] = []
    for sig in [SIGTERM, SIGINT] {
        signal(sig, SIG_IGN)
        let s = DispatchSource.makeSignalSource(signal: sig, queue: .global())
        s.setEventHandler {
            daemon.stop()
            exit(0)
        }
        s.resume()
        sources.append(s)
    }
    daemon.start()
    withExtendedLifetime(sources) { dispatchMain() }
}
