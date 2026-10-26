//! Line-based editing of `Host` blocks that keeps comments and other keywords intact.

use std::fs;
use std::io;
use std::path::Path;

use super::parser::{parse_line, parse_patterns, strip_comment};
use super::EntryType;

/// Keywords edited by the host form, in form order; index 0 is the `Host` line itself.
pub const FIELDS: [(EntryType, &str); 5] = [
    (EntryType::Host, "Host"),
    (EntryType::Hostname, "HostName"),
    (EntryType::User, "User"),
    (EntryType::Port, "Port"),
    (EntryType::IdentityFile, "IdentityFile"),
];

/// Values in `FIELDS` order. An empty string means the keyword is absent. Index 0 is the raw `Host` value, e.g. `"my Lab" lab2`.
pub type HostFields = [String; 5];

fn keyword(line: &str) -> Option<(EntryType, String)> {
    parse_line(strip_comment(line).trim()).ok()
}

fn stale() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "the file changed since it was loaded. Press Ctrl+R to reload.",
    )
}

/// Guards against writing into the wrong block after an outside edit.
fn check_host_line(lines: &[String], host_line: usize, name: &str) -> io::Result<()> {
    match lines.get(host_line).and_then(|line| keyword(line)) {
        Some((EntryType::Host, value)) if parse_patterns(&value).iter().any(|p| p == name) => {
            Ok(())
        }
        _ => Err(stale()),
    }
}

fn block_end(lines: &[String], host_line: usize) -> usize {
    (host_line + 1..lines.len())
        .find(|&i| {
            matches!(
                keyword(&lines[i]),
                Some((EntryType::Host | EntryType::Match, _))
            )
        })
        .unwrap_or(lines.len())
}

fn quote(value: &str) -> String {
    if value.contains(|c: char| c.is_whitespace() || c == '#') {
        format!("\"{value}\"")
    } else {
        value.to_string()
    }
}

