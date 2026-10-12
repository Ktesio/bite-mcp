import XCTest
import BiteCrawlCore

// ── recipient enrichment (participants = sender + to recipients) ──

final class RecipientEnrichmentTests: XCTestCase {
    private func rec(_ id: String, participants: String?) -> CrawlRecord {
        CrawlRecord(app: "mail", id: id, account: "a", container: "INBOX",
                    title: "t", content: nil, participants: participants,
                    start_ms: 1, end_ms: nil, updated_ms: 1,
                    read: true, flagged: false, junk: false,
                    completed: nil, priority: nil, props: nil)
    }

    func testApplyRecipientsAppendsTosAfterSender() {
        let out = MailCrawler.applyRecipients(
            [rec("1", participants: "Ada <ada@example.com>")],
            recipients: ["1": ["bob@example.com", "carol@example.com"]])
        XCTAssertEqual(out[0].participants, "Ada <ada@example.com>, bob@example.com, carol@example.com")
    }

    func testApplyRecipientsDedupesSenderCaseInsensitively() {
        let out = MailCrawler.applyRecipients(
            [rec("1", participants: "Ada <ada@example.com>")],
            recipients: ["1": ["ADA@example.com", "bob@example.com"]])
        XCTAssertEqual(out[0].participants, "Ada <ada@example.com>, bob@example.com")
    }

    func testApplyRecipientsCapsAtTwentyAndSkipsUnknownIds() {
        let tos = (1...30).map { "to\($0)@example.com" }
        let out = MailCrawler.applyRecipients(
            [rec("1", participants: "s"), rec("2", participants: "s2")],
            recipients: ["1": tos])
        XCTAssertEqual(out[0].participants!.components(separatedBy: ", ").count, 20)
        XCTAssertEqual(out[1].participants, "s2")  // id not in map → untouched
    }

    func testApplyRecipientsNilSenderKeepsRecipientsOnly() {
        let out = MailCrawler.applyRecipients(
            [rec("1", participants: nil)],
            recipients: ["1": ["bob@example.com"]])
        XCTAssertEqual(out[0].participants, "bob@example.com")
    }

    // ── recipientsFromReply shapes ──

    func testRecipientsFromReplyPropertyMajor() {
        // property-major: [ids…], [cell…]
        let reply = NSAppleEventDescriptor.list()
        let ids = NSAppleEventDescriptor.list()
        ids.insert(NSAppleEventDescriptor(int32: 5), at: 1)
        ids.insert(NSAppleEventDescriptor(int32: 6), at: 2)
        let cells = NSAppleEventDescriptor.list()
        let cell1 = NSAppleEventDescriptor.list()
        cell1.insert(NSAppleEventDescriptor(string: "bob@example.com"), at: 1)
        cells.insert(cell1, at: 1)
        let cell2 = NSAppleEventDescriptor.list()  // no recipients
        cells.insert(cell2, at: 2)
        reply.insert(ids, at: 1)
        reply.insert(cells, at: 2)
        let out = MailAE.recipientsFromReply(reply)
        XCTAssertEqual(out["5"], ["bob@example.com"])
        XCTAssertEqual(out["6"], [])
    }

    func testRecipientsFromReplyRowMajor() {
        // row-major: one [id, cell] list per message
        let reply = NSAppleEventDescriptor.list()
        let row = NSAppleEventDescriptor.list()
        row.insert(NSAppleEventDescriptor(int32: 7), at: 1)
        let cell = NSAppleEventDescriptor.list()
        cell.insert(NSAppleEventDescriptor(string: "x@example.com"), at: 1)
        row.insert(cell, at: 2)
        reply.insert(row, at: 1)
        let out = MailAE.recipientsFromReply(reply)
        XCTAssertEqual(out["7"], ["x@example.com"])
    }

    func testRecipientsFromReplyRowMajorTwoMessages() {
        // a 2-message row-major reply is also 2 items wide — must NOT be
        // misread as property-major (each row is [id, cell], both lists)
        let reply = NSAppleEventDescriptor.list()
        for id in [7, 8] {
            let row = NSAppleEventDescriptor.list()
            row.insert(NSAppleEventDescriptor(int32: Int32(id)), at: 1)
            let cell = NSAppleEventDescriptor.list()
            cell.insert(NSAppleEventDescriptor(string: "x\(id)@example.com"), at: 1)
            row.insert(cell, at: 2)
            reply.insert(row, at: reply.numberOfItems + 1)
        }
        let out = MailAE.recipientsFromReply(reply)
        XCTAssertEqual(out["7"], ["x7@example.com"])
        XCTAssertEqual(out["8"], ["x8@example.com"])
    }

    func testRecipientsFromReplyNilAndEmpty() {
        XCTAssertEqual(MailAE.recipientsFromReply(nil), [:])
        XCTAssertEqual(MailAE.recipientsFromReply(NSAppleEventDescriptor.record()), [:])
    }
}
