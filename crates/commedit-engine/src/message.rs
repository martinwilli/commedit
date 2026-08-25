//! Commit-message cleanup, mirroring what git does before it writes a commit.
//!
//! `git commit` runs the message through `--cleanup=default` (`strip` when an
//! editor was involved, `whitespace` otherwise): trailing whitespace goes from
//! every line, leading and trailing blank lines go, and the message ends in a
//! single newline. The plumbing (`git commit-tree`) does none of that, and
//! neither does jj-lib — it writes `description` into the object verbatim. So a
//! message arriving straight from a text buffer (the GTK editor) or an MCP
//! argument lands in the object database in a shape git itself would never
//! produce, most visibly without the final newline that every git-made message
//! carries.
//!
//! [`cleanup_message`] closes that gap on the write paths. Two of git's rules
//! are deliberately left out:
//!
//!  * **Comment lines stay.** git drops `#` lines because its editor session
//!    prepends a comment template to strip back off; commedit's editor has no
//!    such template, so a body line like `#1234` is the user's text.
//!  * **Consecutive blank lines stay.** git collapses runs of them. That is the
//!    one rule that can eat deliberate formatting (an indented trace with blank
//!    lines in it), and nothing downstream needs it.

/// Normalize `message` the way git normalizes a commit message: strip trailing
/// whitespace from each line, drop leading and trailing blank lines, and
/// terminate the result with exactly one newline. A message with no content at
/// all cleans to the empty string (jj's "undescribed", never a lone newline).
///
/// Idempotent, so re-saving an already-clean message is a no-op.
pub fn cleanup_message(message: &str) -> String {
    // `lines()` also takes a CRLF's `\r` off, which trailing-whitespace removal
    // would do anyway.
    let lines: Vec<&str> = message.lines().map(str::trim_end).collect();
    let Some(first) = lines.iter().position(|l| !l.is_empty()) else {
        return String::new();
    };
    let last = lines
        .iter()
        .rposition(|l| !l.is_empty())
        .expect("a non-empty line was just found");
    let mut out = lines[first..=last].join("\n");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_whitespace_goes_from_every_line() {
        assert_eq!(
            cleanup_message("subject   \n\nbody with a tab\t\nand a space \n"),
            "subject\n\nbody with a tab\nand a space\n"
        );
    }

    #[test]
    fn leading_and_trailing_blank_lines_go() {
        assert_eq!(cleanup_message("\n  \nsubject\n\n\t\n\n"), "subject\n");
    }

    #[test]
    fn the_message_ends_in_exactly_one_newline() {
        assert_eq!(cleanup_message("subject"), "subject\n");
        assert_eq!(cleanup_message("subject\n"), "subject\n");
        assert_eq!(cleanup_message("subject\n\n\n"), "subject\n");
    }

    #[test]
    fn interior_structure_survives() {
        // Blank runs and leading indentation are the author's formatting: an
        // indented trace keeps its shape, unlike under git's `strip`.
        let body = "subject\n\nbefore:\n\n    a  b\n\n\n    c  d\n\nafter\n";
        assert_eq!(cleanup_message(body), body);
    }

    #[test]
    fn comment_lines_are_the_users_text() {
        assert_eq!(
            cleanup_message("fix #1234\n\n#1235 too\n"),
            "fix #1234\n\n#1235 too\n"
        );
    }

    #[test]
    fn a_message_without_content_stays_empty() {
        assert_eq!(cleanup_message(""), "");
        assert_eq!(cleanup_message("\n\n"), "");
        assert_eq!(cleanup_message("  \t \n "), "");
    }

    #[test]
    fn crlf_line_endings_normalize() {
        assert_eq!(
            cleanup_message("subject\r\n\r\nbody\r\n"),
            "subject\n\nbody\n"
        );
    }

    #[test]
    fn cleaning_a_clean_message_changes_nothing() {
        let clean = cleanup_message("subject\n\nbody\n");
        assert_eq!(cleanup_message(&clean), clean);
    }
}
