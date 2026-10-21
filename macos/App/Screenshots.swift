import AppKit
import RustFuseKit
import SwiftUI

/// `rustFUSE --screenshots <dir>`: shows the main window, its views and
/// sheets with sample data, saves a PNG of each into `dir`, and quits. CI
/// runs it so the UI can be looked at without a Mac at hand. Nothing is
/// read, run or saved: the model only shows `sampleModel()`.
@MainActor
func takeScreenshots(into dir: URL) -> Never {
    try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    let app = NSApplication.shared
    app.setActivationPolicy(.regular)
    let model = sampleModel()
    let home = model.library.connections[0].id
    let main = NSSize(width: 1180, height: 760)
    let shots: [(name: String, dark: Bool, size: NSSize, view: AnyView)] = [
        ("1-volume-overview", false, main, AnyView(MainWindow(model: model, selection: .volume("proj"), tab: .overview))),
        ("2-volume-files", false, main, AnyView(MainWindow(model: model, selection: .volume("proj"), tab: .files))),
        ("3-volume-in-the-bucket", false, main, AnyView(MainWindow(model: model, selection: .volume("proj"), tab: .bucket))),
        ("4-connection", false, main, AnyView(MainWindow(model: model, selection: .connection(home)))),
        ("5-add-volume", false, NSSize(width: 560, height: 520), AnyView(AddVolumeSheet(model: model, connectionID: home, preset: "site"))),
        ("6-settings", false, NSSize(width: 520, height: 460), AnyView(SettingsView(model: model))),
        ("7-volume-overview-dark", true, main, AnyView(MainWindow(model: model, selection: .volume("proj"), tab: .overview))),
        ("8-volume-files-dark", true, main, AnyView(MainWindow(model: model, selection: .volume("proj"), tab: .files))),
    ]
    Task { @MainActor in
        for shot in shots {
            app.appearance = NSAppearance(named: shot.dark ? .darkAqua : .aqua)
            let window = NSWindow(
                contentRect: NSRect(origin: NSPoint(x: 60, y: 60), size: shot.size),
                styleMask: [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView],
                backing: .buffered, defer: false)
            window.title = "rustFUSE"
            window.toolbarStyle = .unified
            window.isReleasedWhenClosed = false
            let host = NSHostingView(rootView: shot.view)
            host.sceneBridgingOptions = .all
            window.contentView = host
            window.makeKeyAndOrderFront(nil)
            app.activate()
            // Lets the views lay out and their sample data load.
            try? await Task.sleep(for: .seconds(2))
            capture(window, to: dir.appendingPathComponent("\(shot.name).png"))
            window.close()
        }
        exit(0)
    }
    app.run()
    exit(0)
}

/// Saves `window` as the window server shows it (`screencapture`, which
/// needs a logged-in session), or else as AppKit draws it offscreen.
@MainActor
private func capture(_ window: NSWindow, to file: URL) {
    let p = Process()
    p.executableURL = URL(fileURLWithPath: "/usr/sbin/screencapture")
    p.arguments = ["-x", "-o", "-l", String(window.windowNumber), file.path]
    try? p.run()
    p.waitUntilExit()
    if p.terminationStatus == 0, FileManager.default.fileExists(atPath: file.path) {
        return
    }
    guard let view = window.contentView?.superview ?? window.contentView,
        let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds)
    else { return }
    view.cacheDisplay(in: view.bounds, to: rep)
    try? rep.representation(using: .png, properties: [:])?.write(to: file)
}

