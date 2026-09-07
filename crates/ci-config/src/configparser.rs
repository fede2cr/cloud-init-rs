//! Port of `configparser.RawConfigParser` as `cc_puppet` uses it, through
//! `helpers.DefaultingConfigParser`.
//!
//! Only the read/set/write path is here, because that is the whole of what the
//! module does: parse an existing `puppet.conf`, overwrite options from
//! cloud-config, and write it back. Interpolation is not: `RawConfigParser`
//! disables it, so a `%` in a value survives to disk untouched.
//!
//! The awkward parts are all in the details rather than the shape --
//! option names are lowercased, values are stripped, a value can be continued
//! on an indented line, `DEFAULT` is a section that is not in the section
//! list, and a line that parses as neither is collected rather than raised so
//! that the error at the end can name every bad line at once. Each of those
//! decides what ends up in `/etc/puppet/puppet.conf`, so each is reproduced.

use crate::repr::repr_str;

/// The section name that lives outside the section list.
const DEFAULT_SECTION: &str = "DEFAULT";

/// What the parser and `set` raise, with `Display` giving the message Python
/// puts in the exception.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// An option line before any `[section]`.
    MissingSectionHeader {
        source: String,
        lineno: usize,
        line: String,
    },
    /// `[main]` twice in one file, under `strict=True`.
    DuplicateSection {
        source: String,
        lineno: usize,
        section: String,
    },
    /// The same option twice in one section, under `strict=True`.
    DuplicateOption {
        source: String,
        lineno: usize,
        section: String,
        option: String,
    },
    /// Every line that was neither a section, an option nor a comment. These
    /// are collected as the file is read and raised once at the end.
    Parsing {
        source: String,
        lines: Vec<(usize, String)>,
    },
    /// `set` into a section that was never added.
    NoSection(String),
    /// `add_section("DEFAULT")`.
    Value(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingSectionHeader {
                source,
                lineno,
                line,
            } => write!(
                f,
                "File contains no section headers.\nfile: {}, line: {lineno}\n{}",
                repr_str(source),
                repr_str(line)
            ),
            Self::DuplicateSection {
                source,
                lineno,
                section,
            } => write!(
                f,
                "While reading from {} [line {lineno:2}]: section {} already exists",
                repr_str(source),
                repr_str(section)
            ),
            Self::DuplicateOption {
                source,
                lineno,
                section,
                option,
            } => write!(
                f,
                "While reading from {} [line {lineno:2}]: option {} in section {} \
                 already exists",
                repr_str(source),
                repr_str(option),
                repr_str(section)
            ),
            Self::Parsing { source, lines } => {
                write!(f, "Source contains parsing errors: {}", repr_str(source))?;
                for (lineno, line) in lines {
                    write!(f, "\n\t[line {lineno:2}]: {}", repr_str(line))?;
                }
                Ok(())
            }
            Self::NoSection(section) => write!(f, "No section: {}", repr_str(section)),
            Self::Value(message) => write!(f, "{message}"),
        }
    }
}

/// The exception class name, which is what a caller logging `type(e).__name__`
/// would print.
impl Error {
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::MissingSectionHeader { .. } => "MissingSectionHeaderError",
            Self::DuplicateSection { .. } => "DuplicateSectionError",
            Self::DuplicateOption { .. } => "DuplicateOptionError",
            Self::Parsing { .. } => "ParsingError",
            Self::NoSection(_) => "NoSectionError",
            Self::Value(_) => "ValueError",
        }
    }
}

/// A parsed config file, in the order the sections and options were seen.
///
/// Order is the whole point of keeping this by hand: `write` emits sections in
/// insertion order and options in insertion order within a section, so a set
/// of an option that already exists replaces it in place while a new one is
/// appended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawConfigParser {
    defaults: Vec<(String, String)>,
    sections: Vec<(String, Vec<(String, String)>)>,
}

