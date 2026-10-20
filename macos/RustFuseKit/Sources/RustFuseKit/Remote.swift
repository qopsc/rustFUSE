import Foundation

// Reading volumes straight from the bucket, without a synced directory:
// `rustfuse ls --json` and `rustfuse get`. Tests/RustFuseKitTests/wire.json
// pins the listing format on both sides.

/// One folder of a volume as the bucket holds it (`rustfuse ls --json`).
public struct RemoteListing: Codable, Equatable, Sendable {
    /// The volume's changeset the listing was read at.
    public var seq: UInt64
    public var entries: [RemoteEntry]
}

/// A file, folder or symlink in a remote listing.
public struct RemoteEntry: Codable, Equatable, Hashable, Identifiable, Sendable {
    public var id: String { name }
    public var name: String
    public var kind: Kind
    public var size: UInt64
    /// Permission bits.
    public var mode: UInt32
    public var mtimeNs: Int64
    /// Symlink target.
    public var target: String?

    enum CodingKeys: String, CodingKey {
        case name, kind, size, mode, target
        case mtimeNs = "mtime_ns"
    }
}

/// `rustfuse ls`: the folder `path` of volume `volume` (the top when empty).
public func lsArguments(volume: String, path: String, _ c: Connection) -> [String] {
    ["ls", "--volume", volume] + (path.isEmpty ? [] : [path]) + ["--json"] + connectionArguments(c)
}

/// `rustfuse get`: downloads the file or folder `path` of `volume` to
/// `dest`, which must not exist yet.
public func getArguments(volume: String, path: String, dest: URL, _ c: Connection) -> [String] {
    ["get", "--volume", volume, path, dest.path] + connectionArguments(c)
}

/// Whether `name` is a conflict copy: the losing side of an edit made on
/// two machines at once, named `<stem>.conflict-<replica>-<UTC time>[.ext]`.
/// Matches the daemon's test (`original_name` in
/// crates/rustfuse-sync/src/git.rs): the last `.conflict-` tag, not at the
/// start of the name.
public func isConflictCopy(_ name: String) -> Bool {
    guard let tag = name.range(of: ".conflict-", options: .backwards), tag.lowerBound > name.startIndex else {
        return false
    }
    let rest = Array(name[tag.upperBound...].utf8.prefix(24))
    guard rest.count == 24 else { return false }
    let hex = rest[0..<8].allSatisfy { $0 < 0x80 && Character(Unicode.Scalar($0)).isHexDigit }
    let digits = { (r: ArraySlice<UInt8>) in r.allSatisfy { $0 >= UInt8(ascii: "0") && $0 <= UInt8(ascii: "9") } }
    return hex && rest[8] == UInt8(ascii: "-") && digits(rest[9..<17]) && rest[17] == UInt8(ascii: "-")
        && digits(rest[18..<24])
}

/// What `rustfuse conflicts --json` reports for a synced directory.
public struct ConflictReport: Codable, Equatable, Sendable {
    /// Conflict copies of ordinary files, relative to the volume's top.
    public var files: [String]
    /// Branches that diverged on two machines, kept as their own ref.
    public var branches: [DivergedBranch]
}

public struct DivergedBranch: Codable, Equatable, Hashable, Sendable {
    /// The repository's folder in the volume, `.` for the top.
    public var repo: String
    /// The other machine's side, to merge with `git merge <branch>`.
    public var branch: String
}

/// `rustfuse conflicts --json` for the synced directory `dir`.
public func conflictsArguments(dir: URL) -> [String] {
    ["conflicts", dir.path, "--json"]
}