/// A model with three volumes on a home NAS, one of each kind, and a
/// sample tree.
@MainActor
func sampleModel() -> AppModel {
    var lib = Library()
    let home = Connection(
        name: "Home NAS", hub: "tls://nas.local:7420",
        s3: S3Access(bucket: "rustfs", endpoint: "http://nas.local:9000", accessKey: "rustfuse"))
    lib.connections = [home]
    let homeDir = NSHomeDirectory()
    lib.volumes = [
        VolumeConfig(key: "proj", name: "proj", connection: home.id, mode: .finder, cacheSize: 20 * gigabyte),
        VolumeConfig(key: "docs", name: "docs", connection: home.id, mode: .folder, path: "\(homeDir)/rustFUSE/docs"),
        VolumeConfig(key: "media", name: "media", connection: home.id, mode: .mount, path: "/Volumes/media", cacheSize: 50 * gigabyte),
    ]
    let usage = [
        "proj": VolumeUsage(
            usage: decode(#"{"files": 48210, "total": 9900000000, "local": 3300000000, "git": 1500000000, "pinned": 640000000, "cached": 1160000000}"#),
            limit: 20 * gigabyte, lazy: true),
        "docs": VolumeUsage(
            usage: decode(#"{"files": 1204, "total": 820000000, "local": 820000000, "git": 0, "pinned": 0, "cached": 0}"#),
            limit: nil, lazy: false),
        "media": VolumeUsage(
            usage: decode(#"{"files": 9312, "total": 412000000000, "local": 51800000000, "git": 0, "pinned": 2100000000, "cached": 49700000000}"#),
            limit: 50 * gigabyte, lazy: true),
    ]
    let states = ["proj": "running", "docs": "running", "media": "running"]
    let root = [
        item("a1", ".git", .dir, pinned: true),
        item("a2", "crates", .dir),
        item("a3", "docs", .dir, pinned: true),
        item("a4", "macos", .dir),
        item("a5", "Cargo.toml", .file, size: 1_834),
        item("a6", "Cargo.lock", .file, size: 98_112),
        item("a7", "README.md", .file, size: 12_408, downloaded: false),
        item("a8", "README.conflict-0badf00d-20261020-091500.md", .file, size: 12_377),
        item("a9", "demo.mov", .file, size: 742_000_000, downloaded: false),
        item("a10", "release.tar.zst", .file, size: 318_000_000, downloaded: false, pinned: true),
        item("a11", "notes.txt", .file, size: 2_210, synced: false),
    ]
    let bucket: RemoteListing = decode(
        #"""
        {"seq": 1842, "entries": [
          {"name": ".git", "kind": "dir", "size": 0, "mode": 493, "mtime_ns": 1792500000000000000},
          {"name": "crates", "kind": "dir", "size": 0, "mode": 493, "mtime_ns": 1792400000000000000},
          {"name": "docs", "kind": "dir", "size": 0, "mode": 493, "mtime_ns": 1792300000000000000},
          {"name": "macos", "kind": "dir", "size": 0, "mode": 493, "mtime_ns": 1792500000000000000},
          {"name": "Cargo.lock", "kind": "file", "size": 98112, "mode": 420, "mtime_ns": 1792480000000000000},
          {"name": "Cargo.toml", "kind": "file", "size": 1834, "mode": 420, "mtime_ns": 1792480000000000000},
          {"name": "README.md", "kind": "file", "size": 12408, "mode": 420, "mtime_ns": 1792460000000000000},
          {"name": "README.conflict-0badf00d-20261020-091500.md", "kind": "file", "size": 12377, "mode": 420, "mtime_ns": 1792440000000000000},
          {"name": "demo.mov", "kind": "file", "size": 742000000, "mode": 420, "mtime_ns": 1792100000000000000},
          {"name": "release.tar.zst", "kind": "file", "size": 318000000, "mode": 420, "mtime_ns": 1792200000000000000},
          {"name": "latest", "kind": "symlink", "size": 15, "mode": 511, "mtime_ns": 1792200000000000000, "target": "release.tar.zst"}
        ]}
        """#)
    let conflicts: ConflictReport = decode(
        #"""
        {"files": ["README.conflict-0badf00d-20261020-091500.md", "crates/rustfuse/src/main.conflict-0badf00d-20261020-091500.rs"],
         "branches": [{"repo": ".", "branch": "main.conflict-0badf00d-20261020-091500"}]}
        """#)
    return AppModel(
        library: lib, usage: usage, states: states,
        demo: AppModel.Demo(
            items: [rootID: root], bucket: ["": bucket], conflicts: conflicts,
            volumes: ["proj", "docs", "media", "site"]))
}

private func item(
    _ id: String, _ name: String, _ kind: Kind, size: UInt64 = 0, downloaded: Bool = true, synced: Bool = true,
    pinned: Bool = false
) -> Item {
    let mode = kind == .dir ? 493 : 420
    return decode(
        """
        {"id": "\(id)", "parent": "\(rootID)", "name": "\(name)", "kind": "\(kind.rawValue)", "size": \(size),
         "mode": \(mode), "mtime_ns": 1792500000000000000, "content_version": "c\(id)", "meta_version": "m\(id)",
         "downloaded": \(downloaded), "synced": \(synced), "pinned": \(pinned)}
        """)
}

private func decode<T: Decodable>(_ json: String) -> T {
    do {
        return try JSONDecoder().decode(T.self, from: Data(json.utf8))
    } catch {
        fatalError("sample \(T.self): \(error)")
    }
}
