import FSKit
import Foundation
import RustFuseKit

/// A file, directory or symlink of the synced directory, by inode. Its
/// path is kept current through renames; the content is read and written
/// through a descriptor opened when the item is.
final class Node: FSItem {
    /// Relative to the synced directory; empty for its root.
    var rel: String
    let ino: UInt64
    var fd: Int32 = -1
    var fdWritable = false
    /// When the daemon last heard this file is in use (see `ensureContent`).
    var lastMarked = Date.distantPast

    init(rel: String, ino: UInt64) {
        self.rel = rel
        self.ino = ino
        super.init()
    }

    func closeFile() {
        if fd >= 0 {
            close(fd)
            fd = -1
            fdWritable = false
        }
    }
}

/// A passthrough over the synced directory: every operation is done on the
/// directory itself, so the sync running beside it captures writes like any
/// other edit. Placeholders (`attach --lazy`) are filled in by the daemon,
/// asked over its provider socket, before they are opened or read. The
/// state directory `.rustfuse` is hidden.
final class RustFuseVolume: FSVolume {
    let root: String
    private let rootIno: UInt64
    private let rootNode: Node
    /// Every item the kernel holds, by inode, until it is reclaimed.
    private var nodes: [UInt64: Node] = [:]
    private let lock = NSLock()
    private let daemon: DaemonLink?

    init(root url: URL) throws {
        let rootPath = url.standardizedFileURL.path
        var st = Darwin.stat()
        guard lstat(rootPath, &st) == 0 else { throw posixError() }
        guard (st.st_mode & S_IFMT) == S_IFDIR else { throw POSIXError(.ENOTDIR) }
        let ino = UInt64(st.st_ino)
        root = rootPath
        rootIno = ino
        rootNode = Node(rel: "", ino: ino)
        daemon = DaemonLink(root: url)
        super.init(
            volumeID: FSVolume.Identifier(uuid: stableUUID(rootPath)),
            volumeName: FSFileName(string: url.lastPathComponent))
        nodes[rootIno] = rootNode
    }

    /// Runs `body` on the node table under its lock.
    func withNodes<T>(_ body: (inout [UInt64: Node]) -> T) -> T {
        lock.lock()
        defer { lock.unlock() }
        return body(&nodes)
    }

    // MARK: Paths and attributes

    func path(_ rel: String) -> String {
        rel.isEmpty ? root : root + "/" + rel
    }

    func path(of item: FSItem) throws -> String {
        guard let n = item as? Node else { throw POSIXError(.EINVAL) }
        return path(n.rel)
    }

    /// The child's relative path, refusing names that would leave the
    /// directory or reach the state directory.
    func childRel(_ dir: FSItem, _ name: FSFileName) throws -> String {
        guard let d = dir as? Node else { throw POSIXError(.EINVAL) }
        guard let s = name.string, !s.isEmpty, s != ".", s != "..", !s.contains("/") else {
            throw POSIXError(.EINVAL)
        }
        if d.rel.isEmpty && s == stateDir { throw POSIXError(.ENOENT) }
        return d.rel.isEmpty ? s : d.rel + "/" + s
    }

    func id(_ ino: UInt64) -> FSItem.Identifier {
        ino == rootIno ? .rootDirectory : (FSItem.Identifier(rawValue: ino) ?? .invalid)
    }

    /// The node for `rel` (as `lstat` found it), reusing the one the kernel
    /// already holds for that inode.
    func node(_ rel: String, _ st: Darwin.stat) -> Node {
        let ino = UInt64(st.st_ino)
        lock.lock()
        defer { lock.unlock() }
        if let n = nodes[ino] {
            n.rel = rel
            return n
        }
        let n = Node(rel: rel, ino: ino)
        nodes[ino] = n
        return n
    }

