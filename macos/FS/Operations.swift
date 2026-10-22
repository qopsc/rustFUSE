import FSKit
import Foundation

extension RustFuseVolume: FSVolume.PathConfOperations {
    var maximumLinkCount: Int { Int(LINK_MAX) }
    var maximumNameLength: Int { Int(NAME_MAX) }
    var restrictsOwnershipChanges: Bool { true }
    var truncatesLongNames: Bool { false }
    var maximumXattrSize: Int { 0 }
    var maximumFileSize: UInt64 { UInt64(Int64.max) }
}

extension RustFuseVolume: FSVolume.Operations {
    var supportedVolumeCapabilities: FSVolume.SupportedCapabilities {
        let c = FSVolume.SupportedCapabilities()
        c.supportsSymbolicLinks = true
        c.supportsPersistentObjectIDs = true
        c.supports64BitObjectIDs = true
        c.supportsHiddenFiles = true
        c.caseFormat = pathconf(root, _PC_CASE_SENSITIVE) == 1 ? .sensitive : .insensitiveCasePreserving
        return c
    }

    var volumeStatistics: FSStatFSResult {
        let r = FSStatFSResult(fileSystemTypeName: "rustfuse")
        var fs = statfs()
        if statfs(root, &fs) == 0 {
            r.blockSize = Int(fs.f_bsize)
            r.ioSize = Int(fs.f_iosize)
            r.totalBlocks = UInt64(fs.f_blocks)
            r.availableBlocks = UInt64(fs.f_bavail)
            r.freeBlocks = UInt64(fs.f_bfree)
            r.totalFiles = UInt64(fs.f_files)
            r.freeFiles = UInt64(fs.f_ffree)
        }
        return r
    }

    func activate(options: FSTaskOptions) async throws -> FSItem {
        try node("", info(""))
    }

    func deactivate(options: FSDeactivateOptions = []) async throws {}

    func mount(options: FSTaskOptions) async throws {}

    func unmount() async {}

    func synchronize(flags: FSSyncFlags) async throws {}

    func attributes(_ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem) async throws -> FSItem.Attributes {
        guard let n = item as? Node else { throw POSIXError(.EINVAL) }
        return attributes(try info(n.rel), rel: n.rel)
    }

    func setAttributes(_ newAttributes: FSItem.SetAttributesRequest, on item: FSItem) async throws -> FSItem.Attributes {
        guard let n = item as? Node else { throw POSIXError(.EINVAL) }
        let p = path(n.rel)
        var consumed: FSItem.Attribute = []
        if newAttributes.isValid(.size) {
            // Shortening a placeholder would keep zeros, not its content.
            if newAttributes.size > 0 { try ensureContent(n) }
            guard truncate(p, off_t(newAttributes.size)) == 0 else { throw posixError() }
            consumed.insert(.size)
        }
        if newAttributes.isValid(.mode) {
            guard fchmodat(AT_FDCWD, p, mode_t(newAttributes.mode & 0o7777), AT_SYMLINK_NOFOLLOW) == 0 else {
                throw posixError()
            }
            consumed.insert(.mode)
        }
        if newAttributes.isValid(.uid) || newAttributes.isValid(.gid) {
            let uid = newAttributes.isValid(.uid) ? newAttributes.uid : UInt32.max
            let gid = newAttributes.isValid(.gid) ? newAttributes.gid : UInt32.max
            guard lchown(p, uid, gid) == 0 else { throw posixError() }
            if newAttributes.isValid(.uid) { consumed.insert(.uid) }
            if newAttributes.isValid(.gid) { consumed.insert(.gid) }
        }
        if newAttributes.isValid(.flags) {
            guard lchflags(p, newAttributes.flags) == 0 else { throw posixError() }
            consumed.insert(.flags)
        }
        if newAttributes.isValid(.accessTime) || newAttributes.isValid(.modifyTime) {
            let omit = timespec(tv_sec: 0, tv_nsec: Int(UTIME_OMIT))
            var times = [
                newAttributes.isValid(.accessTime) ? newAttributes.accessTime : omit,
                newAttributes.isValid(.modifyTime) ? newAttributes.modifyTime : omit,
            ]
            guard utimensat(AT_FDCWD, p, &times, AT_SYMLINK_NOFOLLOW) == 0 else { throw posixError() }
            if newAttributes.isValid(.accessTime) { consumed.insert(.accessTime) }
            if newAttributes.isValid(.modifyTime) { consumed.insert(.modifyTime) }
        }
        newAttributes.consumedAttributes = consumed
        return attributes(try info(n.rel), rel: n.rel)
    }

