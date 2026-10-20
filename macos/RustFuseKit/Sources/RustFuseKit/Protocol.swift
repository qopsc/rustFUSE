import Foundation

// The File Provider protocol of `rustfuse sync --provider-socket`
// (crates/rustfuse-sync/src/provider.rs): one JSON request per line,
// answered by one JSON line. Tests/RustFuseKitTests/wire.json pins the
// format on both sides.

/// The root directory's item id.
public let rootID = "root"

public enum Kind: String, Codable, Sendable {
    case file, dir, symlink
}

/// A file, directory or symlink in the volume.
public struct Item: Codable, Equatable, Sendable {
    public var id: String
    public var parent: String
    public var name: String
    public var kind: Kind
    public var size: UInt64
    /// Permission bits.
    public var mode: UInt32
    public var mtimeNs: Int64
    /// Symlink target.
    public var target: String?
    /// Changes whenever the content does.
    public var contentVersion: String
    /// Changes whenever the name, place or permissions do.
    public var metaVersion: String
    /// Whether the content is on this machine (not a placeholder).
    public var downloaded: Bool
    /// Whether the volume holds it; machine-local entries are not synced.
    public var synced: Bool

    enum CodingKeys: String, CodingKey {
        case id, parent, name, kind, size, mode, target, downloaded, synced
        case mtimeNs = "mtime_ns"
        case contentVersion = "content_version"
        case metaVersion = "meta_version"
    }
}

public enum Request: Equatable, Sendable {
    /// One item.
    case item(id: String)
    /// A directory's children, by name, after `after`.
    case list(id: String, after: String? = nil)
    /// The current position of the change journal.
    case anchor
    /// What changed since `anchor`.
    case changes(anchor: String)
    /// The item's content on disk, fetched first if it is a placeholder.
    case fetch(id: String)
    /// A new item in `parent`. Files take their content from `source`
    /// (moved into place), symlinks their target from `target`.
    case create(
        parent: String, name: String, kind: Kind, source: String? = nil, target: String? = nil,
        mode: UInt32? = nil, mtimeNs: Int64? = nil)
    /// Moves an item, replaces its content with `source`, or sets its
    /// permissions or time. Unset fields stay.
    case modify(
        id: String, parent: String? = nil, name: String? = nil, source: String? = nil,
        mode: UInt32? = nil, mtimeNs: Int64? = nil)
    /// Deletes an item; a directory only if empty unless `recursive`.
    case delete(id: String, recursive: Bool = false)

    /// Whether sending it twice is harmless.
    public var isReadOnly: Bool {
        switch self {
        case .item, .list, .anchor, .changes, .fetch: return true
        case .create, .modify, .delete: return false
        }
    }
}

extension Request: Encodable {
    // Unset optionals are left out, as the daemon writes them.
    private struct Wire: Encodable {
        var op: String
        var id: String?
        var after: String?
        var anchor: String?
        var parent: String?
        var name: String?
        var kind: Kind?
        var source: String?
        var target: String?
        var mode: UInt32?
        var mtimeNs: Int64?
        var recursive: Bool?

        enum CodingKeys: String, CodingKey {
            case op, id, after, anchor, parent, name, kind, source, target, mode, recursive
            case mtimeNs = "mtime_ns"
        }
    }

    public func encode(to encoder: Encoder) throws {
        var w: Wire
        switch self {
        case .item(let id):
            w = Wire(op: "item", id: id)
        case .list(let id, let after):
            w = Wire(op: "list", id: id, after: after)
        case .anchor:
            w = Wire(op: "anchor")
        case .changes(let anchor):
            w = Wire(op: "changes", anchor: anchor)
        case .fetch(let id):
            w = Wire(op: "fetch", id: id)
        case .create(let parent, let name, let kind, let source, let target, let mode, let mtimeNs):
            w = Wire(
                op: "create", parent: parent, name: name, kind: kind, source: source, target: target,
                mode: mode, mtimeNs: mtimeNs)
        case .modify(let id, let parent, let name, let source, let mode, let mtimeNs):
            w = Wire(op: "modify", id: id, parent: parent, name: name, source: source, mode: mode, mtimeNs: mtimeNs)
        case .delete(let id, let recursive):
            w = Wire(op: "delete", id: id, recursive: recursive)
        }
        try w.encode(to: encoder)
    }
}

public enum ErrorCode: String, Sendable {
    case noSuchItem = "no_such_item"
    case exists
    case notEmpty = "not_empty"
    /// The anchor is from an earlier run: enumerate everything again.
    case anchorExpired = "anchor_expired"
    case badRequest = "bad_request"
    case io
}

public enum Response: Equatable, Sendable {
    case item(Item)
    /// `next` is passed as `after` for the next page; nil on the last.
    case list(items: [Item], next: String?)
    case anchor(String)
    case changes(updated: [Item], deleted: [String], anchor: String)
    case path(String)
    case done
    case error(ErrorCode, String)
}

extension Response: Decodable {
    private struct Wire: Decodable {
        var result: String
        var item: Item?
        var items: [Item]?
        var next: String?
        var anchor: String?
        var updated: [Item]?
        var deleted: [String]?
        var path: String?
        var code: String?
        var message: String?
    }

    public init(from decoder: Decoder) throws {
        let w = try Wire(from: decoder)
        func need<T>(_ v: T?, _ field: String) throws -> T {
            guard let v else {
                throw DecodingError.dataCorrupted(
                    .init(codingPath: decoder.codingPath, debugDescription: "\(w.result) without \(field)"))
            }
            return v
        }
        switch w.result {
        case "item": self = .item(try need(w.item, "item"))
        case "list": self = .list(items: try need(w.items, "items"), next: w.next)
        case "anchor": self = .anchor(try need(w.anchor, "anchor"))
        case "changes":
            self = .changes(
                updated: try need(w.updated, "updated"), deleted: try need(w.deleted, "deleted"),
                anchor: try need(w.anchor, "anchor"))
        case "path": self = .path(try need(w.path, "path"))
        case "done": self = .done
        case "error":
            // A code this version does not know is still an error.
            self = .error(ErrorCode(rawValue: w.code ?? "") ?? .io, w.message ?? "")
        default:
            throw DecodingError.dataCorrupted(
                .init(codingPath: decoder.codingPath, debugDescription: "unknown result \(w.result)"))
        }
    }
}
