import FileProvider
import RustFuseKit
import UniformTypeIdentifiers
import os

let log = Logger(subsystem: "dev.rustfuse", category: "fileprovider")

/// The File Provider extension for one volume (domain). It holds no sync
/// logic: every request goes to the volume's `rustfuse sync` over its
/// provider socket (docs/design.md §2.2), which applies it to the synced
/// directory like any other edit.
final class FileProviderExtension: NSObject, NSFileProviderReplicatedExtension {
    let domain: NSFileProviderDomain
    let client: Client

    required init(domain: NSFileProviderDomain) {
        self.domain = domain
        let socket = (try? Shared.socket(domain.identifier.rawValue).path) ?? ""
        client = Client(socketPath: socket)
        super.init()
    }

    func invalidate() {
        client.close()
    }

    // MARK: Reading

    func item(
        for identifier: NSFileProviderItemIdentifier, request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, Error?) -> Void
    ) -> Progress {
        run({ completionHandler(nil, $0) }) { [client] in
            completionHandler(ProviderItem(try client.item(identifier.wire)), nil)
        }
    }

    func fetchContents(
        for itemIdentifier: NSFileProviderItemIdentifier, version requestedVersion: NSFileProviderItemVersion?,
        request: NSFileProviderRequest, completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void
    ) -> Progress {
        run({ completionHandler(nil, nil, $0) }) { [client, domain] in
            let id = itemIdentifier.wire
            // The content and the version handed back must match: if the
            // file changes while it is copied, copy it again.
            for _ in 0..<3 {
                let path = try client.fetch(id)
                let before = try client.item(id)
                let tmp = try Self.temporaryFile(domain)
                try cloneFile(URL(fileURLWithPath: path), to: tmp)
                let after = try client.item(id)
                if after.contentVersion == before.contentVersion {
                    completionHandler(tmp, ProviderItem(after), nil)
                    return
                }
                try? FileManager.default.removeItem(at: tmp)
            }
            throw CocoaError(.fileReadUnknown, userInfo: [NSLocalizedDescriptionKey: "\(id) keeps changing"])
        }
    }

    func enumerator(
        for containerItemIdentifier: NSFileProviderItemIdentifier, request: NSFileProviderRequest
    ) throws -> NSFileProviderEnumerator {
        switch containerItemIdentifier {
        case .workingSet:
            return WorkingSetEnumerator(client: client)
        case .trashContainer:
            // Deleting goes to the user's Trash, outside the volume.
            throw NSFileProviderError(.noSuchItem)
        default:
            return FolderEnumerator(client: client, id: containerItemIdentifier.wire)
        }
    }

    // MARK: Changing

    func createItem(
        basedOn itemTemplate: NSFileProviderItem, fields: NSFileProviderItemFields, contents url: URL?,
        options: NSFileProviderCreateItemOptions = [], request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
    ) -> Progress {
        run({ completionHandler(nil, [], false, $0) }) { [client] in
            let type = itemTemplate.contentType ?? .data
            let kind: Kind =
                type.conforms(to: .symbolicLink) ? .symlink : type.conforms(to: .directory) ? .dir : .file
            let parent = itemTemplate.parentItemIdentifier.wire
            let name = itemTemplate.filename
            var mode: UInt32?
            if fields.contains(.fileSystemFlags), let flags = itemTemplate.fileSystemFlags {
                mode = permissionBits(kind == .dir ? 0o755 : 0o644, with: flags)
            }
            var mtime: Int64?
            if fields.contains(.contentModificationDate), let d = itemTemplate.contentModificationDate ?? nil {
                mtime = nanoseconds(d)
            }
            let staged = try kind == .file ? url.map(Self.stage) : nil
            do {
                let target: String? = kind == .symlink ? (itemTemplate.symlinkTargetPath ?? nil) : nil
                let item = try client.change(
                    .create(
                        parent: parent, name: name, kind: kind, source: staged?.path, target: target, mode: mode,
                        mtimeNs: mtime))
                completionHandler(ProviderItem(item), [], false, nil)
            } catch ProviderError.daemon(.exists, _) where options.contains(.mayAlreadyExist) {
                // A re-import: the item is there already.
                if let staged { try? FileManager.default.removeItem(at: staged) }
                guard let existing = try Self.child(client, of: parent, named: name) else {
                    throw NSFileProviderError(.filenameCollision)
                }
                completionHandler(ProviderItem(existing), [], false, nil)
            } catch {
                if let staged { try? FileManager.default.removeItem(at: staged) }
                throw error
            }
        }
    }

    func modifyItem(
        _ item: NSFileProviderItem, baseVersion version: NSFileProviderItemVersion,
        changedFields: NSFileProviderItemFields, contents newContents: URL?,
        options: NSFileProviderModifyItemOptions = [], request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
    ) -> Progress {
        run({ completionHandler(nil, [], false, $0) }) { [client] in
            let id = item.itemIdentifier.wire
            var parent: String?
            var name: String?
            var mode: UInt32?
            var mtime: Int64?
            if changedFields.contains(.parentItemIdentifier) {
                parent = item.parentItemIdentifier.wire
            }
            if changedFields.contains(.filename) {
                name = item.filename
            }
            if changedFields.contains(.fileSystemFlags), let flags = item.fileSystemFlags {
                let current = try client.item(id).mode
                mode = permissionBits(current, with: flags)
            }
            if changedFields.contains(.contentModificationDate), let d = item.contentModificationDate ?? nil {
                mtime = nanoseconds(d)
            }
            let staged = try changedFields.contains(.contents) ? newContents.map(Self.stage) : nil
            // Other fields (tags, last used, extended attributes) are not
            // synced; they are accepted and dropped.
            if parent == nil, name == nil, mode == nil, mtime == nil, staged == nil {
                completionHandler(ProviderItem(try client.item(id)), [], false, nil)
                return
            }
            do {
                let updated = try client.change(
                    .modify(id: id, parent: parent, name: name, source: staged?.path, mode: mode, mtimeNs: mtime))
                completionHandler(ProviderItem(updated), [], false, nil)
            } catch {
                if let staged { try? FileManager.default.removeItem(at: staged) }
                throw error
            }
        }
    }

    func deleteItem(
        identifier: NSFileProviderItemIdentifier, baseVersion version: NSFileProviderItemVersion,
        options: NSFileProviderDeleteItemOptions = [], request: NSFileProviderRequest,
        completionHandler: @escaping (Error?) -> Void
    ) -> Progress {
        run({ completionHandler($0) }) { [client] in
            do {
                try client.delete(identifier.wire, recursive: options.contains(.recursive))
            } catch ProviderError.daemon(.noSuchItem, _) {
                // Already gone.
            }
            completionHandler(nil)
        }
    }

    // MARK: Helpers

    /// Runs `work` off the caller's thread; a thrown error goes to `fail`.
    private func run(_ fail: @escaping (Error) -> Void, _ work: @escaping () throws -> Void) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                try work()
            } catch {
                log.error("request failed: \(String(describing: error), privacy: .public)")
                fail(fileProviderError(error))
            }
            progress.completedUnitCount = 1
        }
        return progress
    }

    /// Copies new content where the daemon can move it into the volume.
    /// macOS keeps `url`, so it is copied (cloned) rather than moved.
    private static func stage(_ url: URL) throws -> URL {
        let dst = try Shared.staging().appendingPathComponent(UUID().uuidString)
        try cloneFile(url, to: dst)
        return dst
    }

    private static func temporaryFile(_ domain: NSFileProviderDomain) throws -> URL {
        guard let manager = NSFileProviderManager(for: domain) else {
            throw NSFileProviderError(.providerNotFound)
        }
        return try manager.temporaryDirectoryURL().appendingPathComponent(UUID().uuidString)
    }

    private static func child(_ client: Client, of parent: String, named name: String) throws -> Item? {
        var after: String?
        repeat {
            let page = try client.list(parent, after: after)
            if let hit = page.items.first(where: { $0.name == name }) {
                return hit
            }
            after = page.next
        } while after != nil
        return nil
    }
}
