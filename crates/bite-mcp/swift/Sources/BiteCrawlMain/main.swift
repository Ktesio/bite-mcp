// bite-crawl — standalone Mail indexing process.
//
// Spawned detached by the Rust control plane (`bite index rebuild`); runs
// the 30-day initial window + 10-day backfill batches and writes JSONL
// batches + crawl-state.json into bite's data dir. Exits when done.

import Foundation
import BiteCrawlCore

// Parse args: --window-days N --mailbox X --no-body
var windowDays = 30
var mailboxFilter: String?
var storeBody = true

var it = Array(CommandLine.arguments.dropFirst()).makeIterator()
while let arg = it.next() {
    switch arg {
    case "--window-days":
        windowDays = Int(it.next() ?? "") ?? 30
    case "--mailbox":
        mailboxFilter = it.next()
    case "--no-body":
        storeBody = false
    default:
        break
    }
}

let jobID = "crawl-\(Int(Date().timeIntervalSince1970))"
MailCrawler.writeState(jobID: jobID, state: "running", processed: 0, found: 0, window: nil)
MailCrawler.runJob(jobID: jobID, windowDays: windowDays, storeBody: storeBody, mailboxFilter: mailboxFilter) { state, processed, found in
    MailCrawler.writeState(jobID: jobID, state: state, processed: processed, found: found, window: nil)
}
exit(0)