    func lookupItem(named name: FSFileName, inDirectory directory: FSItem) async throws -> (FSItem, FSFileName) {
        let rel = try childRel(directory, name)
        return (node(rel, try info(rel)), name)
    }

    func reclaimItem(_ item: FSItem) async throws {
        guard let n = item as? Node else { return }
        n.closeFile()
        forget(n)
    }

    func readSymbolicLink(_ item: FSItem) async throws -> FSFileName {
        var buf = [CChar](repeating: 0, count: Int(PATH_MAX) + 1)
        let len = readlink(try path(of: item), &buf, Int(PATH_MAX))
        guard len >= 0 else { throw posixError() }
        return FSFileName(data: Data(bytes: buf, count: len))
    }

    func createItem(
        named name: FSFileName, type: FSItem.ItemType, inDirectory directory: FSItem,
        attributes newAttributes: FSItem.SetAttributesRequest
    ) async throws -> (FSItem, FSFileName) {
        let rel = try childRel(directory, name)
        let p = path(rel)
        switch type {
        case .file:
            let mode = newAttributes.isValid(.mode) ? mode_t(newAttributes.mode & 0o7777) : 0o644
            let fd = open(p, O_CREAT | O_EXCL | O_WRONLY | O_NOFOLLOW, mode)
            guard fd >= 0 else { throw posixError() }
            close(fd)
        case .directory:
            let mode = newAttributes.isValid(.mode) ? mode_t(newAttributes.mode & 0o7777) : 0o755
            guard mkdir(p, mode) == 0 else { throw posixError() }
        default:
            throw POSIXError(.ENOTSUP)
        }
        newAttributes.consumedAttributes = newAttributes.isValid(.mode) ? .mode : []
        return (node(rel, try info(rel)), name)
    }

    func createSymbolicLink(
        named name: FSFileName, inDirectory directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest,
        linkContents contents: FSFileName
    ) async throws -> (FSItem, FSFileName) {
        let rel = try childRel(directory, name)
        guard let target = contents.string else { throw POSIXError(.EINVAL) }
        guard symlink(target, path(rel)) == 0 else { throw posixError() }
        return (node(rel, try info(rel)), name)
    }

    func createLink(to item: FSItem, named name: FSFileName, inDirectory directory: FSItem) async throws -> FSFileName {
        // The volume keeps hard links as separate files; refuse rather than
        // sync something else than what was asked.
        throw POSIXError(.ENOTSUP)
    }

    func removeItem(_ item: FSItem, named name: FSFileName, fromDirectory directory: FSItem) async throws {
        let rel = try childRel(directory, name)
        let st = try info(rel)
        let p = path(rel)
        guard ((st.st_mode & S_IFMT) == S_IFDIR ? rmdir(p) : unlink(p)) == 0 else { throw posixError() }
    }

    func renameItem(
        _ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName,
        to destinationName: FSFileName, inDirectory destinationDirectory: FSItem, overItem: FSItem?
    ) async throws -> FSFileName {
        let from = try childRel(sourceDirectory, sourceName)
        let to = try childRel(destinationDirectory, destinationName)
        guard rename(path(from), path(to)) == 0 else { throw posixError() }
        moved(from: from, to: to)
        return destinationName
    }

