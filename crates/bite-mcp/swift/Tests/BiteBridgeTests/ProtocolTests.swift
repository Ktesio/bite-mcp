import XCTest
@testable import bite_helper

final class MarkdownTests: XCTestCase {
    func testHeadingsAndParagraphs() {
        let html = markdownToHtml("# Title\n\nHello **world** and *stars*.")
        XCTAssertTrue(html.contains("<h1>Title</h1>"))
        XCTAssertTrue(html.contains("<b>world</b>"))
        XCTAssertTrue(html.contains("<i>stars</i>"))
    }

    func testListAndLink() {
        let html = markdownToHtml("- first\n- second\n\nSee [docs](https://example.com).")
        XCTAssertTrue(html.contains("<ul>"))
        XCTAssertTrue(html.contains("<li>first</li>"))
        XCTAssertTrue(html.contains("<li>second</li>"))
        XCTAssertTrue(html.contains("<a href=\"https://example.com\">docs</a>"))
    }

    func testEscaping() {
        let html = markdownToHtml("a < b & c")
        XCTAssertTrue(html.contains("a &lt; b &amp; c"))
        XCTAssertFalse(html.contains("<div>a < b"))
    }

    func testCodeSpan() {
        let html = markdownToHtml("run `bite doctor` now")
        XCTAssertTrue(html.contains("<code>bite doctor</code>"))
    }

    func testHtmlToText() {
        let text = htmlToText("<div>Hello<br>World</div><div>Bye</div>")
        XCTAssertTrue(text.contains("Hello"))
        XCTAssertTrue(text.contains("World"))
        XCTAssertFalse(text.contains("<"))
    }

    func testHtmlToMarkdownRoundtrip() {
        let md = "## Heading\n- item one\n- item two"
        let html = markdownToHtml(md)
        let back = htmlToMarkdown(html)
        XCTAssertTrue(back.contains("## Heading"))
        XCTAssertTrue(back.contains("- item one"))
    }
}

final class DateTests: XCTestCase {
    func testParseIsoWithOffset() throws {
        let d = try D.parse("2026-09-21T15:00:00+02:00")
        let s = D.format(d)
        XCTAssertNotNil(s)
        XCTAssertTrue(s!.contains("T"))
    }

    func testParseIsoZulu() throws {
        XCTAssertNoThrow(try D.parse("2026-09-21T15:00:00Z"))
    }

    func testParseDayOnly() throws {
        XCTAssertNoThrow(try D.parse("2026-09-21"))
    }

    func testParseGarbageThrows() {
        XCTAssertThrowsError(try D.parse("sometime next week"))
    }
}

final class ProtocolTests: XCTestCase {
    func testParseRequest() throws {
        let line = #"{"id":7,"method":"sys.ping","params":{"probe":true}}"#.data(using: .utf8)!
        let req = Envelope.parse(line)
        XCTAssertEqual(req?.id, 7)
        XCTAssertEqual(req?.method, "sys.ping")
        XCTAssertEqual(req?.params["probe"] as? Bool, true)
    }

    func testParseStringId() throws {
        let line = #"{"id":"12","method":"sys.ping","params":{}}"#.data(using: .utf8)!
        XCTAssertEqual(Envelope.parse(line)?.id, 12)
    }

    func testNotificationHasNoId() throws {
        let line = #"{"method":"log","params":{}}"#.data(using: .utf8)!
        XCTAssertNil(Envelope.parse(line)?.id)
    }

    func testDispatchResponds() {
        let d = Dispatcher()
        d.register("sys.ping") { _ in ["pong": true] }
        let req = Incoming(id: 1, method: "sys.ping", params: [:])
        let resp = d.respond(req)
        XCTAssertEqual(resp["id"] as? Int64, 1)
        XCTAssertEqual((resp["result"] as? [String: Any])?["pong"] as? Bool, true)
    }

    func testDispatchUnknownMethod() {
        let d = Dispatcher()
        let req = Incoming(id: 2, method: "nope.nada", params: [:])
        let resp = d.respond(req)
        let err = resp["error"] as? [String: Any]
        XCTAssertEqual(err?["code"] as? String, "unknown_method")
    }

    func testDispatchHandlerError() {
        let d = Dispatcher()
        d.register("boom") { _ in throw BridgeError.permissionDenied(app: "Calendar", pane: "Calendars") }
        let resp = d.respond(Incoming(id: 3, method: "boom", params: [:]))
        let err = resp["error"] as? [String: Any]
        XCTAssertEqual(err?["code"] as? String, "permission_denied")
        XCTAssertTrue((err?["fix"] as? String ?? "").contains("x-apple.systempreferences"))
    }
}
