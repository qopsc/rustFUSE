import FileProvider
import RustFuseKit

/// Most items handed to macOS in one call.
private let batch = 200

/// A directory's children, a page at a time.
final class FolderEnumerator: NSObject, NSFileProviderEnumerator {
    let client: Client
    let id: String

    init(client: Client, id: String) {
        self.client = client
        self.id = id
    }

    func invalidate() {}

    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        // Observers may be called from any thread; see `run` in
        // FileProviderExtension.swift.
        nonisolated(unsafe) let observer = observer
        let after = Self.after(page)
        DispatchQueue.global(qos: .userInitiated).async { [client, id] in
            do {
                let (items, next) = try client.list(id, after: after)
                observer.didEnumerate(items.map(ProviderItem.init))
                observer.finishEnumerating(upTo: next.map(Self.page))
            } catch {
                observer.finishEnumeratingWithError(fileProviderError(error))
            }
        }
    }

    /// Changes come through the working set (a replicated extension's
    /// directory enumerators report none).
    func enumerateChanges(for observer: NSFileProviderChangeObserver, from anchor: NSFileProviderSyncAnchor) {
        observer.finishEnumeratingChanges(upTo: anchor, moreComing: false)
    }

    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        currentAnchor(client, completionHandler)
    }

    // Our pages carry the last name listed; macOS's initial pages do not.
    private static let prefix = "after:"

    private static func after(_ page: NSFileProviderPage) -> String? {
        guard let s = String(data: page.rawValue, encoding: .utf8), s.hasPrefix(prefix) else { return nil }
        return String(s.dropFirst(prefix.count))
    }

    private static func page(_ after: String) -> NSFileProviderPage {
        NSFileProviderPage(Data((prefix + after).utf8))
    }
}

/// Everything that changed in the volume, from the daemon's change journal.
/// The app signals it whenever the journal moves.
final class WorkingSetEnumerator: NSObject, NSFileProviderEnumerator {
    let client: Client

    init(client: Client) {
        self.client = client
    }

    func invalidate() {}

    /// Items reach macOS through their directories as they are browsed;
    /// the working set starts empty and is kept current by changes.
    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        observer.finishEnumerating(upTo: nil)
    }

    func enumerateChanges(for observer: NSFileProviderChangeObserver, from anchor: NSFileProviderSyncAnchor) {
        nonisolated(unsafe) let observer = observer
        let since = String(decoding: anchor.rawValue, as: UTF8.self)
        DispatchQueue.global(qos: .userInitiated).async { [client] in
            do {
                let (updated, deleted, next) = try client.changes(since: since)
                for start in stride(from: 0, to: updated.count, by: batch) {
                    observer.didUpdate(updated[start..<min(start + batch, updated.count)].map(ProviderItem.init))
                }
                for start in stride(from: 0, to: deleted.count, by: batch) {
                    observer.didDeleteItems(
                        withIdentifiers: deleted[start..<min(start + batch, deleted.count)].map {
                            NSFileProviderItemIdentifier(wire: $0)
                        })
                }
                observer.finishEnumeratingChanges(upTo: NSFileProviderSyncAnchor(Data(next.utf8)), moreComing: false)
            } catch {
                observer.finishEnumeratingWithError(fileProviderError(error))
            }
        }
    }

    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        currentAnchor(client, completionHandler)
    }
}

private func currentAnchor(_ client: Client, _ done: @escaping (NSFileProviderSyncAnchor?) -> Void) {
    nonisolated(unsafe) let done = done
    DispatchQueue.global(qos: .userInitiated).async {
        done((try? client.anchor()).map { NSFileProviderSyncAnchor(Data($0.utf8)) })
    }
}
