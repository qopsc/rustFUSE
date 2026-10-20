import FileProvider
import RustFuseKit
import UniformTypeIdentifiers

extension NSFileProviderItemIdentifier {
    init(wire: String) {
        self = wire == rootID ? .rootContainer : NSFileProviderItemIdentifier(wire)
    }

    var wire: String {
        self == .rootContainer ? rootID : rawValue
    }
}

/// An item from the daemon, as macOS sees it.
final class ProviderItem: NSObject, NSFileProviderItem {
    let item: Item

    init(_ item: Item) {
        self.item = item
    }

    var itemIdentifier: NSFileProviderItemIdentifier { .init(wire: item.id) }
    var parentItemIdentifier: NSFileProviderItemIdentifier { .init(wire: item.parent) }
    var filename: String { item.name }

    var contentType: UTType {
        switch item.kind {
        case .dir: return .folder
        case .symlink: return .symbolicLink
        case .file:
            let ext = (item.name as NSString).pathExtension
            return ext.isEmpty ? .data : UTType(filenameExtension: ext) ?? .data
        }
    }

    var documentSize: NSNumber? { item.kind == .file ? NSNumber(value: item.size) : nil }
    var contentModificationDate: Date? { date(nanoseconds: item.mtimeNs) }
    var symlinkTargetPath: String? { item.target }

    var itemVersion: NSFileProviderItemVersion {
        NSFileProviderItemVersion(
            contentVersion: Data(item.contentVersion.utf8), metadataVersion: Data(item.metaVersion.utf8))
    }

    var capabilities: NSFileProviderItemCapabilities {
        var c: NSFileProviderItemCapabilities = [.allowsReading, .allowsWriting]
        if item.id != rootID {
            c.formUnion([.allowsRenaming, .allowsReparenting, .allowsDeleting])
        }
        if item.kind == .dir {
            c.formUnion([.allowsContentEnumerating, .allowsAddingSubItems])
        }
        return c
    }

    var fileSystemFlags: NSFileProviderFileSystemFlags {
        var f: NSFileProviderFileSystemFlags = []
        if item.mode & 0o400 != 0 { f.insert(.userReadable) }
        if item.mode & 0o200 != 0 { f.insert(.userWritable) }
        if item.mode & 0o100 != 0 { f.insert(.userExecutable) }
        return f
    }

    /// Git reads its own data constantly and expects it local: keep `.git`
    /// (a repository's directory, or a worktree's pointer file) and
    /// everything under it downloaded. Everything else downloads on first
    /// open.
    var contentPolicy: NSFileProviderContentPolicy {
        item.name == ".git" ? .downloadEagerlyAndKeepDownloaded : .inherited
    }
}

/// The permission bits for `flags`, keeping `mode`'s group and other bits
/// (macOS only shows the provider the owner's).
func permissionBits(_ mode: UInt32, with flags: NSFileProviderFileSystemFlags) -> UInt32 {
    var m = mode & ~0o700
    if flags.contains(.userReadable) { m |= 0o400 }
    if flags.contains(.userWritable) { m |= 0o200 }
    if flags.contains(.userExecutable) { m |= 0o100 }
    return m
}
