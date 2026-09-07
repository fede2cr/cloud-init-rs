//! Line numbers for the dotted paths in a YAML document.
//!
//! `cloud-init schema --annotate` marks the source line each schema error came
//! from. Upstream gets this from a `PyYAML` loader subclass
//! (`cloudinit/safeyaml.py::_CustomSafeLoaderWithMarks`) that records
//! `start_mark` for every key and list item, producing a map from the same
//! dot-delimited paths the validator reports to 1-based line numbers.
//!
//! The port scans the block structure directly rather than instrumenting a
//! parser. It covers block mappings, block sequences, block scalars, and flow
//! collections, which is the whole of what cloud-config is written in.

use std::collections::BTreeMap;

/// Dotted path to the 1-based line where that element begins.
pub type Marks = BTreeMap<String, usize>;

#[derive(Debug)]
struct Frame {
    /// Column at which this container's direct children start.
    child_indent: usize,
    path: String,
    is_seq: bool,
    next_index: usize,
}

/// Map every key and list item in `content` to its starting line.
#[allow(clippy::too_many_lines)]
// The block-structure walk is one loop by nature; splitting it would only move
// the shared cursor state into arguments.
pub fn scan(content: &str) -> Marks {
    let mut marks = Marks::new();
    let mut stack: Vec<Frame> = vec![Frame {
        child_indent: 0,
        path: String::new(),
        is_seq: false,
        next_index: 0,
    }];
    // A `key:` with no inline value opens a container whose indentation is only
    // known once its first child is seen.
    let mut pending: Option<(String, usize)> = None;
    // Inside a `|`/`>` scalar every more-indented line is opaque text.
    let mut block_scalar_indent: Option<usize> = None;

    for (offset, raw) in content.lines().enumerate() {
        let line = offset + 1;
        let indent = raw.len() - raw.trim_start().len();
        let text = raw.trim_start();

        if let Some(owner) = block_scalar_indent {
            if text.is_empty() || indent > owner {
                continue;
            }
            block_scalar_indent = None;
        }
        if text.is_empty() || text.starts_with('#') || text == "---" || text == "..." {
            continue;
        }

        let is_item = strip_seq_marker(text).is_some();
        if let Some((path, key_indent)) = pending.take() {
            let opens_map = indent > key_indent;
            let opens_seq = indent == key_indent && is_item;
            if opens_map || opens_seq {
                stack.push(Frame {
                    child_indent: indent,
                    path,
                    is_seq: is_item,
                    next_index: 0,
                });
            }
        }
        // A sequence written at its parent key's indentation ends as soon as a
        // line at that indentation is not another item.
        while stack.len() > 1
            && stack.last().is_some_and(|f| {
                f.child_indent > indent
                    || (f.child_indent == indent && f.is_seq && !is_item)
            })
        {
            stack.pop();
        }

        let Some(top) = stack.last_mut() else {
            continue;
        };

        if let Some(rest) = strip_seq_marker(text) {
            if !top.is_seq {
                continue;
            }
            let index = top.next_index;
            top.next_index += 1;
            let item_path = join(&top.path, &index.to_string());
            marks.insert(item_path.clone(), line);

            // The `- ` marker is part of the item's indentation.
            let inner_indent = indent + (text.len() - rest.len());
            if is_block_scalar(rest) {
                block_scalar_indent = Some(indent);
            } else if let Some((key, value)) = split_entry(rest) {
                // `- key: value` starts a mapping aligned with `key`.
                let key_path = join(&item_path, key);
                marks.insert(key_path.clone(), line);
                mark_flow(&mut marks, &key_path, value, line);
                stack.push(Frame {
                    child_indent: inner_indent,
                    path: item_path,
                    is_seq: false,
                    next_index: 0,
                });
                if value.is_empty() {
                    pending = Some((key_path, inner_indent));
                } else if is_block_scalar(value) {
                    block_scalar_indent = Some(inner_indent);
                }
            } else {
                mark_flow(&mut marks, &item_path, rest, line);
            }
            continue;
        }

        let Some((key, value)) = split_entry(text) else {
            continue;
        };
        let path = join(&top.path, key);
        marks.insert(path.clone(), line);
        mark_flow(&mut marks, &path, value, line);
        if value.is_empty() {
            pending = Some((path, indent));
        } else if is_block_scalar(value) {
            block_scalar_indent = Some(indent);
        }
    }
    marks
}

