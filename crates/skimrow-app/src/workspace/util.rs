//! Small pure helpers: SQL string munging and connection-config mapping.

use gpui_component::table::ColumnSort;
use skimrow_config::{ConnectionEntry, SshAuthKind};
use skimrow_db::{ConnSetup, ConnectionConfig, RelationKind, SshAuth, SshConfig, SslMode};

/// Pretty-print SQL: 2-space indent, uppercased keywords. Statements are
/// formatted one at a time and keep the blank lines that were between them —
/// at least one — and a `--` comment on the line a statement ends on stays
/// there. Best-effort — `sqlformat` returns SQL it cannot parse unchanged.
pub(crate) fn format_sql(sql: &str) -> String {
    let opts = sqlformat::FormatOptions {
        uppercase: Some(true),
        // Array syntax (`a[1]`, `ARRAY[…]`) and Postgres operators.
        dialect: sqlformat::Dialect::PostgreSql,
        ..Default::default()
    };
    let format = |stmt: &str| {
        sqlformat::format(stmt, &sqlformat::QueryParams::None, &opts)
            .trim()
            .to_string()
    };
    let mut out = String::new();
    for (start, end) in statement_spans(sql) {
        // The span stops before its `;`; format the statement with it.
        let end = if sql[end..].starts_with(';') {
            end + 1
        } else {
            end
        };
        let mut part = &sql[start..end];
        if !out.is_empty() {
            let rest = part.trim_start_matches([' ', '\t']);
            if rest.starts_with("--") {
                let eol = rest.find('\n').unwrap_or(rest.len());
                out.push(' ');
                out.push_str(rest[..eol].trim_end());
                part = &rest[eol..];
            }
        }
        let body = part.trim();
        if body.is_empty() || body == ";" {
            continue;
        }
        if !out.is_empty() {
            // The first line break ends the previous statement's line; any
            // more are blank lines.
            let lead = &part[..part.len() - part.trim_start().len()];
            let blank = lead.matches('\n').count().saturating_sub(1).max(1);
            out.push_str(&"\n".repeat(blank + 1));
        }
        out.push_str(&format(body));
    }
    if sql.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out
}

/// Wrap a query so it is ordered by one of its result columns. `Default` clears
/// the sort (returns the base query unchanged).
///
/// A trailing `LIMIT n` is hoisted OUTSIDE the wrapper: table-browse queries
/// end in `LIMIT 500`, and ordering inside the limit would sort only the
/// already-fetched page instead of the whole table (ORDER BY must run before
/// LIMIT to pick the top-n of all rows).
pub(crate) fn order_by_sql(base: &str, col: &str, sort: ColumnSort) -> String {
    let q = base.trim().trim_end_matches(';').trim_end();
    let ident = col.replace('"', "\"\"");
    let dir = match sort {
        ColumnSort::Default => return base.to_string(),
        ColumnSort::Ascending => "ASC",
        ColumnSort::Descending => "DESC",
    };
    let (inner, limit) = split_trailing_limit(q);
    let tail = limit.map(|n| format!(" LIMIT {n}")).unwrap_or_default();
    format!("SELECT * FROM ({inner}) AS _skimrow ORDER BY \"{ident}\" {dir}{tail}")
}

/// Split a trailing `LIMIT <digits>` off a query, if present. Conservative: the
/// keyword must be preceded by whitespace and followed only by digits, so a
/// `limit` inside an identifier or string literal never matches.
fn split_trailing_limit(q: &str) -> (&str, Option<&str>) {
    let lower = q.to_ascii_lowercase();
    if let Some(pos) = lower.rfind("limit") {
        let count = q[pos + 5..].trim();
        let preceded_by_space = q[..pos].ends_with(|ch: char| ch.is_whitespace());
        if preceded_by_space && !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit()) {
            return (q[..pos].trim_end(), Some(count));
        }
    }
    (q, None)
}

/// `n` with thousands separators (`10000000` → `10,000,000`).
pub(crate) fn grouped(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() * 4 / 3);
    for (i, d) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(d);
    }
    out
}

