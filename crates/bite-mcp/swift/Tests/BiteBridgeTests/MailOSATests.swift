import XCTest
import BiteCrawlCore

// Pure helpers of the OSA Mail transport (MailAE/MailCrawler/MailBulk) —
// script building, walk planning, reply mapping, terminal states. No Mail
// interaction happens here.
final class MailOSATests: XCTestCase {
    // ── script building ──

    func testQuotedAppleStringEscapes() {
        XCTAssertEqual(MailAE.quotedAppleString("plain"), "\"plain\"")
        XCTAssertEqual(MailAE.quotedAppleString("he said \"hi\""), "\"he said \\\"hi\\\"\"")
        XCTAssertEqual(MailAE.quotedAppleString("back\\slash"), "\"back\\\\slash\"")
        XCTAssertEqual(MailAE.quotedAppleString("Funnel 📥 INBOX"), "\"Funnel 📥 INBOX\"")
    }

    func testQuotedAppleStringControlCharacters() {
        XCTAssertEqual(MailAE.quotedAppleString("line1\nline2"), "\"line1\\nline2\"")
        XCTAssertEqual(MailAE.quotedAppleString("cr\rend"), "\"cr\\rend\"")
        XCTAssertEqual(MailAE.quotedAppleString("col\tvalue"), "\"col\\tvalue\"")
        // other scalars below 0x20 are stripped (no AppleScript escape exists)
        XCTAssertEqual(MailAE.quotedAppleString("a\u{01}b\u{1F}c"), "\"abc\"")
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

    func testIntStringWideAndRealDescriptors() {
        // 'comp' — 64-bit big-endian integer beyond Int32
        var compBits = UInt64(bitPattern: 3_500_000_000).bigEndian
        let comp = NSAppleEventDescriptor(descriptorType: DescType(0x636f6d70), bytes: &compBits, length: 8)!
        XCTAssertEqual(MailAE.intString(comp), "3500000000")
        // 'doub' — AppleScript widens overflowing integers to real
        var doubBits = (3_600_000_000.0 as Double).bitPattern.bigEndian
        let doub = NSAppleEventDescriptor(descriptorType: DescType(0x646f7562), bytes: &doubBits, length: 8)!
        XCTAssertEqual(MailAE.intString(doub), "3600000000")
    }

    // ── walk planning ──

    func testWalkPlanNewestFirstStartsWithProbeRange() {
        let (plan, processProbeFirst) = MailAE.walkPlan(total: 105, walkLimit: 105, chunkSize: 40, newestFirst: true)
        XCTAssertEqual(plan.map { "\($0.lowerBound)-\($0.upperBound)" }, ["1-40", "41-80", "81-105"])
        XCTAssertTrue(processProbeFirst)
        // probe reads 1…min(chunkSize, walkLimit, total) — exactly plan[0]
        XCTAssertEqual(plan[0], 1...min(40, 105, 105))
    }

    func testWalkPlanOldestFirstCoversNewestChunkAndFullPlan() {
        let (plan, processProbeFirst) = MailAE.walkPlan(total: 105, walkLimit: 105, chunkSize: 40, newestFirst: false)
        // walk starts at the far (newest) end…
        XCTAssertEqual(plan.first?.upperBound, 105)
        // …and the plan is NOT truncated: the tail (oldest) rows stay covered
        XCTAssertEqual(plan.map { "\($0.lowerBound)-\($0.upperBound)" }, ["66-105", "26-65", "1-25"])
        XCTAssertFalse(processProbeFirst)
    }

    func testWalkPlanOldestFirstClampsToWalkLimit() {
        let (plan, _) = MailAE.walkPlan(total: 105, walkLimit: 50, chunkSize: 40, newestFirst: false)
        // walkLimit 50 → positions 56…105, chunks in descending walk order
        XCTAssertEqual(plan.map { "\($0.lowerBound)-\($0.upperBound)" }, ["66-105", "56-65"])
    }

    func testWalkPlanTotalWithinSingleChunk() {
        for newestFirst in [true, false] {
            let (plan, processProbeFirst) = MailAE.walkPlan(total: 3, walkLimit: 20_000, chunkSize: 12, newestFirst: newestFirst)
            XCTAssertEqual(plan.map { "\($0.lowerBound)-\($0.upperBound)" }, ["1-3"], "newestFirst=\(newestFirst)")
            XCTAssertEqual(processProbeFirst, newestFirst)
        }
    }

    // ── chunk failure policy ──

    func testFailureDecisionRetriesSameRange() {
        let range = 13...24
        // no refreshed count → treat as transient, retry the SAME range
        XCTAssertEqual(MailAE.failureDecision(range: range, refreshedTotal: nil, consecutiveFailures: 0), .retrySame)
        XCTAssertEqual(MailAE.failureDecision(range: range, refreshedTotal: 105, consecutiveFailures: 1), .retrySame)
    }

    func testFailureDecisionSkipsEvaporatedRangeWithoutStrike() {
        // mailbox shrank below the range start → advance, no failure counted
        XCTAssertEqual(MailAE.failureDecision(range: 96...105, refreshedTotal: 50, consecutiveFailures: 0), .advance)
    }

    func testFailureDecisionAbortsAfterThreeStrikes() {
        XCTAssertEqual(MailAE.failureDecision(range: 13...24, refreshedTotal: nil, consecutiveFailures: 3), .abort)
        XCTAssertEqual(MailAE.failureDecision(range: 13...24, refreshedTotal: 105, consecutiveFailures: 4), .abort)
    }

    // ── walk order detection ──

    func testOrderFromSamples() {
        let early = Date(timeIntervalSince1970: 1_000)
        let late = Date(timeIntervalSince1970: 2_000)
        XCTAssertEqual(MailAE.orderFromSamples(late, early), true)   // newest-first
        XCTAssertEqual(MailAE.orderFromSamples(early, late), false)  // oldest-first
        XCTAssertNil(MailAE.orderFromSamples(nil, late))             // missing sample
        XCTAssertNil(MailAE.orderFromSamples(late, nil))
        XCTAssertNil(MailAE.orderFromSamples(late, late))            // equal → ambiguous
    }

    // ── reply mapping ──

    private func stringDesc(_ s: String) -> NSAppleEventDescriptor { NSAppleEventDescriptor(string: s) }

    func testRowsFromReplyBareRecordIsSingleRow() {
        // `messages S thru S` answers a bare record, not a list
        let rec = NSAppleEventDescriptor.record()
        let rows = MailAE.rowsFromReply(rec, expectedProps: 7)
        XCTAssertEqual(rows.count, 1)
        XCTAssertEqual(rows[0]?.descriptorType, rec.descriptorType)
    }

    func testRowsFromReplyListOfRecords() {
        let list = NSAppleEventDescriptor.list()
        list.insert(NSAppleEventDescriptor.record(), at: 1)
        list.insert(NSAppleEventDescriptor.record(), at: 2)
        let rows = MailAE.rowsFromReply(list, expectedProps: 0)
        XCTAssertEqual(rows.count, 2)
    }

    func testRowsFromReplyRowMajorLists() {
        let list = NSAppleEventDescriptor.list()
        list.insert(NSAppleEventDescriptor.list(), at: 1)
        list.insert(NSAppleEventDescriptor.list(), at: 2)
        let rows = MailAE.rowsFromReply(list, expectedProps: 7)
        XCTAssertEqual(rows.count, 2)
    }

    func testRowsFromReplyPropertyMajorColumns() {
        // CONFIRMED live shape for the no-body bundle: outer list of 7
        // property columns, each column a list of per-message values
        let propNames = ["id", "subject", "sender", "date", "read", "flagged", "junk"]
        let outer = NSAppleEventDescriptor.list()
        for p in 0..<7 {
            let col = NSAppleEventDescriptor.list()
            for m in 1...3 {
                col.insert(p == 0 ? NSAppleEventDescriptor(int32: Int32(m)) : stringDesc("\(propNames[p]) m\(m)"),
                           at: Int(col.numberOfItems) + 1)
            }
            outer.insert(col, at: Int(outer.numberOfItems) + 1)
        }
        let rows = MailAE.rowsFromReply(outer, expectedProps: 7)
        XCTAssertEqual(rows.count, 3)
        XCTAssertEqual(rows[0]?.atIndex(1)?.int32Value, 1)
        XCTAssertEqual(rows[0]?.atIndex(2)?.stringValue, "subject m1")
        XCTAssertEqual(rows[0]?.atIndex(7)?.stringValue, "junk m1")
        XCTAssertEqual(rows[2]?.atIndex(1)?.int32Value, 3)
        XCTAssertEqual(rows[2]?.atIndex(2)?.stringValue, "subject m3")
    }

    func testRowsFromReplyAmbiguousSevenBySevenDisambiguatesByTypes() {
        // 7×7: columns of the SAME scalar type → property-major columns
        let columns = NSAppleEventDescriptor.list()
        for p in 0..<7 {
            let col = NSAppleEventDescriptor.list()
            for m in 1...7 { col.insert(NSAppleEventDescriptor(int32: Int32(p * 10 + m)), at: Int(col.numberOfItems) + 1) }
            columns.insert(col, at: Int(columns.numberOfItems) + 1)
        }
        let rows = MailAE.rowsFromReply(columns, expectedProps: 7)
        XCTAssertEqual(rows.count, 7)
        XCTAssertEqual(rows[0]?.atIndex(1)?.int32Value, 1)       // col0 m1
        XCTAssertEqual(rows[0]?.atIndex(2)?.int32Value, 11)      // col1 m1
        // 7×7 heterogeneous rows (mixed types) → row-major, rows as-is
        let mixed = NSAppleEventDescriptor.list()
        for m in 1...7 {
            let row = NSAppleEventDescriptor.list()
            row.insert(NSAppleEventDescriptor(int32: Int32(m)), at: 1)          // id: long
            row.insert(stringDesc("s\(m)"), at: 2)                              // subject: text
            for _ in 3...7 { row.insert(NSAppleEventDescriptor(boolean: true), at: Int(row.numberOfItems) + 1) }
            mixed.insert(row, at: Int(mixed.numberOfItems) + 1)
        }
        let rows2 = MailAE.rowsFromReply(mixed, expectedProps: 7)
        XCTAssertEqual(rows2.count, 7)
        XCTAssertEqual(rows2[3]?.atIndex(2)?.stringValue, "s4")  // row kept intact
    }

    func testRowsFromReplyPropertyMajorFlatBundle() {
        // get {p1, p2, p3} of messages 1 thru 2 → flat list in
        // property-major order: [p1m1, p1m2, p2m1, p2m2, p3m1, p3m2]
        let list = NSAppleEventDescriptor.list()
        let values = ["p1m1", "p1m2", "p2m1", "p2m2", "p3m1", "p3m2"]
        for v in values { list.insert(stringDesc(v), at: Int(list.numberOfItems) + 1) }
        let rows = MailAE.rowsFromReply(list, expectedProps: 3)
        XCTAssertEqual(rows.count, 2)
        XCTAssertEqual(rows[0]?.atIndex(1)?.stringValue, "p1m1")
        XCTAssertEqual(rows[0]?.atIndex(2)?.stringValue, "p2m1")
        XCTAssertEqual(rows[0]?.atIndex(3)?.stringValue, "p3m1")
        XCTAssertEqual(rows[1]?.atIndex(1)?.stringValue, "p1m2")
        XCTAssertEqual(rows[1]?.atIndex(2)?.stringValue, "p2m2")
        XCTAssertEqual(rows[1]?.atIndex(3)?.stringValue, "p3m2")
    }

    func testRowsFromReplyMalformedFlatBundleYieldsNoRows() {
        let list = NSAppleEventDescriptor.list()
        for v in ["a", "b", "c"] { list.insert(stringDesc(v), at: Int(list.numberOfItems) + 1) }
        XCTAssertTrue(MailAE.rowsFromReply(list, expectedProps: 7).isEmpty)  // 3 % 7 != 0
        XCTAssertTrue(MailAE.rowsFromReply(NSAppleEventDescriptor.list(), expectedProps: 7).isEmpty)
    }

    // ── record mapping (JSONL contract) ──

    private func sampleRow() -> NSAppleEventDescriptor {
        let row = NSAppleEventDescriptor.record()
        row.setDescriptor(NSAppleEventDescriptor(int32: 542874), forKeyword: MailAE.kwMessageID)
        row.setDescriptor(stringDesc("Test subject"), forKeyword: MailAE.kwSubject)
        row.setDescriptor(stringDesc("Ada <ada@example.com>"), forKeyword: MailAE.kwSender)
        row.setDescriptor(NSAppleEventDescriptor(date: Date(timeIntervalSince1970: 1_790_000_000)), forKeyword: MailAE.kwDateSent)
        row.setDescriptor(NSAppleEventDescriptor(boolean: true), forKeyword: MailAE.kwRead)
        row.setDescriptor(NSAppleEventDescriptor(boolean: false), forKeyword: MailAE.kwFlagged)
        row.setDescriptor(NSAppleEventDescriptor(boolean: true), forKeyword: MailAE.kwJunk)
        row.setDescriptor(stringDesc("body text"), forKeyword: MailAE.kwContent)
        return row
    }

    private func sampleBundleRow() -> NSAppleEventDescriptor {
        // bundleProps order: id, subject, sender, date sent, read, flagged, junk
        let row = NSAppleEventDescriptor.list()
        row.insert(NSAppleEventDescriptor(int32: 542874), at: 1)
        row.insert(stringDesc("Test subject"), at: 2)
        row.insert(stringDesc("Ada <ada@example.com>"), at: 3)
        row.insert(NSAppleEventDescriptor(date: Date(timeIntervalSince1970: 1_790_000_000)), at: 4)
        row.insert(NSAppleEventDescriptor(boolean: true), at: 5)
        row.insert(NSAppleEventDescriptor(boolean: false), at: 6)
        row.insert(NSAppleEventDescriptor(boolean: true), at: 7)
        return row
    }

    private func assertContractRecord(_ rec: CrawlRecord?) {
        XCTAssertNotNil(rec)
        XCTAssertEqual(rec?.app, "mail")
        XCTAssertEqual(rec?.id, "542874")
        XCTAssertEqual(rec?.account, "iCloud")
        XCTAssertEqual(rec?.container, "INBOX")
        XCTAssertEqual(rec?.title, "Test subject")
        XCTAssertEqual(rec?.participants, "Ada <ada@example.com>")
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

    func testRecordFromPropertiesRecordShapeMapsContractFields() {
        let rec = MailAE.recordFromRow(sampleRow(), account: "iCloud", mailbox: "INBOX", includeContent: true)
        assertContractRecord(rec)
        XCTAssertEqual(rec?.content, "body text")
    }

    func testRecordFromBundleRowMapsContractFields() {
        let rec = MailAE.recordFromRow(sampleBundleRow(), account: "iCloud", mailbox: "INBOX", includeContent: false)
        assertContractRecord(rec)
        XCTAssertNil(rec?.content)  // --no-body never carries content
    }

    func testRecordContentOmittedWhenStoreBodyOff() {
        let rec = MailAE.recordFromRow(sampleRow(), account: "a", mailbox: "m", includeContent: false)
        XCTAssertNil(rec?.content)
        XCTAssertNotNil(rec?.title)
    }

    func testRecordRejectsMissingOrZeroID() {
        XCTAssertNil(MailAE.recordFromRow(nil, account: "a", mailbox: "m", includeContent: true))
        let zero = NSAppleEventDescriptor.record()
        zero.setDescriptor(NSAppleEventDescriptor(int32: 0), forKeyword: MailAE.kwMessageID)
        XCTAssertNil(MailAE.recordFromRow(zero, account: "a", mailbox: "m", includeContent: true))
        let emptyList = NSAppleEventDescriptor.list()
        XCTAssertNil(MailAE.recordFromRow(emptyList, account: "a", mailbox: "m", includeContent: true))
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

    // ── bulk terminal states ──

    func testBulkTerminalStateTable() {
        XCTAssertEqual(MailBulk.terminalState(ok: true, remaining: 0), "done")
        XCTAssertEqual(MailBulk.terminalState(ok: true, remaining: 5), "partial")
        XCTAssertEqual(MailBulk.terminalState(ok: true, remaining: nil), "failed")  // verification failed
        XCTAssertEqual(MailBulk.terminalState(ok: false, remaining: 0), "failed")
        XCTAssertEqual(MailBulk.terminalState(ok: false, remaining: nil), "failed")
    }

    func testBulkSelectionLabelAndEmptiness() {
        XCTAssertTrue(BulkSelection().isEmpty)
        XCTAssertFalse(BulkSelection(unread: false).isEmpty)
        XCTAssertEqual(BulkSelection(unread: false).label, "read")
        XCTAssertEqual(BulkSelection(unread: true, olderThanDays: 7).label, "unread AND olderThanDays=7")
    }
}
