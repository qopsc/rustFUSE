import CryptoKit
import FSKit
import Foundation
import os

let log = Logger(subsystem: "dev.rustfuse", category: "fskit")

/// The FSKit module behind `rustfuse mount --native` (docs/design.md §2.1):
/// `mount -t rustfuse <synced dir> <mount point>` presents the synced
/// directory as its own file system, without macFUSE.
@main
struct RustFuseFSExtension: UnaryFileSystemExtension {
    var fileSystem: FSUnaryFileSystem & FSUnaryFileSystemOperations {
        RustFuseFileSystem.shared
    }
}

/// One resource (a synced directory) and its one volume.
final class RustFuseFileSystem: FSUnaryFileSystem, FSUnaryFileSystemOperations {
    static let shared = RustFuseFileSystem()

    private var resource: FSPathURLResource?

    func probeResource(resource: FSResource, replyHandler: @escaping (FSProbeResult?, (any Error)?) -> Void) {
        guard let r = resource as? FSPathURLResource else {
            return replyHandler(FSProbeResult.notRecognized, nil)
        }
        replyHandler(
            FSProbeResult.usable(name: r.url.lastPathComponent, containerID: FSContainerIdentifier(uuid: stableUUID(r.url.path))),
            nil)
    }

    func loadResource(
        resource: FSResource, options: FSTaskOptions, replyHandler: @escaping (FSVolume?, (any Error)?) -> Void
    ) {
        guard let r = resource as? FSPathURLResource else {
            return replyHandler(nil, POSIXError(.EINVAL))
        }
        // Security-scoped when a sandboxed client passed it; a plain path
        // from `mount` needs no scope.
        let scoped = r.url.startAccessingSecurityScopedResource()
        do {
            let volume = try RustFuseVolume(root: r.url)
            self.resource = r
            containerStatus = .ready
            log.info("serving \(r.url.path, privacy: .public)")
            replyHandler(volume, nil)
        } catch {
            if scoped { r.url.stopAccessingSecurityScopedResource() }
            replyHandler(nil, error)
        }
    }

    func unloadResource(resource: FSResource, options: FSTaskOptions, replyHandler reply: @escaping ((any Error)?) -> Void) {
        if let loaded = self.resource, (resource as? FSPathURLResource)?.url == loaded.url {
            loaded.url.stopAccessingSecurityScopedResource()
        }
        self.resource = nil
        reply(nil)
    }
}

/// The same UUID for the same path every time, so the system sees one
/// container and volume per synced directory.
func stableUUID(_ path: String) -> UUID {
    var bytes = Array(Insecure.MD5.hash(data: Data(path.utf8)))
    bytes[6] = (bytes[6] & 0x0f) | 0x30  // version 3: name-based (MD5)
    bytes[8] = (bytes[8] & 0x3f) | 0x80
    return UUID(
        uuid: (
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
        ))
}