impl RawConfigParser {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `read_file(fp, source)`.
    ///
    /// The text is split the way iterating a file object splits it: on `\n`,
    /// with the newline still attached to each line, because the `repr` in a
    /// parse error shows it.
    ///
    /// # Errors
    /// The first fatal problem, or the collected [`Error::Parsing`] list.
    pub fn read_str(&mut self, text: &str, source: &str) -> Result<(), Error> {
        let mut errors: Vec<(usize, String)> = Vec::new();
        // The section currently being filled: `None` before the first header,
        // `Some(None)` for `[DEFAULT]`, `Some(Some(index))` otherwise.
        let mut cursect: Option<Option<usize>> = None;
        let mut sectname = String::new();
        let mut optname: Option<String> = None;
        let mut indent_level = 0_usize;
        // `elements_added` under `strict=True`: sections and options seen in
        // *this* read, which is why a second read of the same names is fine.
        let mut added_sections: Vec<String> = Vec::new();
        let mut added_options: Vec<(String, String)> = Vec::new();

        for (index, line) in split_file_lines(text).into_iter().enumerate() {
            let lineno = index + 1;
            let commented =
                line.trim().starts_with('#') || line.trim().starts_with(';');
            let value = if commented { "" } else { line.trim() };
            if value.is_empty() {
                // `empty_lines_in_values`: a blank line inside a value keeps
                // the value open and contributes a newline to it.
                if !commented {
                    if let (Some(target), Some(name)) = (cursect, optname.as_ref()) {
                        push_continuation(self, target, name, "");
                    }
                }
                continue;
            }

            let cur_indent_level = line.find(|c: char| !c.is_whitespace()).unwrap_or(0);
            if let (Some(target), Some(name)) = (cursect, optname.as_ref()) {
                if cur_indent_level > indent_level {
                    push_continuation(self, target, name, value);
                    continue;
                }
            }
            indent_level = cur_indent_level;

            if let Some(header) = section_header(value) {
                if header == DEFAULT_SECTION {
                    cursect = Some(None);
                } else if let Some(at) = self.index_of(header) {
                    if added_sections.iter().any(|name| name == header) {
                        return Err(Error::DuplicateSection {
                            source: source.to_owned(),
                            lineno,
                            section: header.to_owned(),
                        });
                    }
                    cursect = Some(Some(at));
                    added_sections.push(header.to_owned());
                } else {
                    self.sections.push((header.to_owned(), Vec::new()));
                    cursect = Some(Some(self.sections.len() - 1));
                    added_sections.push(header.to_owned());
                }
                header.clone_into(&mut sectname);
                optname = None;
                continue;
            }

            let Some(target) = cursect else {
                return Err(Error::MissingSectionHeader {
                    source: source.to_owned(),
                    lineno,
                    line: line.to_owned(),
                });
            };

            let Some((option, optval)) = split_option(value) else {
                errors.push((lineno, line.to_owned()));
                continue;
            };
            if option.is_empty() {
                errors.push((lineno, line.to_owned()));
            }
            let name = option.trim_end().to_lowercase();
            if added_options
                .iter()
                .any(|(sect, opt)| sect == &sectname && opt == &name)
            {
                return Err(Error::DuplicateOption {
                    source: source.to_owned(),
                    lineno,
                    section: sectname.clone(),
                    option: name,
                });
            }
            added_options.push((sectname.clone(), name.clone()));
            optval.trim().clone_into(self.slot(target, &name));
            optname = Some(name);
        }

        // `_join_multiline_values` right-strips every value, which is where a
        // trailing blank line inside a value goes.
        for (_, value) in &mut self.defaults {
            *value = value.trim_end().to_owned();
        }
        for (_, options) in &mut self.sections {
            for (_, value) in options.iter_mut() {
                *value = value.trim_end().to_owned();
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(Error::Parsing {
                source: source.to_owned(),
                lines: errors,
            })
        }
    }

    /// `DefaultingConfigParser.set`: add the section first unless it is named
    /// `default` in any case, then set.
    ///
    /// # Errors
    /// [`Error::NoSection`] for a section that was skipped by that guard and
    /// is not `DEFAULT` exactly -- see the note in `cc_puppet`.
    pub fn set(
        &mut self,
        section: &str,
        option: &str,
        value: &str,
    ) -> Result<(), Error> {
        if self.index_of(section).is_none() && section.to_lowercase() != "default" {
            self.add_section(section)?;
        }
        let name = option.to_lowercase();
        if section.is_empty() || section == DEFAULT_SECTION {
            set_in(&mut self.defaults, &name, value);
            return Ok(());
        }
        let Some(at) = self.index_of(section) else {
            return Err(Error::NoSection(section.to_owned()));
        };
        let Some((_, options)) = self.sections.get_mut(at) else {
            return Err(Error::NoSection(section.to_owned()));
        };
        set_in(options, &name, value);
        Ok(())
    }

    /// `add_section`.
    ///
    /// # Errors
    /// The `ValueError` `DEFAULT` raises. A duplicate cannot reach here: the
    /// only caller tests `has_section` first.
    pub fn add_section(&mut self, section: &str) -> Result<(), Error> {
        if section == DEFAULT_SECTION {
            return Err(Error::Value("Invalid section name: DEFAULT".to_owned()));
        }
        if self.index_of(section).is_none() {
            self.sections.push((section.to_owned(), Vec::new()));
        }
        Ok(())
    }

    #[must_use]
    pub fn has_section(&self, section: &str) -> bool {
        self.index_of(section).is_some()
    }

    /// `DefaultingConfigParser.stringify()` with no header: `write` into a
    /// string, `DEFAULT` first, a blank line after every section.
    #[must_use]
    pub fn stringify(&self) -> String {
        let mut out = String::new();
        if !self.defaults.is_empty() {
            write_section(&mut out, DEFAULT_SECTION, &self.defaults);
        }
        for (name, options) in &self.sections {
            write_section(&mut out, name, options);
        }
        out
    }

    fn index_of(&self, section: &str) -> Option<usize> {
        self.sections.iter().position(|(name, _)| name == section)
    }

    /// The value cell for an option in the section being read, creating it.
    fn slot(&mut self, target: Option<usize>, name: &str) -> &mut String {
        let options = match target {
            None => &mut self.defaults,
            Some(at) => match self.sections.get_mut(at) {
                Some((_, options)) => options,
                None => &mut self.defaults,
            },
        };
        if !options.iter().any(|(key, _)| key == name) {
            options.push((name.to_owned(), String::new()));
        }
        let at = options.iter().position(|(key, _)| key == name).unwrap_or(0);
        match options.get_mut(at) {
            Some((_, value)) => value,
            None => unreachable!("just inserted"),
        }
    }
}

/// A continuation line, which joins the running value with a newline.
fn push_continuation(
    parser: &mut RawConfigParser,
    target: Option<usize>,
    name: &str,
    value: &str,
) {
    let slot = parser.slot(target, name);
    slot.push('\n');
    slot.push_str(value);
}

/// Replace in place or append, so that `write` keeps the file's own order.
fn set_in(options: &mut Vec<(String, String)>, name: &str, value: &str) {
    if let Some(slot) = options.iter_mut().find(|(key, _)| key == name) {
        value.clone_into(&mut slot.1);
    } else {
        options.push((name.to_owned(), value.to_owned()));
    }
}

/// `_write_section`, whose only escaping is that a newline in a value is
/// continued with a tab.
fn write_section(out: &mut String, name: &str, options: &[(String, String)]) {
    out.push('[');
    out.push_str(name);
    out.push_str("]\n");
    for (key, value) in options {
        out.push_str(key);
        out.push_str(" = ");
        out.push_str(&value.replace('\n', "\n\t"));
        out.push('\n');
    }
    out.push('\n');
}

/// Iterating a text file yields lines with the separator still attached, and
/// the last line only has one if the text ends with one. The parse-error
/// `repr` shows the difference, so it is kept.
fn split_file_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find('\n') {
        lines.push(rest.get(..=at).unwrap_or(""));
        rest = rest.get(at + 1..).unwrap_or("");
    }
    if !rest.is_empty() {
        lines.push(rest);
    }
    lines
}