fn join(prefix: &str, segment: &str) -> String {
    if prefix.is_empty() {
        segment.to_owned()
    } else {
        format!("{prefix}.{segment}")
    }
}

/// `- ` or a bare `-` introducing a nested block.
fn strip_seq_marker(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('-')?;
    if rest.is_empty() {
        return Some(rest);
    }
    if !rest.starts_with(' ') {
        return None;
    }
    Some(rest.trim_start())
}

/// Whether a value opens a literal or folded block scalar, allowing for a
/// leading anchor (`&log_file |`) and the chomping/indent indicators.
fn is_block_scalar(value: &str) -> bool {
    let rest = value.strip_prefix('&').map_or(value, |after| {
        after
            .split_once(' ')
            .map_or("", |(_, tail)| tail.trim_start())
    });
    let Some(body) = rest.strip_prefix(['|', '>']) else {
        return false;
    };
    body.chars()
        .all(|c| matches!(c, '+' | '-') || c.is_ascii_digit())
}

/// Split `key: value`, tolerating quoted keys and colons inside values, and
/// returning an empty value for a key that only opens a block.
fn split_entry(text: &str) -> Option<(&str, &str)> {
    let mut in_single = false;
    let mut in_double = false;
    for (index, ch) in text.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            // A flow collection can only be a value, never a bare key here.
            '[' | '{' if !in_single && !in_double => return None,
            ':' if !in_single && !in_double => {
                let after = text.get(index + 1..)?;
                if !after.is_empty() && !after.starts_with(' ') {
                    continue;
                }
                let key = text.get(..index)?.trim_end();
                return Some((unquote(key), strip_comment(after.trim())));
            }
            _ => {}
        }
    }
    None
}

fn unquote(key: &str) -> &str {
    for quote in ['\'', '"'] {
        if key.len() >= 2 && key.starts_with(quote) && key.ends_with(quote) {
            return key.get(1..key.len() - 1).unwrap_or(key);
        }
    }
    key
}

/// Drop a trailing ` # comment` that is not inside quotes.
fn strip_comment(value: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let mut previous = ' ';
    for (index, ch) in value.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single && !in_double && previous == ' ' => {
                return value.get(..index).unwrap_or(value).trim_end();
            }
            _ => {}
        }
        previous = ch;
    }
    value
}

/// Recursively mark a flow collection. Everything on one line shares that line,
/// which is what `PyYAML`'s `start_mark` reports.
fn mark_flow(marks: &mut Marks, path: &str, value: &str, line: usize) {
    let value = value.trim();
    if let Some(inner) = enclosed(value, '[', ']') {
        for (index, element) in split_flow(inner).into_iter().enumerate() {
            let child = join(path, &index.to_string());
            marks.insert(child.clone(), line);
            mark_flow(marks, &child, element, line);
        }
    } else if let Some(inner) = enclosed(value, '{', '}') {
        for element in split_flow(inner) {
            let Some((key, rest)) = split_flow_entry(element) else {
                continue;
            };
            let child = join(path, key);
            marks.insert(child.clone(), line);
            mark_flow(marks, &child, rest, line);
        }
    }
}

fn enclosed(value: &str, open: char, close: char) -> Option<&str> {
    value
        .strip_prefix(open)
        .and_then(|rest| rest.strip_suffix(close))
}

