// bite-crawl — standalone background indexer + bulk operator.
//
// Spawned detached by the Rust control plane. Modes:
//   (default)  crawl: 30-day Mail window + 10-day backfill batches + mirrors
//   --bulk-op  one-shot bulk Mail operation (mark/move/delete)

import Foundation
import BiteCrawlCore

// ── args ──
var windowDays = 30
var mailboxFilter: String?
var storeBody = true
var mirrors = true
var bulkOp: String?
var bulkMailbox = "INBOX"
var bulkToMailbox: String?
var bulkAccount: String?
var selection = BulkSelection()
var setRead: Bool?
var setFlagged: Bool?
var setJunk: Bool?

var it = Array(CommandLine.arguments.dropFirst()).makeIterator()
while let arg = it.next() {
    switch arg {
    case "--window-days": windowDays = Int(it.next() ?? "") ?? 30
    case "--mailbox": mailboxFilter = it.next()
    case "--no-body": storeBody = false
    case "--no-mirrors": mirrors = false
    case "--bulk-op": bulkOp = it.next()
    case "--bulk-mailbox": bulkMailbox = it.next() ?? "INBOX"
    case "--to-mailbox": bulkToMailbox = it.next()
    case "--bulk-account": bulkAccount = it.next()
    case "--unread": selection.unread = true
    case "--older-than-days": selection.olderThanDays = Int(it.next() ?? "")
    case "--set-read": setRead = Bool(it.next() ?? "true") ?? true
    case "--set-flagged": setFlagged = Bool(it.next() ?? "true") ?? true
    case "--set-junk": setJunk = Bool(it.next() ?? "true") ?? true
    default: break
    }
}

let jobID = "bulk-\(Int(Date().timeIntervalSince1970))"
MailCrawler.writeState(jobID: jobID, state: "running", processed: 0, found: 0, window: nil)

if let op = bulkOp {
    // ── bulk mode ──
    guard let target = MailAE.mailTarget() else {
        MailCrawler.writeState(jobID: jobID, state: "failed", processed: 0, found: 0, window: nil)
        exit(1)
    }
    let ok = MailBulk.waitHealthy(
        target: target,
        attempts: 1440, interval: 60,          // ~24 h of patience
        isCancelled: { CrawlState.shared.isCancelled },
        progress: { note in
            MailCrawler.writeState(jobID: jobID, state: "waiting_mail", processed: 0, found: 0, window: note)
        }
    )
    guard ok else {
        MailCrawler.writeState(jobID: jobID, state: "failed", processed: 0, found: 0,
                               window: "Mail unresponsive for 24 h")
        exit(1)
    }
    MailBulk.run(jobID: jobID, op: op, accountName: bulkAccount, mailboxName: bulkMailbox,
                 toMailboxName: bulkToMailbox, selection: selection,
                 setRead: setRead, setFlagged: setFlagged, setJunk: setJunk,
                 target: target) { state, count in
        MailCrawler.writeState(jobID: jobID, state: state, processed: count, found: 0, window: nil)
    }
    exit(0)
}

// ── crawl mode ──
MailCrawler.runJob(jobID: jobID, windowDays: windowDays, storeBody: storeBody, mailboxFilter: mailboxFilter) { state, processed, found in
    MailCrawler.writeState(jobID: jobID, state: state, processed: processed, found: found, window: nil)
}

if mirrors, !CrawlState.shared.isCancelled {
    let staging = MailCrawler.stagingDir()
    var seq = 0
    Mirrors.calendar(staging: staging, jobID: jobID, seq: &seq) { _, p, f in
        MailCrawler.writeState(jobID: jobID, state: "running", processed: p, found: f, window: "mirrors:calendar")
    }
    Mirrors.reminders(staging: staging, jobID: jobID, seq: &seq) { _, p, f in
        MailCrawler.writeState(jobID: jobID, state: "running", processed: p, found: f, window: "mirrors:reminders")
    }
    Mirrors.contacts(staging: staging, jobID: jobID, seq: &seq) { _, p, f in
        MailCrawler.writeState(jobID: jobID, state: "running", processed: p, found: f, window: "mirrors:contacts")
    }
}
exit(0)