/// `SECTCRE`, which is `\[(?P<header>.+)\]` matched at the start: `.+` is
/// greedy, so `[a][b]` is one section named `a][b`.
fn section_header(value: &str) -> Option<&str> {
    let rest = value.strip_prefix('[')?;
    let at = rest.rfind(']')?;
    if at == 0 {
        return None;
    }
    rest.get(..at)
}

/// `OPTCRE` with delimiters `=` and `:`: the option name is non-greedy, so the
/// first of either wins.
fn split_option(value: &str) -> Option<(&str, &str)> {
    let at = value.find(['=', ':'])?;
    let name = value.get(..at)?;
    let rest = value.get(at + 1..)?;
    Some((name, rest))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    fn round_trip(text: &str) -> String {
        let mut parser = RawConfigParser::new();
        parser.read_str(text, "x").unwrap();
        parser.stringify()
    }

    #[test]
    fn options_are_lowercased_and_the_delimiter_is_normalised() {
        assert_eq!(
            round_trip("[main]\nfoo:bar\nBAZ=1\n"),
            "[main]\nfoo = bar\nbaz = 1\n\n"
        );
    }

    #[test]
    fn the_default_section_is_written_first_and_is_not_a_section() {
        let mut parser = RawConfigParser::new();
        parser
            .read_str("[main]\na=2\n[DEFAULT]\nd=1\n", "x")
            .unwrap();
        assert!(!parser.has_section("DEFAULT"));
        assert_eq!(parser.stringify(), "[DEFAULT]\nd = 1\n\n[main]\na = 2\n\n");
    }

    #[test]
    fn a_lowercase_default_section_cannot_be_set() {
        let mut parser = RawConfigParser::new();
        parser.read_str("[main]\na=1\n", "x").unwrap();
        let error = parser.set("default", "x", "y").unwrap_err();
        assert_eq!(error.kind(), "NoSectionError");
        assert_eq!(error.to_string(), "No section: 'default'");
    }

    #[test]
    fn setting_an_existing_option_keeps_its_place() {
        let mut parser = RawConfigParser::new();
        parser.read_str("[main]\na=1\nb=2\n", "x").unwrap();
        parser.set("main", "A", "9").unwrap();
        parser.set("main", "c", "3").unwrap();
        assert_eq!(parser.stringify(), "[main]\na = 9\nb = 2\nc = 3\n\n");
    }

    #[test]
    fn a_valueless_line_is_collected_and_raised_at_the_end() {
        let mut parser = RawConfigParser::new();
        let error = parser
            .read_str("[main]\nfoo\nbar\n", "pp.conf")
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Source contains parsing errors: 'pp.conf'\n\t[line  2]: 'foo\\n'\
             \n\t[line  3]: 'bar\\n'"
        );
    }

    #[test]
    fn an_option_before_any_section_is_fatal() {
        let mut parser = RawConfigParser::new();
        let error = parser.read_str("a=1\n", "pp.conf").unwrap_err();
        assert_eq!(
            error.to_string(),
            "File contains no section headers.\nfile: 'pp.conf', line: 1\n'a=1\\n'"
        );
    }

    #[test]
    fn a_repeated_section_or_option_is_fatal() {
        let mut parser = RawConfigParser::new();
        assert_eq!(
            parser
                .read_str("[main]\na=1\n[main]\nb=2\n", "pp.conf")
                .unwrap_err()
                .to_string(),
            "While reading from 'pp.conf' [line  3]: section 'main' already exists"
        );
        let mut parser = RawConfigParser::new();
        assert_eq!(
            parser
                .read_str("[main]\na=1\na=2\n", "pp.conf")
                .unwrap_err()
                .to_string(),
            "While reading from 'pp.conf' [line  3]: option 'a' in section 'main' \
             already exists"
        );
    }

    #[test]
    fn an_indented_line_continues_the_value_and_is_written_back_with_a_tab() {
        assert_eq!(
            round_trip("[main]\nfoo = bar\n\tmore\n"),
            "[main]\nfoo = bar\n\tmore\n\n"
        );
    }

    #[test]
    fn nothing_is_interpolated() {
        assert_eq!(round_trip("[main]\np = 100%%s\n"), "[main]\np = 100%%s\n\n");
    }
}
