import Foundation

/// Small, dependency-free Markdown ⇄ HTML converters for Notes bodies and
/// message content. Not CommonMark-complete — covers the subset agents emit:
/// headings, bold/italic/code, links, lists, blockquotes, hr, paragraphs.

func markdownToHtml(_ md: String) -> String {
    guard !md.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
        return "<div></div>"
    }
    var out = ""
    var inList = false
    var lines = md.components(separatedBy: "\n")
    // emit closing list tag at end
    lines.append("")  // sentinel to flush list

    func inline(_ s: String) -> String {
        var t = escapeHtml(s)
        t = codeSpans(t)
        t = replace(t, pattern: #"\*\*([^*]+)\*\*"#) { "<b>\($0)</b>" }
        t = replace(t, pattern: #"(?<!\*)\*([^*]+)\*(?!\*)"#) { "<i>\($0)</i>" }
        t = replace2(t, pattern: #"\[([^\]]+)\]\(([^)]+)\)"#) { text, url in "<a href=\"\(url)\">\(text)</a>" }
        return t
    }

    var idx = 0
    while idx < lines.count {
        let line = lines[idx]
        let trimmed = line.trimmingCharacters(in: .whitespaces)

        if trimmed.isEmpty {
            if inList { out += "</ul>\n"; inList = false }
            idx += 1
            continue
        }

        if trimmed.hasPrefix("#") {
            let level = min(trimmed.prefix(while: { $0 == "#" }).count, 6)
            let content = trimmed.drop(while: { $0 == "#" }).trimmingCharacters(in: .whitespaces)
            if inList { out += "</ul>\n"; inList = false }
            out += "<h\(level)>\(inline(content))</h\(level)>\n"
            idx += 1
            continue
        }

        if trimmed == "---" || trimmed == "***" {
            if inList { out += "</ul>\n"; inList = false }
            out += "<hr>\n"
            idx += 1
            continue
        }

        if trimmed.hasPrefix("- ") || trimmed.hasPrefix("* ") || trimmed.hasPrefix("+ ") {
            if !inList { out += "<ul>\n"; inList = true }
            out += "<li>\(inline(String(trimmed.dropFirst(2))))</li>\n"
            idx += 1
            continue
        }

        if trimmed.hasPrefix("> ") {
            if inList { out += "</ul>\n"; inList = false }
            out += "<blockquote>\(inline(String(trimmed.dropFirst(2))))</blockquote>\n"
            idx += 1
            continue
        }

        // paragraph: consecutive non-empty, non-structural lines
        var para = [inline(trimmed)]
        idx += 1
        while idx < lines.count {
            let nxt = lines[idx].trimmingCharacters(in: .whitespaces)
            if nxt.isEmpty || nxt.hasPrefix("#") || nxt.hasPrefix("- ") || nxt.hasPrefix("* ")
                || nxt.hasPrefix("+ ") || nxt.hasPrefix("> ") || nxt == "---" || nxt == "***" { break }
            para.append(inline(nxt))
            idx += 1
        }
        if inList { out += "</ul>\n"; inList = false }
        out += "<div>\(para.joined(separator: "<br>\n"))</div>\n"
    }
    if inList { out += "</ul>\n" }
    return out
}

// MARK: - helpers

private func escapeHtml(_ s: String) -> String {
    s.replacingOccurrences(of: "&", with: "&amp;")
        .replacingOccurrences(of: "<", with: "&lt;")
        .replacingOccurrences(of: ">", with: "&gt;")
}

private func replace(_ s: String, pattern: String, transform: (String) -> String) -> String {
    replace2(s, pattern: pattern) { a, _ in transform(a) }
}

private func replace2(_ s: String, pattern: String, transform: (String, String) -> String) -> String {
    guard let regex = try? NSRegularExpression(pattern: pattern) else { return s }
    let ns = s as NSString
    var out = ""
    var cursor = 0
    regex.enumerateMatches(in: s, range: NSRange(location: 0, length: ns.length)) { m, _, _ in
        guard let m, m.range.location != NSNotFound else { return }
        out += ns.substring(with: NSRange(location: cursor, length: m.range.location - cursor))
        let g1 = m.numberOfRanges > 1 ? ns.substring(with: m.range(at: 1)) : ""
        let g2 = m.numberOfRanges > 2 ? ns.substring(with: m.range(at: 2)) : ""
        out += transform(g1, g2)
        cursor = m.range.location + m.range.length
    }
    out += ns.substring(from: cursor)
    return out
}

/// `code` spans: escape inner text, mark so later passes don't touch it.
private func codeSpans(_ s: String) -> String {
    guard let regex = try? NSRegularExpression(pattern: #"`([^`]+)`"#) else { return s }
    let ns = s as NSString
    var out = ""
    var cursor = 0
    regex.enumerateMatches(in: s, range: NSRange(location: 0, length: ns.length)) { m, _, _ in
        guard let m, m.range.location != NSNotFound else { return }
        out += ns.substring(with: NSRange(location: cursor, length: m.range.location - cursor))
        out += "<code>\(ns.substring(with: m.range(at: 1)))</code>"
        cursor = m.range.location + m.range.length
    }
    out += ns.substring(from: cursor)
    return out
}

/// HTML → plain text (for snippets).
func htmlToText(_ html: String) -> String {
    var t = html.replacingOccurrences(of: "<br>", with: "\n")
        .replacingOccurrences(of: "<br/>", with: "\n")
        .replacingOccurrences(of: "<br />", with: "\n")
        .replacingOccurrences(of: "</div>", with: "\n")
        .replacingOccurrences(of: "</p>", with: "\n")
        .replacingOccurrences(of: "</li>", with: "\n")
        .replacingOccurrences(of: "</h1>", with: "\n")
        .replacingOccurrences(of: "</h2>", with: "\n")
        .replacingOccurrences(of: "</h3>", with: "\n")
        .replacingOccurrences(of: "</blockquote>", with: "\n")
    t = t.replacingOccurrences(of: "&nbsp;", with: " ")
        .replacingOccurrences(of: "&amp;", with: "&")
        .replacingOccurrences(of: "&lt;", with: "<")
        .replacingOccurrences(of: "&gt;", with: ">")
        .replacingOccurrences(of: "&quot;", with: "\"")
    // strip remaining tags
    while let start = t.range(of: "<") , let end = t.range(of: ">", range: start.upperBound..<t.endIndex) {
        t.removeSubrange(start.lowerBound..<end.upperBound)
    }
    // collapse >2 newlines
    while t.contains("\n\n\n") { t = t.replacingOccurrences(of: "\n\n\n", with: "\n\n") }
    return t
}

/// Notes HTML → readable Markdown (best-effort inverse).
func htmlToMarkdown(_ html: String) -> String {
    var t = html
    t = replace(t, pattern: #"<h1>(.*?)</h1>"#, transform: { "# \($0)" })
    t = replace(t, pattern: #"<h2>(.*?)</h2>"#, transform: { "## \($0)" })
    t = replace(t, pattern: #"<h3>(.*?)</h3>"#, transform: { "### \($0)" })
    t = replace(t, pattern: #"<b>(.*?)</b>"#, transform: { "**\($0)**" })
    t = replace(t, pattern: #"<i>(.*?)</i>"#, transform: { "*\($0)*" })
    t = replace(t, pattern: #"<li>(.*?)</li>"#, transform: { "- \($0)" })
    t = replace2(t, pattern: #"<a href="([^"]*)">(.*?)</a>"#) { url, text in "[\(text)](\(url))" }
    t = t.replacingOccurrences(of: "<br>", with: "\n")
        .replacingOccurrences(of: "</div>", with: "\n")
        .replacingOccurrences(of: "</ul>", with: "")
    // strip remaining tags
    while let start = t.range(of: "<") , let end = t.range(of: ">", range: start.upperBound..<t.endIndex) {
        t.removeSubrange(start.lowerBound..<end.upperBound)
    }
    t = t.replacingOccurrences(of: "&amp;", with: "&")
        .replacingOccurrences(of: "&lt;", with: "<")
        .replacingOccurrences(of: "&gt;", with: ">")
        .replacingOccurrences(of: "&quot;", with: "\"")
        .replacingOccurrences(of: "&nbsp;", with: " ")
    return t
}
