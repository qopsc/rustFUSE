import Foundation

// What the app keeps in `library.json` in the app group container: saved
// connections, the volumes this Mac uses and how, and the app's settings.
// Secrets (hub tokens, S3 secret keys) are in the login keychain, never
// here. The daemon command lines are built here so they can be tested.

/// How a volume shows up on this Mac.
public enum Mode: String, Codable, CaseIterable, Sendable {
    /// In Finder under Locations (File Provider). Files download when first
    /// opened.
    case finder
    /// A plain folder anywhere, holding a full copy.
    case folder
    /// A macFUSE mount anywhere. Files download when first opened.
    case mount

    /// Whether content stays in the bucket until used (and a cache size
    /// applies).
    public var isLazy: Bool { self != .folder }
}

/// Reading the bucket directly, beside the hub: faster on a LAN.
public struct S3Access: Codable, Equatable, Sendable {
    public var bucket: String
    /// Defaults to AWS.
    public var endpoint: String?
    public var region: String?
    public var prefix: String?
    public var accessKey: String

    public init(bucket: String, endpoint: String? = nil, region: String? = nil, prefix: String? = nil, accessKey: String) {
        self.bucket = bucket
        self.endpoint = endpoint
        self.region = region
        self.prefix = prefix
        self.accessKey = accessKey
    }
}

/// A saved hub (and optionally its bucket). Its token and S3 secret key
/// are in the keychain under its id.
public struct Connection: Codable, Equatable, Identifiable, Sendable {
    public var id: UUID
    public var name: String
    /// `host[:port]` or `tls://host[:port]`.
    public var hub: String
    /// CA certificates (PEM) to trust for the hub, copied into the container.
    public var caFile: String?
    public var s3: S3Access?

    public init(id: UUID = UUID(), name: String, hub: String, caFile: String? = nil, s3: S3Access? = nil) {
        self.id = id
        self.name = name
        self.hub = hub
        self.caFile = caFile
        self.s3 = s3
    }
}

/// A volume this Mac uses.
public struct VolumeConfig: Codable, Equatable, Identifiable, Sendable {
    /// Unique on this Mac: the File Provider domain and the container's
    /// file names for it. Usually the volume's name.
    public var key: String
    /// The volume's name in the bucket.
    public var name: String
    public var connection: UUID
    public var mode: Mode
    /// `folder`: the synced folder. `mount`: the mount point.
    public var path: String?
    /// Lazy modes: the most fetched content to keep, in bytes.
    public var cacheSize: UInt64?

    public var id: String { key }

    public init(key: String, name: String, connection: UUID, mode: Mode, path: String? = nil, cacheSize: UInt64? = nil) {
        self.key = key
        self.name = name
        self.connection = connection
        self.mode = mode
        self.path = path
        self.cacheSize = cacheSize
    }
}

/// What quitting the app does to syncing.
public enum OnQuit: String, Codable, CaseIterable, Sendable {
    /// Stop every volume (and unmount).
    case stop
    /// Keep every volume syncing in the background (a login agent per
    /// volume, also started at login).
    case keepSyncing
}

/// macFUSE's two ways to mount.
public enum MountBackend: String, Codable, CaseIterable, Sendable {
    /// No kernel extension (macOS 15.4 or later).
    case fskit
    /// macFUSE's kernel extension.
    case kernel
}

public let defaultCacheSize: UInt64 = 20 << 30

public struct AppSettings: Codable, Equatable, Sendable {
    public var onQuit: OnQuit = .stop
    /// For new lazy volumes.
    public var defaultCacheSize: UInt64 = RustFuseKit.defaultCacheSize
    public var mountBackend: MountBackend = .fskit

    public init() {}

    // Settings added later default when an older file lacks them.
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        onQuit = try c.decodeIfPresent(OnQuit.self, forKey: .onQuit) ?? .stop
        defaultCacheSize = try c.decodeIfPresent(UInt64.self, forKey: .defaultCacheSize) ?? RustFuseKit.defaultCacheSize
        mountBackend = try c.decodeIfPresent(MountBackend.self, forKey: .mountBackend) ?? .fskit
    }
}

public struct Library: Codable, Equatable, Sendable {
    public var connections: [Connection] = []
    public var volumes: [VolumeConfig] = []
    public var settings = AppSettings()