fn leading_whitespace(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// The line's trailing comment as ` # …`, or an empty string.
fn comment_tail(line: &str) -> String {
    let comment = &line[strip_comment(line).len()..];
    if comment.is_empty() {
        String::new()
    } else {
        format!(" {comment}")
    }
}

fn load(path: &Path) -> io::Result<(Vec<String>, &'static str)> {
    let content = fs::read_to_string(path)?;
    let newline = if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    Ok((content.lines().map(str::to_string).collect(), newline))
}

fn save(path: &Path, lines: &[String], newline: &str) -> io::Result<()> {
    // Include paths are not canonical, and renaming onto a symlink would replace it with a plain file.
    let canonical = fs::canonicalize(path)?;
    let mut tmp = canonical.clone().into_os_string();
    tmp.push(".sshs-tmp");

    let result = fs::write(&tmp, lines.join(newline) + newline)
        .and_then(|()| fs::set_permissions(&tmp, fs::metadata(&canonical)?.permissions()))
        .and_then(|()| fs::rename(&tmp, &canonical));
    if result.is_err() {
        fs::remove_file(&tmp).ok();
    }
    result
}

/// Reads the block's own values (not inherited ones) for the keywords in `FIELDS`.
///
/// # Errors
///
/// Will return `Err` if the file cannot be read, or if line `host_line` is no longer a `Host` line listing `name`.
pub fn read_block(path: &Path, host_line: usize, name: &str) -> io::Result<HostFields> {
    let (lines, _) = load(path)?;
    check_host_line(&lines, host_line, name)?;
    let end = block_end(&lines, host_line);

    Ok(std::array::from_fn(|i| {
        let range = if i == 0 {
            host_line..host_line + 1
        } else {
            host_line + 1..end
        };
        lines[range]
            .iter()
            .filter_map(|line| keyword(line))
            .find(|(entry_type, _)| *entry_type == FIELDS[i].0)
            .map(|(_, value)| value)
            .unwrap_or_default()
    }))
}

/// Rewrites the `Host` block at `host_line`, touching only lines whose value changed.
///
/// # Errors
///
/// Will return `Err` if the file cannot be read or written, or if line `host_line` is no longer a `Host` line listing `name`.
pub fn write_block(
    path: &Path,
    host_line: usize,
    name: &str,
    fields: &HostFields,
) -> io::Result<()> {
    let (mut lines, newline) = load(path)?;
    check_host_line(&lines, host_line, name)?;
    let end = block_end(&lines, host_line);

    let indent = (host_line + 1..end)
        .find(|&i| keyword(&lines[i]).is_some())
        .map_or("  ", |i| leading_whitespace(&lines[i]))
        .to_string();

    let host = &lines[host_line];
    if keyword(host).map(|(_, value)| value).as_deref() != Some(fields[0].as_str()) {
        lines[host_line] = format!(
            "{}Host {}{}",
            leading_whitespace(host),
            fields[0],
            comment_tail(host)
        );
    }

    let mut dropped = Vec::new();
    let mut inserts = Vec::new();
    for i in 1..FIELDS.len() {
        let (entry_type, label) = &FIELDS[i];
        let value = &fields[i];
        // shortcut: only the first occurrence of a repeated keyword is edited (IdentityFile repeats are cumulative in OpenSSH); others stay untouched.
        let found = (host_line + 1..end).find_map(|pos| {
            keyword(&lines[pos])
                .filter(|(t, _)| t == entry_type)
                .map(|(_, current)| (pos, current))
        });
        match found {
            Some((pos, _)) if value.is_empty() => dropped.push(pos),
            Some((pos, current)) if current != *value => {
                let line = &lines[pos];
                lines[pos] = format!(
                    "{}{label} {}{}",
                    leading_whitespace(line),
                    quote(value),
                    comment_tail(line)
                );
            }
            None if !value.is_empty() => inserts.push(format!("{indent}{label} {}", quote(value))),
            _ => {}
        }
    }

    let mut output = Vec::with_capacity(lines.len() + inserts.len());
    for (i, line) in lines.into_iter().enumerate() {
        if dropped.contains(&i) {
            continue;
        }
        output.push(line);
        if i == host_line {
            output.append(&mut inserts);
        }
    }
    save(path, &output, newline)
}

/// Adds a new `Host` block before the first `Match` or wildcard `Host` block, or at the end of the file.
///
/// # Errors
///
/// Will return `Err` if the file cannot be read or written.
pub fn append_block(path: &Path, fields: &HostFields) -> io::Result<()> {
    let (lines, newline) = load(path)?;

    let mut block = vec![format!("Host {}", fields[0])];
    for i in 1..FIELDS.len() {
        if !fields[i].is_empty() {
            block.push(format!("  {} {}", FIELDS[i].1, quote(&fields[i])));
        }
    }

    // OpenSSH uses the first value it finds, so a block after `Host *` would be overridden by it.
    let insert_at = lines.iter().position(|line| match keyword(line) {
        Some((EntryType::Match, _)) => true,
        Some((EntryType::Host, value)) => parse_patterns(&value)
            .iter()
            .any(|p| p.contains('*') || p.contains('?')),
        _ => false,
    });

    let mut output = Vec::with_capacity(lines.len() + block.len() + 2);
    if let Some(k) = insert_at {
        output.extend_from_slice(&lines[..k]);
        if k > 0 && !lines[k - 1].trim().is_empty() {
            output.push(String::new());
        }
        output.append(&mut block);
        output.push(String::new());
        output.extend_from_slice(&lines[k..]);
    } else {
        output.extend_from_slice(&lines);
        if lines.last().is_some_and(|line| !line.trim().is_empty()) {
            output.push(String::new());
        }
        output.append(&mut block);
    }
    save(path, &output, newline)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_config(name: &str, content: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("sshs_test_editor_{name}.conf"));
        fs::write(&p, content).unwrap();
        p
    }

    fn fields(values: [&str; 5]) -> HostFields {
        values.map(str::to_string)
    }

    #[test]
    fn test_read_block_returns_own_values() {
        let p = temp_config(
            "read",
            "User g\nHost a\n  IdentityFile \"~/My Keys/id\"\n  Port 22\nHost b\n  HostName b.example.com\n",
        );

        let result = read_block(&p, 1, "a");
        fs::remove_file(&p).ok();

        assert_eq!(result.unwrap(), fields(["a", "", "", "22", "~/My Keys/id"]));
    }

    #[test]
    fn test_write_block_updates_only_changed_lines() {
        let p = temp_config(
            "write",
            "# top\nHost a\n  HostName old  # note\n  User bob\n  ForwardAgent yes\n\nHost b\n  User carol\n",
        );

        let result = write_block(
            &p,
            1,
            "a",
            &fields(["a", "new.example.com", "", "2222", "~/My Keys/id"]),
        );
        let content = fs::read_to_string(&p).unwrap();
        fs::remove_file(&p).ok();

        result.unwrap();
        assert_eq!(
            content,
            "# top\nHost a\n  Port 2222\n  IdentityFile \"~/My Keys/id\"\n  HostName new.example.com # note\n  ForwardAgent yes\n\nHost b\n  User carol\n"
        );
    }

    #[test]
    fn test_write_block_rejects_stale_host_line() {
        let original = "# top\nHost a\n  HostName old  # note\n  User bob\n";
        let p = temp_config("stale", original);

        let result = write_block(&p, 2, "a", &fields(["a", "x", "", "", ""]));
        let content = fs::read_to_string(&p).unwrap();
        fs::remove_file(&p).ok();

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(content, original);
    }

    #[test]
    fn test_append_block_goes_before_wildcard_host() {
        let new_host = fields(["new", "n.example.com", "", "", ""]);
        for (name, before, after) in [
            (
                "wildcard",
                "Host a\n  User x\n\nHost *\n  User root\n",
                "Host a\n  User x\n\nHost new\n  HostName n.example.com\n\nHost *\n  User root\n",
            ),
            (
                "no_newline",
                "Host a\n  User x",
                "Host a\n  User x\n\nHost new\n  HostName n.example.com\n",
            ),
        ] {
            let p = temp_config(name, before);

            let result = append_block(&p, &new_host);
            let content = fs::read_to_string(&p).unwrap();
            fs::remove_file(&p).ok();

            result.unwrap();
            assert_eq!(content, after, "case {name}");
        }
    }
}