/// An `ORDER BY` list with every item's direction flipped (`a, b DESC NULLS
/// LAST` → `a DESC, b ASC NULLS FIRST`), so the end of the ordering can be read
/// from its start. Flipping only the direction also flips where NULLs go by
/// default, which is what reversing needs. `None` when it can't be done
/// safely: empty text or item, a `USING` operator, a comment, a `$$` string.
pub(crate) fn reverse_order(order: &str) -> Option<String> {
    if order.contains("--") || order.contains("/*") || order.contains("$$") {
        return None;
    }
    // Split on top-level commas (not inside parens or quotes).
    let mut items = Vec::new();
    let (mut depth, mut quote, mut start) = (0i32, None, 0);
    for (i, ch) in order.char_indices() {
        match (quote, ch) {
            (Some(q), _) if ch == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(ch),
            (None, '(' | '[') => depth += 1,
            (None, ')' | ']') => depth -= 1,
            (None, ',') if depth == 0 => {
                items.push(&order[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if quote.is_some() || depth != 0 {
        return None;
    }
    items.push(&order[start..]);
    items
        .into_iter()
        .map(reverse_item)
        .collect::<Option<Vec<_>>>()
        .map(|v| v.join(", "))
}

/// One `ORDER BY` item, reversed (see `reverse_order`).
fn reverse_item(item: &str) -> Option<String> {
    let item = item.trim();
    // Each word with the byte offset it starts at, to cut the expression off
    // before its trailing keywords without touching its own spacing.
    let words: Vec<(usize, &str)> = item
        .split_whitespace()
        .map(|w| (w.as_ptr() as usize - item.as_ptr() as usize, w))
        .collect();
    let is = |w: &str, kw: &str| w.eq_ignore_ascii_case(kw);
    if words.iter().any(|(_, w)| is(w, "using")) {
        return None;
    }
    let mut end = words.len();
    let mut nulls = "";
    if end >= 3 && is(words[end - 2].1, "nulls") {
        nulls = match words[end - 1].1 {
            w if is(w, "first") => " NULLS LAST",
            w if is(w, "last") => " NULLS FIRST",
            _ => return None,
        };
        end -= 2;
    }
    let mut dir = " DESC";
    if end >= 2 && (is(words[end - 1].1, "asc") || is(words[end - 1].1, "desc")) {
        if is(words[end - 1].1, "desc") {
            dir = " ASC";
        }
        end -= 1;
    }
    let cut = words.get(end).map_or(item.len(), |&(at, _)| at);
    let expr = item[..cut].trim_end();
    (!expr.is_empty()).then(|| format!("{expr}{dir}{nulls}"))
}

/// The statements of `sql` as byte spans, split on top-level `;` (each span
/// stops before its `;`; the last runs to the end). Honors single/double
/// quotes, line (`--`) and block (`/* */`) comments, and dollar-quoted bodies
/// (`$$ … $$`, `$tag$ … $tag$`).
fn statement_spans(sql: &str) -> Vec<(usize, usize)> {
    let b = sql.as_bytes();
    let n = b.len();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < n {
        match b[i] {
            b'\'' | b'"' => {
                let q = b[i];
                i += 1;
                while i < n {
                    if b[i] == q {
                        // doubled quote = escaped, stays inside the string
                        if i + 1 < n && b[i + 1] == q {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'-' if i + 1 < n && b[i + 1] == b'-' => {
                i += 2;
                while i < n && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < n && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(n);
            }
            b'$' => match dollar_tag(&b[i..]) {
                Some(taglen) => {
                    let tag = &b[i..i + taglen];
                    i += taglen;
                    while i < n {
                        if b[i] == b'$' && b[i..].starts_with(tag) {
                            i += taglen;
                            break;
                        }
                        i += 1;
                    }
                }
                None => i += 1,
            },
            b';' => {
                ranges.push((start, i));
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    ranges.push((start, n));
    ranges
}

/// Byte range of the single SQL statement containing byte offset `cursor`,
/// trimmed (split as `statement_spans` does). Falls back to the whole
/// (trimmed) input when the picked statement is empty. A range rather than the text, so an error the server
/// reports in it can be put back on the editor text.
pub(crate) fn statement_range(sql: &str, cursor: usize) -> std::ops::Range<usize> {
    let n = sql.len();
    let ranges = statement_spans(sql);
    let cur = cursor.min(n);
    let (s, e) = ranges
        .iter()
        .find(|(s, e)| cur >= *s && cur <= *e)
        .copied()
        .unwrap_or((0, n));
    let trimmed = |s: usize, e: usize| {
        let part = &sql[s..e];
        let start = s + (part.len() - part.trim_start().len());
        start..start + part.trim().len()
    };
    let stmt = trimmed(s, e);
    if stmt.is_empty() {
        trimmed(0, n)
    } else {
        stmt
    }
}

/// What Run (and Explain) take from an editor: the selection when one holds
/// any SQL (trimmed, a final `;` too, as `statement_range` leaves it out — it
/// may span several statements, which run as one submission), else the
/// statement under the cursor.
pub(crate) fn run_range(
    sql: &str,
    selected: std::ops::Range<usize>,
    cursor: usize,
) -> std::ops::Range<usize> {
    if let Some(part) = sql.get(selected.clone()) {
        let start = selected.start + (part.len() - part.trim_start().len());
        let len = part
            .trim()
            .trim_end_matches(|c: char| c == ';' || c.is_whitespace())
            .len();
        if len > 0 {
            return start..start + len;
        }
    }
    statement_range(sql, cursor)
}

/// `stmt` from its first token on: leading whitespace and comments dropped
/// (empty for a blank / comment-only statement, which the server skips).
fn skip_comments(stmt: &str) -> &str {
    let mut rest = stmt;
    loop {
        rest = rest.trim_start();
        if let Some(r) = rest.strip_prefix("--") {
            rest = r.find('\n').map_or("", |i| &r[i..]);
        } else if let Some(r) = rest.strip_prefix("/*") {
            rest = r.find("*/").map_or("", |i| &r[i + 2..]);
        } else {
            return rest;
        }
    }
}

/// The statements of a submission as the server runs them, in order:
/// `statement_spans` minus the blank / comment-only ones (it skips those, so
/// they get no result of their own).
pub(crate) fn statements(sql: &str) -> Vec<&str> {
    statement_spans(sql)
        .into_iter()
        .map(|(s, e)| &sql[s..e])
        .filter(|stmt| !skip_comments(stmt).is_empty())
        .collect()
}

/// What a statement is, for its line under the editor: its leading keyword
/// (`UPDATE`), and for a schema change the kind of object too (`CREATE
/// TABLE`, `DROP INDEX`) — roughly Postgres' command tag, which the driver
/// doesn't pass on. Empty when it starts with no keyword.
pub(crate) fn command_tag(stmt: &str) -> String {
    let mut words = Vec::new();
    let mut rest = skip_comments(stmt);
    while words.len() < 8 {
        rest = skip_comments(rest);
        let end = rest
            .find(|c: char| !(c.is_ascii_alphabetic() || c == '_'))
            .unwrap_or(rest.len());
        if end == 0 {
            break;
        }
        words.push(rest[..end].to_ascii_uppercase());
        rest = &rest[end..];
    }
    let mut words = words.into_iter();
    let Some(mut tag) = words.next() else {
        return String::new();
    };
    if matches!(tag.as_str(), "CREATE" | "ALTER" | "DROP") {
        const MODIFIERS: &[&str] = &[
            "OR",
            "REPLACE",
            "UNIQUE",
            "TEMP",
            "TEMPORARY",
            "UNLOGGED",
            "GLOBAL",
            "LOCAL",
            "TRUSTED",
            "PROCEDURAL",
            "RECURSIVE",
        ];
        let mut words = words.skip_while(|w| MODIFIERS.contains(&w.as_str()));
        if let Some(object) = words.next() {
            let second: &[&str] = match object.as_str() {
                "MATERIALIZED" => &["VIEW"],
                "FOREIGN" => &["TABLE", "DATA"],
                "EVENT" => &["TRIGGER"],
                "TEXT" => &["SEARCH"],
                "ACCESS" => &["METHOD"],
                "USER" => &["MAPPING"],
                "OPERATOR" => &["CLASS", "FAMILY"],
                _ => &[],
            };
            tag = format!("{tag} {object}");
            if let Some(next) = words.next().filter(|w| second.contains(&w.as_str())) {
                tag = format!("{tag} {next}");
            }
        }
    }
    tag
}

/// Whether a statement tagged `tag` (`command_tag`) reports how many rows
/// it changed — the ones Postgres gives a row count in their command tag.
pub(crate) fn counts_rows(tag: &str) -> bool {
    matches!(
        tag.split(' ').next().unwrap_or(""),
        "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "COPY" | "SELECT" | "WITH" | "FETCH" | "MOVE"
    )
}

/// A statement's run time for its message: `12 ms`, `3.40 s`.
pub(crate) fn took(d: std::time::Duration) -> String {
    match d.as_millis() {
        0 => "<1 ms".into(),
        ms if ms < 1000 => format!("{ms} ms"),
        _ => format!("{:.2} s", d.as_secs_f64()),
    }
}

/// Where byte offset `at` of `old` lands in `new`, a reformatting of it (the
/// same tokens, other whitespace and keyword case — `format_sql`): right after
/// the same character it followed, or — when it stood before a token or in
/// whitespace — right before that token. The two texts are lined up by their
/// non-whitespace characters, skipping a few that one side dropped (an empty
/// statement's `;`) so the rest still lines up.
pub(crate) fn map_offset(old: &str, new: &str, at: usize) -> usize {
    let solid = |s: &str| -> Vec<(usize, char)> {
        s.char_indices()
            .filter(|(_, ch)| !ch.is_whitespace())
            .collect()
    };
    let (o, n) = (solid(old), solid(new));
    let same = |a: char, b: char| a.eq_ignore_ascii_case(&b);
    // `to[i]`: the character of `new` that `old`'s i-th one became.
    let mut to = vec![None; o.len()];
    let (mut i, mut j) = (0, 0);
    while i < o.len() && j < n.len() {
        if same(o[i].1, n[j].1) {
            to[i] = Some(j);
            i += 1;
            j += 1;
        } else if let Some(k) = (1..=4).find(|&k| o.get(i + k).is_some_and(|c| same(c.1, n[j].1))) {
            i += k;
        } else if let Some(k) = (1..=4).find(|&k| n.get(j + k).is_some_and(|c| same(o[i].1, c.1))) {
            j += k;
        } else {
            to[i] = Some(j);
            i += 1;
            j += 1;
        }
    }
    let at = at.min(old.len());
    // How many of `old`'s characters come before `at`.
    let before = o.partition_point(|&(b, _)| b < at);
    let after_token = old[..at]
        .chars()
        .next_back()
        .is_some_and(|c| !c.is_whitespace());
    if after_token {
        // Right after the last character before it that has a counterpart.
        (0..before)
            .rev()
            .find_map(|k| to[k])
            .map_or(0, |j| n[j].0 + n[j].1.len_utf8())
    } else {
        // Right before the next one that has.
        (before..o.len())
            .find_map(|k| to[k])
            .map_or(new.len(), |j| n[j].0)
    }
}

/// The byte span of `sql` to underline for a server error at 1-based
/// character `pos` (Postgres' error position): the token that starts there —
/// a word, a quoted name / string, or one symbol. An error at the very end
/// ("syntax error at end of input") marks the last token instead. Empty when
/// `sql` is blank.
pub(crate) fn error_span(sql: &str, pos: u32) -> std::ops::Range<usize> {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut at = sql
        .char_indices()
        .nth(pos.saturating_sub(1) as usize)
        .map_or(sql.len(), |(i, _)| i);
    if sql[at..].trim().is_empty() {
        let head = sql[..at].trim_end();
        let Some(last) = head.chars().next_back() else {
            return 0..0;
        };
        at = if word(last) {
            head.rfind(|c: char| !word(c)).map_or(0, |i| i + 1)
        } else {
            head.len() - last.len_utf8()
        };
    }
    let rest = &sql[at..];
    let first = rest.chars().next().unwrap_or(' ');
    let len = match first {
        c if word(c) => rest.find(|c: char| !word(c)).unwrap_or(rest.len()),
        '"' | '\'' => rest[1..].find(first).map_or(rest.len(), |i| i + 2),
        c => c.len_utf8(),
    };
    at..at + len
}

/// If `b` begins a dollar-quote opening tag (`$`, optional `[A-Za-z0-9_]*`, `$`),
/// return the tag's byte length; else `None`.
fn dollar_tag(b: &[u8]) -> Option<usize> {
    if b.first() != Some(&b'$') {
        return None;
    }
    let mut j = 1;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
        j += 1;
    }
    (j < b.len() && b[j] == b'$').then_some(j + 1)
}

/// Collapse a (possibly multi-line) SQL string to a single trimmed line.
pub(crate) fn oneline(sql: &str) -> String {
    let s: String = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.len() > 120 {
        format!("{}…", &s[..120])
    } else {
        s
    }
}

pub(crate) fn ssl_from_str(s: &str) -> SslMode {
    match s.trim() {
        "disable" => SslMode::Disable,
        "require" => SslMode::Require,
        "verify-ca" => SslMode::VerifyCa,
        "verify-full" => SslMode::VerifyFull,
        _ => SslMode::Prefer,
    }
}

/// The config for a saved connection, with its pool setup. Fails on a bad
/// parameter line (settings.json is hand-editable), naming the connection.
pub(crate) fn entry_to_config(
    entry: &ConnectionEntry,
    password: Option<String>,
    ssh_secret: Option<String>,
) -> Result<ConnectionConfig, String> {
    let mut cfg = entry_config(entry, password, ssh_secret);
    cfg.setup = ConnSetup::parse(&entry.params.join("\n"), &entry.init_sql)
        .map_err(|e| format!("{}: parameters, {e}", entry.name))?;
    Ok(cfg)
}

/// The config for a saved connection, without its pool setup. `ssh_secret` is
/// its SSH password or key passphrase.
pub(crate) fn entry_config(
    entry: &ConnectionEntry,
    password: Option<String>,
    ssh_secret: Option<String>,
) -> ConnectionConfig {
    let mut cfg = ConnectionConfig::new(
        entry.name.clone(),
        entry.host.clone(),
        entry.dbname.clone(),
        entry.user.clone(),
    );
    cfg.port = entry.port;
    cfg.ssl_mode = ssl_from_str(&entry.ssl_mode);
    cfg.password = password;
    cfg.ssh = entry.ssh.as_ref().map(|ssh| SshConfig {
        host: ssh.host.clone(),
        port: ssh.port,
        user: ssh.user.clone(),
        auth: match ssh.auth {
            SshAuthKind::Key => SshAuth::Key(ssh.key_file.trim().into()),
            SshAuthKind::Password => SshAuth::Password,
            SshAuthKind::Agent => SshAuth::Agent,
        },
        secret: ssh_secret,
        known_hosts: skimrow_config::known_hosts_files(),
    });
    cfg
}

/// OS window title: the active connection name plus the app name, or just the
/// app name when not connected. This is what the taskbar / alt-tab show, so it
/// has to carry the connection even though our custom title bar draws it inside
/// the client area (which the OS cannot see).
pub(crate) fn window_title(conn_name: Option<&str>) -> String {
    match conn_name {
        Some(n) if !n.trim().is_empty() => format!("{} — skimrow", n.trim()),
        _ => "skimrow".into(),
    }
}

pub(crate) fn rel_icon(kind: RelationKind) -> &'static str {
    match kind {
        RelationKind::Table => "icons/table.svg",
        RelationKind::View | RelationKind::MaterializedView => "icons/eye.svg",
        RelationKind::ForeignTable => "icons/database.svg",
        RelationKind::Other => "icons/table.svg",
    }
}

/// How long a wait goes unlabelled: its loader is a bare spinner until then.
pub(crate) const LABEL_AFTER: std::time::Duration = std::time::Duration::from_secs(5);

/// A slow wait's loader label — `"Running query · 12s"` — once
/// `LABEL_AFTER` has passed; `None` before.
pub(crate) fn wait_label(what: &str, elapsed: std::time::Duration) -> Option<String> {
    (elapsed >= LABEL_AFTER).then(|| format!("{what} · {}s", elapsed.as_secs()))
}

/// The values Settings' connect-timeout stepper walks through (seconds;
/// 0 = no limit).
pub(crate) const CONNECT_TIMEOUT_STEPS: &[u64] = &[0, 5, 10, 15, 30, 60, 120, 300, 600];

/// The stepper's neighbours of `secs` (which a hand-edited settings.json may
/// have put between steps): the step below and the step above, if any.
pub(crate) fn timeout_steps(secs: u64) -> (Option<u64>, Option<u64>) {
    let less = CONNECT_TIMEOUT_STEPS
        .iter()
        .rev()
        .find(|&&s| s < secs)
        .copied();
    let more = CONNECT_TIMEOUT_STEPS.iter().find(|&&s| s > secs).copied();
    (less, more)
}

#[cfg(test)]
mod error_span_tests {
    use super::*;

    #[test]
    fn statement_range_points_into_the_text() {
        let sql = "select 1;\n  select 2 ;\n";
        let r = statement_range(sql, 13);
        assert_eq!(&sql[r.clone()], "select 2");
        assert_eq!(r.start, 12);
        assert_eq!(statement_range("  x  ", 0), 2..3);
    }

    #[test]
    fn marks_the_token_postgres_points_at() {
        let sql = "SELECT nme FROM t";
        assert_eq!(&sql[error_span(sql, 8)], "nme");
        let sql = "SELECT * FROM \"Bad Name\"";
        assert_eq!(&sql[error_span(sql, 15)], "\"Bad Name\"");
        let sql = "SELECT 1 +* 2";
        assert_eq!(&sql[error_span(sql, 11)], "*");
        // "syntax error at end of input": the last token.
        let sql = "SELECT * FROM";
        assert_eq!(&sql[error_span(sql, 14)], "FROM");
        let sql = "SELECT (1 ";
        assert_eq!(&sql[error_span(sql, 11)], "1");
        assert_eq!(error_span("", 1), 0..0);
        // Positions count characters, not bytes.
        let sql = "SELECT 'ä' + nope";
        assert_eq!(&sql[error_span(sql, 14)], "nope");
    }
}

#[cfg(test)]
mod run_text_tests {
    use super::*;

    /// A selection runs as it is (trimmed, several statements and all); an
    /// empty or blank one falls back to the statement under the cursor.
    #[test]
    fn run_range_prefers_the_selection() {
        let sql = "select 1;\nupdate t set a = 1;\nselect 2;";
        let whole = 0..sql.len();
        assert_eq!(
            &sql[run_range(sql, whole.clone(), 3)],
            "select 1;\nupdate t set a = 1;\nselect 2"
        );
        let r = run_range(sql, 7..9, 0);
        assert_eq!(&sql[r], "1");
        // Whitespace around the selection is dropped, not run.
        let r = run_range(sql, 9..29, 0);
        assert_eq!(r.start, 10);
        assert_eq!(&sql[r], "update t set a = 1");
        // Nothing (or only whitespace) selected: the cursor's statement.
        assert_eq!(&sql[run_range(sql, 12..12, 12)], "update t set a = 1");
        assert_eq!(&sql[run_range(sql, 9..10, 25)], "update t set a = 1");
        assert_eq!(&sql[run_range(sql, 8..9, 25)], "update t set a = 1");
    }

    #[test]
    fn statements_skip_blank_and_comment_only_ones() {
        let sql = "-- head\nselect 1; ;\n/* c */ ;update t set a = ';';\n-- tail";
        assert_eq!(
            statements(sql),
            vec!["-- head\nselect 1", "update t set a = ';'"]
        );
    }

    #[test]
    fn command_tags_name_the_statement() {
        assert_eq!(command_tag("  update t set a = 1"), "UPDATE");
        assert_eq!(
            command_tag("-- why\n/* x */ insert into t values (1)"),
            "INSERT"
        );
        assert_eq!(command_tag("create table t (a int)"), "CREATE TABLE");
        assert_eq!(
            command_tag("CREATE OR REPLACE FUNCTION f() …"),
            "CREATE FUNCTION"
        );
        assert_eq!(
            command_tag("create unique index i on t (a)"),
            "CREATE INDEX"
        );
        assert_eq!(command_tag("drop table if exists t"), "DROP TABLE");
        assert_eq!(
            command_tag("create materialized view v as select 1"),
            "CREATE MATERIALIZED VIEW"
        );
        assert_eq!(
            command_tag("create user mapping for x"),
            "CREATE USER MAPPING"
        );
        assert_eq!(command_tag("create user bob"), "CREATE USER");
        assert_eq!(command_tag("begin"), "BEGIN");
        assert_eq!(command_tag("(select 1)"), "");
        assert!(counts_rows("UPDATE") && counts_rows("DELETE") && counts_rows("WITH"));
        assert!(!counts_rows("CREATE TABLE") && !counts_rows("BEGIN"));
    }

    #[test]
    fn took_reads_like_a_duration() {
        use std::time::Duration;
        assert_eq!(took(Duration::from_micros(300)), "<1 ms");
        assert_eq!(took(Duration::from_millis(42)), "42 ms");
        assert_eq!(took(Duration::from_millis(3400)), "3.40 s");
    }

    /// Format keeps the cursor by the same token, wherever the formatter moved it.
    #[test]
    fn format_keeps_the_cursor_on_its_token() {
        let old = "select a, b from t where x = 1;\n\nupdate t set a = 2 where id = 3;\n";
        let new = format_sql(old);
        let at = |s: &str, pat: &str| s.find(pat).unwrap();
        // Inside a word: the same spot in it.
        let o = at(old, "where x") + 3;
        let n = map_offset(old, &new, o);
        assert_eq!(n, at(&new, "WHERE") + 3, "{new}");
        // Before a token (in the whitespace before it): before that token.
        let o = at(old, "update") - 1;
        assert!(new[map_offset(old, &new, o)..].starts_with("UPDATE"));
        // After a token: right after it.
        let o = at(old, "id = 3") + 2;
        assert!(new[..map_offset(old, &new, o)].ends_with("id"));
        // The ends stay the ends.
        assert_eq!(map_offset(old, &new, 0), 0);
        assert_eq!(map_offset(old, &new, old.len()), new.len());
    }

    /// A character one side dropped (an empty statement's `;`) doesn't throw
    /// off where everything after it lands.
    #[test]
    fn map_offset_survives_dropped_characters() {
        let old = "select 1;;\nselect 22";
        let new = "SELECT\n  1;\n\nSELECT\n  22";
        let o = old.rfind("22").unwrap() + 1;
        let n = map_offset(old, new, o);
        assert_eq!(&new[n - 1..=n], "22");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_component::table::ColumnSort;
    use skimrow_db::SslMode;

    #[test]
    fn wait_label_appears_after_five_seconds() {
        use std::time::Duration;
        assert_eq!(
            wait_label("Running query", Duration::from_millis(4999)),
            None
        );
        assert_eq!(
            wait_label("Running query", Duration::from_millis(5000)).as_deref(),
            Some("Running query · 5s")
        );
        assert_eq!(
            wait_label("Connecting to prod", Duration::from_secs(72)).as_deref(),
            Some("Connecting to prod · 72s")
        );
    }

    #[test]
    fn timeout_stepper_walks_the_steps() {
        assert_eq!(timeout_steps(0), (None, Some(5)));
        assert_eq!(timeout_steps(30), (Some(15), Some(60)));
        assert_eq!(timeout_steps(600), (Some(300), None));
        // Hand-edited between two steps: its neighbours.
        assert_eq!(timeout_steps(45), (Some(30), Some(60)));
    }

    #[test]
    fn format_keeps_the_blank_lines_between_statements() {
        assert_eq!(format_sql("select 1"), "SELECT\n  1");
        // Same line or the next one: still a blank line between them.
        assert_eq!(
            format_sql("select 1; select 2;"),
            "SELECT\n  1;\n\nSELECT\n  2;"
        );
        assert_eq!(
            format_sql("select 1;\nselect 2;"),
            "SELECT\n  1;\n\nSELECT\n  2;"
        );
        // More blank lines than that stay as they were.
        assert_eq!(
            format_sql("select 1;\n\n\n\nselect 2;\n"),
            "SELECT\n  1;\n\n\n\nSELECT\n  2;\n"
        );
        // A comment ending a statement's line stays on it; one above the
        // next statement stays above it.
        assert_eq!(
            format_sql("select 1; -- one\n\n-- two\nselect 2"),
            "SELECT\n  1; -- one\n\n-- two\nSELECT\n  2"
        );
        // A `;` inside a string or a function body doesn't split.
        let f = format_sql(
            "select ';'; create function f() returns int as $$ select 1; $$ language sql;",
        );
        assert!(f.starts_with("SELECT\n  ';';\n\nCREATE FUNCTION"), "{f}");
        assert_eq!(f.matches("\n\n").count(), 1, "{f}");
        assert_eq!(format_sql("  \n "), "");
    }

    fn statement_at(sql: &str, cursor: usize) -> &str {
        &sql[statement_range(sql, cursor)]
    }

    #[test]
    fn groups_thousands() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(9999501), "9,999,501");
        assert_eq!(grouped(10000000), "10,000,000");
    }

    #[test]
    fn reverses_order_by_lists() {
        assert_eq!(reverse_order("\"id\"").as_deref(), Some("\"id\" DESC"));
        assert_eq!(
            reverse_order("name, id desc").as_deref(),
            Some("name DESC, id ASC")
        );
        assert_eq!(
            reverse_order("a ASC NULLS FIRST, b nulls last").as_deref(),
            Some("a DESC NULLS LAST, b DESC NULLS FIRST")
        );
        // Commas / keywords inside calls, quotes and collations stay put.
        assert_eq!(
            reverse_order("coalesce(a, b)  DESC, \"x desc\", lower(n) COLLATE \"C\"").as_deref(),
            Some("coalesce(a, b) ASC, \"x desc\" DESC, lower(n) COLLATE \"C\" DESC")
        );
        assert_eq!(
            reverse_order("'a,b' || c").as_deref(),
            Some("'a,b' || c DESC")
        );
        for unsafe_or_bad in [
            "",
            "a,",
            "a using <",
            "a -- note",
            "f(a",
            "a nulls sometimes",
        ] {
            assert_eq!(reverse_order(unsafe_or_bad), None, "{unsafe_or_bad}");
        }
    }

    #[test]
    fn window_title_reflects_connection() {
        assert_eq!(window_title(Some("Local dev")), "Local dev — skimrow");
        assert_eq!(window_title(None), "skimrow");
        assert_eq!(window_title(Some("   ")), "skimrow");
        assert_eq!(window_title(Some(" prod ")), "prod — skimrow");
    }

    #[test]
    fn single_statement_returned_whole() {
        assert_eq!(statement_at("select 1", 3), "select 1");
        assert_eq!(statement_at("select 1", 0), "select 1");
    }

    #[test]
    fn picks_statement_by_cursor() {
        let sql = "select 1; select 2; select 3";
        assert_eq!(statement_at(sql, 2), "select 1"); // in first
        assert_eq!(statement_at(sql, 12), "select 2"); // in second
        assert_eq!(statement_at(sql, 28), "select 3"); // in third
    }

    #[test]
    fn semicolon_in_string_is_not_a_split() {
        let sql = "select ';' as a; select 2";
        assert_eq!(statement_at(sql, 3), "select ';' as a");
        assert_eq!(statement_at(sql, 20), "select 2");
    }

    #[test]
    fn semicolon_in_comment_is_not_a_split() {
        let sql = "select 1 -- ; not a split\n; select 2";
        assert_eq!(statement_at(sql, 3), "select 1 -- ; not a split");
        assert_eq!(statement_at(sql, 30), "select 2");
    }

    #[test]
    fn block_comment_semicolon_ignored() {
        let sql = "select 1 /* ; */ + 2; select 9";
        assert_eq!(statement_at(sql, 3), "select 1 /* ; */ + 2");
    }

    #[test]
    fn dollar_quoted_body_not_split() {
        let sql = "create function f() returns int as $$ begin; return 1; end; $$ language plpgsql; select 2";
        assert!(statement_at(sql, 10).starts_with("create function"));
        assert!(statement_at(sql, 10).contains("$$ language plpgsql"));
        assert_eq!(statement_at(sql, sql.len() - 1), "select 2");
    }
    #[test]
    fn ssl_parsing() {
        assert_eq!(ssl_from_str("disable"), SslMode::Disable);
        assert_eq!(ssl_from_str("verify-full"), SslMode::VerifyFull);
        assert_eq!(ssl_from_str("whatever"), SslMode::Prefer);
    }

    #[test]
    fn oneline_collapses_and_truncates() {
        assert_eq!(oneline("SELECT\n  1"), "SELECT 1");
        assert_eq!(oneline(&"x ".repeat(200)).chars().last(), Some('…'));
    }

    #[test]
    fn order_by_wraps_query() {
        assert_eq!(
            order_by_sql("SELECT * FROM t", "qty", ColumnSort::Descending),
            r#"SELECT * FROM (SELECT * FROM t) AS _skimrow ORDER BY "qty" DESC"#
        );
        // Default clears the sort.
        assert_eq!(
            order_by_sql("SELECT 1;", "x", ColumnSort::Default),
            "SELECT 1;"
        );
    }

    #[test]
    fn order_by_hoists_trailing_limit() {
        // The LIMIT moves outside the ORDER BY so the whole table is sorted,
        // not just the previously fetched page.
        assert_eq!(
            order_by_sql(
                "SELECT * FROM \"public\".\"t\" LIMIT 500",
                "qty",
                ColumnSort::Ascending
            ),
            r#"SELECT * FROM (SELECT * FROM "public"."t") AS _skimrow ORDER BY "qty" ASC LIMIT 500"#
        );
        // WHERE filter stays inside; LIMIT still hoisted.
        assert_eq!(
            order_by_sql(
                "SELECT * FROM t WHERE id > 5 LIMIT 500",
                "id",
                ColumnSort::Descending
            ),
            r#"SELECT * FROM (SELECT * FROM t WHERE id > 5) AS _skimrow ORDER BY "id" DESC LIMIT 500"#
        );
        // A non-numeric or embedded "limit" is left alone.
        assert_eq!(
            order_by_sql("SELECT 'limit 5'", "a", ColumnSort::Ascending),
            r#"SELECT * FROM (SELECT 'limit 5') AS _skimrow ORDER BY "a" ASC"#
        );
        assert_eq!(
            order_by_sql("SELECT * FROM t LIMIT all", "a", ColumnSort::Ascending),
            r#"SELECT * FROM (SELECT * FROM t LIMIT all) AS _skimrow ORDER BY "a" ASC"#
        );
    }
}
