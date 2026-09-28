import XCTest
import BiteCrawlCore

// Pure helpers of the OSA Mail transport (MailAE.swift) — script building,
// chunk planning, and record mapping. No Mail interaction happens here.
final class MailOSATests: XCTestCase {
    func testQuotedAppleStringEscapes() {
        XCTAssertEqual(MailAE.quotedAppleString("plain"), "\"plain\"")
        XCTAssertEqual(MailAE.quotedAppleString("he said \"hi\""), "\"he said \\\"hi\\\"\"")
        XCTAssertEqual(MailAE.quotedAppleString("back\\slash"), "\"back\\\\slash\"")
        XCTAssertEqual(MailAE.quotedAppleString("Funnel 📥 INBOX"), "\"Funnel 📥 INBOX\"")
    }

    func testChunkPlanNewestFirst() {
        let plan = MailAE.chunkPlan(total: 105, walkLimit: 105, chunkSize: 40, newestFirst: true)
        XCTAssertEqual(plan.map { "\($0.start)-\($0.end)" }, ["1-40", "41-80", "81-105"])
    }

    func testChunkPlanOldestFirstWalksDownFromFarEnd() {
        let plan = MailAE.chunkPlan(total: 105, walkLimit: 105, chunkSize: 40, newestFirst: false)
        XCTAssertEqual(plan.map { "\($0.start)-\($0.end)" }, ["66-105", "26-65", "1-25"])
    }

    func testChunkPlanOldestFirstClampsToWalkLimit() {
        let plan = MailAE.chunkPlan(total: 105, walkLimit: 50, chunkSize: 40, newestFirst: false)
        // walkLimit 50 → positions 56…105, chunks in descending walk order
        XCTAssertEqual(plan.map { "\($0.start)-\($0.end)" }, ["66-105", "56-65"])
    }

    func testChunkPlanChunkLargerThanTotal() {
        let plan = MailAE.chunkPlan(total: 3, walkLimit: 20_000, chunkSize: 12, newestFirst: true)
        XCTAssertEqual(plan.map { "\($0.start)-\($0.end)" }, ["1-3"])
    }

    func testWhoseClauseEmptySelection() {
        XCTAssertEqual(MailAE.whoseClause(BulkSelection()), "")
    }

    func testWhoseClauseUnreadAndOlder() {
        let clause = MailAE.whoseClause(BulkSelection(unread: true, olderThanDays: 30))
        XCTAssertEqual(
            clause,
            " whose read status is false and date sent is less than or equal to ((current date) - 30 * days)"
        )
    }

    func testWhoseClauseReadSelection() {
        let clause = MailAE.whoseClause(BulkSelection(unread: false))
        XCTAssertEqual(clause, " whose read status is true")
    }

    func testIntStringVariants() {
        XCTAssertNil(MailAE.intString(nil))
        XCTAssertEqual(MailAE.intString(NSAppleEventDescriptor(string: "12345")), "12345")
        XCTAssertEqual(MailAE.intString(NSAppleEventDescriptor(int32: 542874)), "542874")
        XCTAssertEqual(MailAE.intString(NSAppleEventDescriptor(int32: 0)), "0")
    }

    // ── record mapping (JSONL contract) ──

    private func sampleRow() -> NSAppleEventDescriptor {
        let row = NSAppleEventDescriptor.record()
        row.setDescriptor(NSAppleEventDescriptor(int32: 542874), forKeyword: MailAE.kwMessageID)
        row.setDescriptor(NSAppleEventDescriptor(string: "Test subject"), forKeyword: MailAE.kwSubject)
        row.setDescriptor(NSAppleEventDescriptor(string: "Ada <ada@example.com>"), forKeyword: MailAE.kwSender)
        row.setDescriptor(NSAppleEventDescriptor(date: Date(timeIntervalSince1970: 1_790_000_000)), forKeyword: MailAE.kwDateSent)
        row.setDescriptor(NSAppleEventDescriptor(boolean: true), forKeyword: MailAE.kwRead)
        row.setDescriptor(NSAppleEventDescriptor(boolean: false), forKeyword: MailAE.kwFlagged)
        row.setDescriptor(NSAppleEventDescriptor(boolean: true), forKeyword: MailAE.kwJunk)
        row.setDescriptor(NSAppleEventDescriptor(string: "body text"), forKeyword: MailAE.kwContent)
        return row
    }

    func testRecordFromPropertiesMapsContractFields() {
        let rec = MailAE.recordFromProperties(sampleRow(), account: "iCloud", mailbox: "INBOX", includeContent: true)
        XCTAssertNotNil(rec)
        XCTAssertEqual(rec?.app, "mail")
        XCTAssertEqual(rec?.id, "542874")
        XCTAssertEqual(rec?.account, "iCloud")
        XCTAssertEqual(rec?.container, "INBOX")
        XCTAssertEqual(rec?.title, "Test subject")
        XCTAssertEqual(rec?.participants, "Ada <ada@example.com>")
        XCTAssertEqual(rec?.content, "body text")
        XCTAssertEqual(rec?.read, true)
        XCTAssertEqual(rec?.flagged, false)
        XCTAssertEqual(rec?.junk, true)
        XCTAssertNotNil(rec?.start_ms)
        XCTAssertEqual(rec?.updated_ms, rec?.start_ms)
        XCTAssertNil(rec?.end_ms)
        XCTAssertNil(rec?.completed)
        XCTAssertNil(rec?.priority)
        XCTAssertNil(rec?.props)
    }

    func testRecordContentOmittedWhenStoreBodyOff() {
        let rec = MailAE.recordFromProperties(sampleRow(), account: "a", mailbox: "m", includeContent: false)
        XCTAssertNil(rec?.content)
        XCTAssertNotNil(rec?.title)
    }

    func testRecordRejectsMissingOrZeroID() {
        XCTAssertNil(MailAE.recordFromProperties(nil, account: "a", mailbox: "m", includeContent: true))
        let zero = NSAppleEventDescriptor.record()
        zero.setDescriptor(NSAppleEventDescriptor(int32: 0), forKeyword: MailAE.kwMessageID)
        XCTAssertNil(MailAE.recordFromProperties(zero, account: "a", mailbox: "m", includeContent: true))
    }

    func testRecordJSONLFieldNamesMatchContract() throws {
        let rec = CrawlRecord(app: "mail", id: "1", account: "a", container: "m", title: "t",
                              content: "c", participants: "p", start_ms: 1, end_ms: nil, updated_ms: 2,
                              read: true, flagged: false, junk: true, completed: nil, priority: nil, props: nil)
        let obj = try JSONSerialization.jsonObject(with: JSONEncoder().encode(rec)) as! [String: Any]
        // nil optionals are omitted by JSONEncoder; non-nil fields must keep
        // the exact contract names (bite-index::store::Record).
        for key in ["app", "id", "account", "container", "title", "content", "participants",
                    "start_ms", "updated_ms", "read", "flagged", "junk"] {
            XCTAssertTrue(obj.keys.contains(key), "missing field \(key)")
        }
        for key in ["end_ms", "completed", "priority", "props"] {
            XCTAssertFalse(obj.keys.contains(key), "nil field \(key) must be omitted")
        }
    }
}