    public init() {}

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        connections = try c.decodeIfPresent([Connection].self, forKey: .connections) ?? []
        volumes = try c.decodeIfPresent([VolumeConfig].self, forKey: .volumes) ?? []
        settings = try c.decodeIfPresent(AppSettings.self, forKey: .settings) ?? AppSettings()
    }

    public func connection(_ id: UUID) -> Connection? {
        connections.first { $0.id == id }
    }

    public func volume(_ key: String) -> VolumeConfig? {
        volumes.first { $0.key == key }
    }

    /// A key for a new volume named `name`: the name, or the name with a
    /// number when the same volume is already used another way.
    public func newKey(for name: String) -> String {
        let taken = Set(volumes.map(\.key))
        if !taken.contains(name) { return name }
        var n = 2
        while taken.contains("\(name)-\(n)") { n += 1 }
        return "\(name)-\(n)"
    }

    /// The first app version's `volumes.json`: Finder volumes, each with
    /// its own hub, the token in the keychain under the volume's name.
    public struct Legacy: Codable, Equatable, Sendable {
        public var name: String
        public var hub: String
    }

    /// The library holding `legacy`'s volumes: one connection per hub.
    public static func migrating(_ legacy: [Legacy]) -> Library {
        var lib = Library()
        for old in legacy {
            let conn: Connection
            if let c = lib.connections.first(where: { $0.hub == old.hub }) {
                conn = c
            } else {
                conn = Connection(name: old.hub, hub: old.hub)
                lib.connections.append(conn)
            }
            lib.volumes.append(
                VolumeConfig(key: old.name, name: old.name, connection: conn.id, mode: .finder, cacheSize: defaultCacheSize))
        }
        return lib
    }
}

/// Where a volume's synced directory is, for its mode: the user's folder,
/// or the app group container (`container/volumes/<key>`).
public func syncedDir(_ v: VolumeConfig, container: URL) -> URL {
    if v.mode == .folder, let p = v.path {
        return URL(fileURLWithPath: p, isDirectory: true)
    }
    return container.appendingPathComponent("volumes", isDirectory: true).appendingPathComponent(v.key, isDirectory: true)
}

/// `--hub`, `--hub-ca` and the bucket options for `c`.
func connectionArguments(_ c: Connection) -> [String] {
    var args = ["--hub", c.hub]
    if let ca = c.caFile { args += ["--hub-ca", ca] }
    if let s3 = c.s3 {
        args += ["--bucket", s3.bucket]
        if let e = s3.endpoint, !e.isEmpty { args += ["--endpoint", e] }
        if let r = s3.region, !r.isEmpty { args += ["--region", r] }
        if let p = s3.prefix, !p.isEmpty { args += ["--prefix", p] }
    }
    return args
}

/// `rustfuse attach` for a new volume into `dir`.
public func attachArguments(_ v: VolumeConfig, _ c: Connection, dir: URL) -> [String] {
    ["attach", dir.path, "--volume", v.name] + (v.mode.isLazy ? ["--lazy"] : []) + connectionArguments(c)
}

/// `rustfuse init`: a new volume `name` from the folder `dir`, which then
/// syncs in place.
public func initArguments(name: String, _ c: Connection, dir: URL) -> [String] {
    ["init", dir.path, "--volume", name] + connectionArguments(c)
}

/// `rustfuse volumes`: the volumes `c`'s token opens.
public func volumesArguments(_ c: Connection) -> [String] {
    ["volumes", "--json"] + connectionArguments(c)
}

/// The long-running command that keeps `v` in sync (and mounted), with
/// the socket the app and the extension talk to.
public func daemonArguments(_ v: VolumeConfig, dir: URL, socket: URL, settings: AppSettings) -> [String] {
    let cache = v.mode.isLazy ? ["--cache-size", String(v.cacheSize ?? settings.defaultCacheSize)] : []
    switch v.mode {
    case .finder, .folder:
        return ["sync", dir.path, "--provider-socket", socket.path] + cache
    case .mount:
        let fskit = settings.mountBackend == .fskit ? ["--fskit"] : []
        return ["mount", dir.path, v.path ?? "", "--provider-socket", socket.path] + cache + fskit
    }
}

/// The environment carrying `c`'s secrets to `rustfuse`.
public func toolEnvironment(base: [String: String], token: String?, s3Secret: String?, _ c: Connection?) -> [String: String] {
    var env = base
    env["RUSTFUSE_HUB_TOKEN"] = token
    if let s3 = c?.s3 {
        env["RUSTFUSE_ACCESS_KEY"] = s3.accessKey
        env["RUSTFUSE_SECRET_KEY"] = s3Secret
    }
    env["RUST_LOG"] = env["RUST_LOG"] ?? "info"
    return env
}