/// Split a flow collection body on top-level commas, respecting quotes.
fn split_flow(inner: &str) -> Vec<&str> {
    if inner.trim().is_empty() {
        return Vec::new();
    }
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut start = 0usize;
    for (index, ch) in inner.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '[' | '{' if !in_single && !in_double => depth += 1,
            ']' | '}' if !in_single && !in_double => depth = depth.saturating_sub(1),
            ',' if depth == 0 && !in_single && !in_double => {
                parts.push(inner.get(start..index).unwrap_or("").trim());
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(inner.get(start..).unwrap_or("").trim());
    parts
}

/// `key: value` inside a flow mapping.
fn split_flow_entry(element: &str) -> Option<(&str, &str)> {
    let mut in_single = false;
    let mut in_double = false;
    let mut depth = 0usize;
    for (index, ch) in element.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '[' | '{' if !in_single && !in_double => depth += 1,
            ']' | '}' if !in_single && !in_double => depth = depth.saturating_sub(1),
            ':' if depth == 0 && !in_single && !in_double => {
                let key = element.get(..index)?.trim_end();
                return Some((unquote(key), element.get(index + 1..)?.trim()));
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    #[test]
    fn reproduces_the_upstream_docstring_example() {
        let got = scan("one: val1\ntwo:\n  subtwo: val2\nthree: [val3, val4]\n");
        assert_eq!(got.get("one"), Some(&1));
        assert_eq!(got.get("two"), Some(&2));
        assert_eq!(got.get("two.subtwo"), Some(&3));
        assert_eq!(got.get("three"), Some(&4));
        assert_eq!(got.get("three.0"), Some(&4));
        assert_eq!(got.get("three.1"), Some(&4));
        assert_eq!(got.len(), 6);
    }

    #[test]
    fn marks_block_sequences_of_mappings() {
        let content =
            "#cloud-config\nfour:\n  - a: 1\n    b: 2\n  - c: 3\nfive:\n  - x\n  - y\n";
        let got = scan(content);
        assert_eq!(got.get("four"), Some(&2));
        assert_eq!(got.get("four.0"), Some(&3));
        assert_eq!(got.get("four.0.a"), Some(&3));
        assert_eq!(got.get("four.0.b"), Some(&4));
        assert_eq!(got.get("four.1"), Some(&5));
        assert_eq!(got.get("four.1.c"), Some(&5));
        assert_eq!(got.get("five"), Some(&6));
        assert_eq!(got.get("five.0"), Some(&7));
        assert_eq!(got.get("five.1"), Some(&8));
    }

    #[test]
    fn allows_a_sequence_at_the_parent_indentation() {
        let got = scan("users:\n- name: u\n- name: v\n");
        assert_eq!(got.get("users"), Some(&1));
        assert_eq!(got.get("users.0.name"), Some(&2));
        assert_eq!(got.get("users.1.name"), Some(&3));
    }

    #[test]
    fn skips_comments_and_blank_lines() {
        let got = scan("# lead\n\na: 1\n  # nested comment\nb: 2\n");
        assert_eq!(got.get("a"), Some(&3));
        assert_eq!(got.get("b"), Some(&5));
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn does_not_split_on_a_colon_inside_a_value() {
        let got = scan("url: https://example.com/x\n");
        assert_eq!(got.get("url"), Some(&1));
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn handles_quoted_keys() {
        assert_eq!(scan("\"a:b\": 1\n").get("a:b"), Some(&1));
    }

    #[test]
    fn treats_block_scalar_bodies_as_opaque() {
        // The body looks like YAML but must not contribute marks.
        let got = scan("script: |\n  format=1\n  other: 2\nafter: 3\n");
        assert_eq!(got.get("script"), Some(&1));
        assert_eq!(got.get("after"), Some(&4));
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn treats_anchored_block_scalars_as_opaque() {
        let got = scan("log_cfgs:\n - &log_base |\n   class=FileHandler\n - x\n");
        assert_eq!(got.get("log_cfgs.0"), Some(&2));
        assert_eq!(got.get("log_cfgs.1"), Some(&4));
    }

    #[test]
    fn marks_flow_mappings() {
        let got = scan("chpasswd: { expire: False }\n");
        assert_eq!(got.get("chpasswd"), Some(&1));
        assert_eq!(got.get("chpasswd.expire"), Some(&1));
    }

    #[test]
    fn does_not_split_flow_items_on_a_quoted_comma() {
        let got = scan("f: [ None, \"defaults,nofail\", \"2\" ]\n");
        assert_eq!(got.get("f.2"), Some(&1));
        assert_eq!(got.get("f.3"), None);
    }

    #[test]
    fn marks_flow_sequences_nested_in_block_sequence_items() {
        let got = scan("runcmd:\n - [ ls, -l, / ]\n");
        assert_eq!(got.get("runcmd.0"), Some(&2));
        assert_eq!(got.get("runcmd.0.0"), Some(&2));
        assert_eq!(got.get("runcmd.0.2"), Some(&2));
    }
}