    func enumerateDirectory(
        _ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier,
        attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker
    ) async throws -> FSDirectoryVerifier {
        guard let d = directory as? Node else { throw POSIXError(.EINVAL) }
        let dirStat = try info(d.rel)
        var entries: [(name: String, ino: UInt64, type: FSItem.ItemType)] = []
        if attributes == nil {
            // Asked without attributes, the listing includes "." and "..".
            entries.append((".", UInt64(dirStat.st_ino), .directory))
            entries.append(("..", 0, .directory))
        }
        guard let dir = opendir(path(d.rel)) else { throw posixError() }
        defer { closedir(dir) }
        while let ent = readdir(dir) {
            let name = withUnsafeBytes(of: ent.pointee.d_name) { raw in
                String(decoding: raw.prefix(Int(ent.pointee.d_namlen)), as: UTF8.self)
            }
            if name == "." || name == ".." || (d.rel.isEmpty && name == stateDir) { continue }
            let type: FSItem.ItemType =
                switch Int32(ent.pointee.d_type) {
                case DT_DIR: .directory
                case DT_LNK: .symlink
                case DT_REG: .file
                default: .unknown
                }
            entries.append((name, UInt64(ent.pointee.d_ino), type))
        }
        let start = Int(cookie.rawValue)
        guard start <= entries.count else { throw POSIXError(.EINVAL) }
        for i in start..<entries.count {
            let e = entries[i]
            let rel = e.name == "." || e.name == ".." ? d.rel : (d.rel.isEmpty ? e.name : d.rel + "/" + e.name)
            var attrs: FSItem.Attributes?
            if attributes != nil, let st = try? info(rel) {
                attrs = self.attributes(st, rel: rel)
            }
            let packed = packer.packEntry(
                name: FSFileName(string: e.name),
                itemType: attrs?.type ?? e.type,
                itemID: e.name == ".." ? (d.rel.isEmpty ? .rootDirectory : parentID(d.rel)) : id(e.ino),
                nextCookie: FSDirectoryCookie(UInt64(i + 1)),
                attributes: attrs)
            if !packed { break }
        }
        // Changes with the directory's modification time.
        let mtime = UInt64(dirStat.st_mtimespec.tv_sec) &* 1_000_000_000 &+ UInt64(dirStat.st_mtimespec.tv_nsec)
        return FSDirectoryVerifier(max(mtime, 1))
    }
}

extension RustFuseVolume: FSVolume.OpenCloseOperations {
    func openItem(_ item: FSItem, modes: FSVolume.OpenModes) async throws {
        guard let n = item as? Node else { throw POSIXError(.EINVAL) }
        guard (try info(n.rel).st_mode & S_IFMT) == S_IFREG else { return }
        try openFile(n, write: modes.contains(.write))
    }

    func closeItem(_ item: FSItem, modes: FSVolume.OpenModes) async throws {
        // `modes` is what stays open; none left means fully closed.
        guard let n = item as? Node, modes.isEmpty else { return }
        n.closeFile()
    }
}

extension RustFuseVolume: FSVolume.ReadWriteOperations {
    func read(from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer) async throws -> Int {
        guard let n = item as? Node else { throw POSIXError(.EINVAL) }
        try ensureContent(n)
        try openFile(n, write: false)
        var got = 0
        var failed = false
        buffer.withUnsafeMutableBytes { dst in
            got = pread(n.fd, dst.baseAddress, min(length, dst.count), offset)
            failed = got < 0
        }
        if failed { throw posixError() }
        return got
    }

    func write(contents: Data, to item: FSItem, at offset: off_t) async throws -> Int {
        guard let n = item as? Node else { throw POSIXError(.EINVAL) }
        try openFile(n, write: true)
        let wrote = contents.withUnsafeBytes { src in pwrite(n.fd, src.baseAddress, src.count, offset) }
        if wrote < 0 { throw posixError() }
        return wrote
    }
}

extension RustFuseVolume {
    /// Forgets a reclaimed item.
    func forget(_ n: Node) {
        withNodes { nodes in
            if nodes[n.ino] === n { nodes[n.ino] = nil }
        }
    }

    /// After a rename: the item and, for a directory, everything under it
    /// the kernel holds now live at `to`.
    func moved(from: String, to: String) {
        withNodes { nodes in
            for n in nodes.values {
                if n.rel == from {
                    n.rel = to
                } else if n.rel.hasPrefix(from + "/") {
                    n.rel = to + n.rel.dropFirst(from.count)
                }
            }
        }
    }
}
