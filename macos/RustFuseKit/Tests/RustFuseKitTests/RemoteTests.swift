import Foundation
import XCTest

@testable import RustFuseKit

final class RemoteTests: XCTestCase {
    /// The `listings` in wire.json, which the daemon's tests also check
    /// against what `rustfuse ls --json` prints.
    func testListingsDecode() throws {
        let url = URL(fileURLWithPath: #filePath).deletingLastPathComponent().appendingPathComponent("wire.json")
        let all = try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as! [String: Any]
        let listings = try (all["listings"] as! [Any]).map {
            try JSONDecoder().decode(RemoteListing.self, from: JSONSerialization.data(withJSONObject: $0))
        }
        XCTAssertEqual(listings.count, 2)
        XCTAssertEqual(listings[0].seq, 42)
        XCTAssertEqual(listings[0].entries.map(\.kind), [.dir, .file, .file, .symlink])
        XCTAssertEqual(listings[0].entries[2].mtimeNs, 1_700_000_000_123_456_789)
        XCTAssertEqual(listings[0].entries[3].target, "a.txt")
        XCTAssertNil(listings[0].entries[2].target)
        XCTAssertEqual(listings[1], RemoteListing(seq: 0, entries: []))
    }

    func testArguments() {
        let c = Connection(name: "home", hub: "h:7420")
        XCTAssertEqual(lsArguments(volume: "proj", path: "", c), ["ls", "--volume", "proj", "--json", "--hub", "h:7420"])
        XCTAssertEqual(
            lsArguments(volume: "proj", path: "src/lib", c), ["ls", "--volume", "proj", "src/lib", "--json", "--hub", "h:7420"])
        XCTAssertEqual(
            getArguments(volume: "proj", path: "src", dest: URL(fileURLWithPath: "/Users/me/Downloads/src"), c),
            ["get", "--volume", "proj", "src", "/Users/me/Downloads/src", "--hub", "h:7420"])
    }

    func testConflictCopies() {
        XCTAssertTrue(isConflictCopy("a.conflict-000000ab-20261019-150000.txt"))
        XCTAssertTrue(isConflictCopy("Makefile.conflict-0badf00d-19700101-000000"))
        XCTAssertTrue(isConflictCopy(".gitignore.conflict-000000ab-19700101-000000"))
        XCTAssertFalse(isConflictCopy("a.txt"))
        XCTAssertFalse(isConflictCopy("notes.conflict-about-merging.md"))
        XCTAssertTrue(isConflictCopy("a.conflict-000000AB-20261019-150000x"))
        XCTAssertFalse(isConflictCopy(".conflict-000000ab-20261019-150000"))
        XCTAssertFalse(isConflictCopy("a.conflict-000000ab-2026101-150000"))
        XCTAssertFalse(isConflictCopy("a.conflict-000000ab-20261019-150000.conflict-x"))
    }

    func testConflictReportsDecode() throws {
        let url = URL(fileURLWithPath: #filePath).deletingLastPathComponent().appendingPathComponent("wire.json")
        let all = try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as! [String: Any]
        let reports = try (all["conflicts"] as! [Any]).map {
            try JSONDecoder().decode(ConflictReport.self, from: JSONSerialization.data(withJSONObject: $0))
        }
        XCTAssertEqual(reports[0].files.count, 2)
        XCTAssertEqual(reports[0].branches[1], DivergedBranch(repo: "tools/sub", branch: "feature/x.conflict-0000000c-20261020-091500"))
        XCTAssertTrue(reports[0].files.allSatisfy { isConflictCopy(URL(fileURLWithPath: $0).lastPathComponent) })
        XCTAssertEqual(reports[1], ConflictReport(files: [], branches: []))
        XCTAssertEqual(conflictsArguments(dir: URL(fileURLWithPath: "/V/proj")), ["conflicts", "/V/proj", "--json"])
    }
}
