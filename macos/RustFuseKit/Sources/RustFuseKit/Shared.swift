import Darwin
import Foundation

/// Where the app, its daemons and the File Provider extension keep things:
/// the app group container both the app and the sandboxed extension can
/// reach.
///
///     volumes/<name>/   each volume's synced directory (`rustfuse attach --lazy`)
///     s/<name>.sock     its daemon's provider socket
///     staging/          new content on its way into a volume
///     logs/<name>.log   its daemon's log
///     volumes.json      the volumes the app runs
public enum Shared {
    /// The app group, from the bundle's `RustFuseAppGroup` Info.plist key
    /// (set by the build to the team prefix plus the bundle prefix).
    public static var appGroup: String {
        Bundle.main.object(forInfoDictionaryKey: "RustFuseAppGroup") as? String ?? "dev.rustfuse"
    }

    public static func container() throws -> URL {
        guard let url = FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: appGroup) else {
            throw CocoaError(.fileNoSuchFile, userInfo: [NSLocalizedDescriptionKey: "no app group container for \(appGroup)"])
        }
        return url
    }

    public static func volumeDir(_ name: String) throws -> URL {
        try dir("volumes").appendingPathComponent(name, isDirectory: true)
    }

    /// Kept short: socket paths have a 103-byte limit.
    public static func socket(_ name: String) throws -> URL {
        try dir("s").appendingPathComponent("\(name).sock")
    }

    public static func staging() throws -> URL {
        try dir("staging")
    }

    public static func log(_ name: String) throws -> URL {
        try dir("logs").appendingPathComponent("\(name).log")
    }

    public static func volumesFile() throws -> URL {
        try container().appendingPathComponent("volumes.json")
    }

    /// Volume names double as File Provider domain ids and file names.
    public static func validName(_ name: String) -> Bool {
        let allowed = Set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-")
        return (1...40).contains(name.count) && !name.hasPrefix(".") && name.allSatisfy(allowed.contains)
    }

    private static func dir(_ sub: String) throws -> URL {
        let url = try container().appendingPathComponent(sub, isDirectory: true)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }
}

/// A volume the app runs: its name (also the File Provider domain) and hub.
public struct Volume: Codable, Equatable, Identifiable, Sendable {
    public var name: String
    public var hub: String
    public var id: String { name }

    public init(name: String, hub: String) {
        self.name = name
        self.hub = hub
    }
}

/// Copies `src` to the new file `dst`, as an APFS clone where it can (the
/// two then share blocks until one changes).
public func cloneFile(_ src: URL, to dst: URL) throws {
    if clonefile(src.path, dst.path, UInt32(CLONE_NOFOLLOW)) == 0 {
        return
    }
    switch errno {
    case ENOTSUP, EXDEV:
        try FileManager.default.copyItem(at: src, to: dst)
    default:
        throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
    }
}

/// Times on the wire are nanoseconds since 1970.
public func nanoseconds(_ date: Date) -> Int64 {
    Int64((date.timeIntervalSince1970 * 1e9).rounded())
}

public func date(nanoseconds ns: Int64) -> Date {
    Date(timeIntervalSince1970: TimeInterval(ns) / 1e9)
}