    func attributes(_ st: Darwin.stat, rel: String) -> FSItem.Attributes {
        let a = FSItem.Attributes()
        a.type = itemType(st.st_mode)
        a.mode = UInt32(st.st_mode)
        a.linkCount = UInt32(st.st_nlink)
        a.uid = st.st_uid
        a.gid = st.st_gid
        a.flags = st.st_flags
        a.size = UInt64(st.st_size)
        a.allocSize = UInt64(st.st_blocks) * 512
        a.fileID = id(UInt64(st.st_ino))
        a.parentID = parentID(rel)
        a.accessTime = st.st_atimespec
        a.modifyTime = st.st_mtimespec
        a.changeTime = st.st_ctimespec
        a.birthTime = st.st_birthtimespec
        return a
    }

    func parentID(_ rel: String) -> FSItem.Identifier {
        if rel.isEmpty { return .parentOfRoot }
        guard let slash = rel.lastIndex(of: "/") else { return .rootDirectory }
        var st = Darwin.stat()
        return lstat(path(String(rel[..<slash])), &st) == 0 ? id(UInt64(st.st_ino)) : .invalid
    }

    func info(_ rel: String) throws -> Darwin.stat {
        var st = Darwin.stat()
        guard lstat(path(rel), &st) == 0 else { throw posixError() }
        return st
    }

    // MARK: Content

    /// Before a file is opened, read or written: has the daemon fill it in
    /// if it is a placeholder, and tells it the file is in use, so it is not
    /// turned back into a placeholder while open (renewed every 30 s).
    func ensureContent(_ n: Node) throws {
        let p = path(n.rel)
        let placeholder = getxattr(p, lazyXattr, nil, 0, 0, XATTR_NOFOLLOW) >= 0
        guard placeholder || Date().timeIntervalSince(n.lastMarked) > 30 else { return }
        guard let daemon else {
            if placeholder { throw POSIXError(.EIO) }
            return
        }
        do {
            try daemon.fetch(rel: n.rel)
            n.lastMarked = Date()
        } catch {
            log.error("fetching \(n.rel, privacy: .public): \(error.localizedDescription, privacy: .public)")
            if placeholder { throw POSIXError(.EIO) }
        }
        if placeholder {
            // Filled in by the daemon: read the new content, not the old file.
            n.closeFile()
        }
    }

    func openFile(_ n: Node, write: Bool) throws {
        if n.fd >= 0 && (n.fdWritable || !write) { return }
        try ensureContent(n)
        let fd = open(path(n.rel), (write ? O_RDWR : O_RDONLY) | O_NOFOLLOW)
        guard fd >= 0 else { throw posixError() }
        n.closeFile()
        n.fd = fd
        n.fdWritable = write
    }
}

let stateDir = ".rustfuse"
let lazyXattr = "user.rustfuse.lazy"

func posixError() -> POSIXError {
    POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
}

func itemType(_ mode: mode_t) -> FSItem.ItemType {
    switch mode & S_IFMT {
    case S_IFREG: return .file
    case S_IFDIR: return .directory
    case S_IFLNK: return .symlink
    case S_IFIFO: return .fifo
    case S_IFCHR: return .charDevice
    case S_IFBLK: return .blockDevice
    case S_IFSOCK: return .socket
    default: return .unknown
    }
}

/// The volume's daemon (`rustfuse mount --native --provider-socket`), when
/// the synced directory is one of the app's volumes: `volumes/<key>` in the
/// app group container, served on `s/<key>.sock`.
final class DaemonLink: @unchecked Sendable {
    private let client: Client
    private let lock = NSLock()

    init?(root: URL) {
        let key = root.lastPathComponent
        guard let container = try? Shared.container(),
            root.standardizedFileURL.deletingLastPathComponent().path
                == container.appendingPathComponent("volumes").standardizedFileURL.path,
            let socket = try? Shared.socket(key)
        else { return nil }
        client = Client(socketPath: socket.path)
    }

    /// Fills in the file at `rel` if it is a placeholder (the protocol's
    /// path-named item id).
    func fetch(rel: String) throws {
        let id = "p" + rel.utf8.map { String(format: "%02x", $0) }.joined()
        lock.lock()
        defer { lock.unlock() }
        _ = try client.fetch(id)
    }
}
