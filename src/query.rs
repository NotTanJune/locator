use anyhow::{anyhow, Result};
use chrono::{DateTime, NaiveDate, Utc};
use clap::ValueEnum;
use nucleo_matcher::{Config, Matcher, Utf32Str};
use regex::{Regex, RegexBuilder};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum QueryMode {
    #[default]
    Contains,
    Exact,
    Prefix,
    Suffix,
    Fuzzy,
    Regex,
    Glob,
}

impl QueryMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Contains => "contains",
            Self::Exact => "exact",
            Self::Prefix => "prefix",
            Self::Suffix => "suffix",
            Self::Fuzzy => "fuzzy",
            Self::Regex => "regex",
            Self::Glob => "glob",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Contains => Self::Exact,
            Self::Exact => Self::Prefix,
            Self::Prefix => Self::Suffix,
            Self::Suffix => Self::Fuzzy,
            Self::Fuzzy => Self::Regex,
            Self::Regex => Self::Glob,
            Self::Glob => Self::Contains,
        }
    }

    /// Convenience one-shot match. Compiles the query and matches a single
    /// candidate. For hot loops, prefer [`CompiledQuery`] which compiles once.
    pub fn matches(self, query: &str, candidate: &str) -> Result<bool> {
        let compiled = CompiledQuery::compile(self, query)?;
        let mut scorer = QueryScorer::new();
        Ok(compiled.is_match(&mut scorer, candidate))
    }
}

/// Reusable fuzzy-match scratch space (nucleo `Matcher` plus buffers). Construct
/// once per search (or per draw) and reuse across all candidates to avoid
/// repeated allocation.
pub struct QueryScorer {
    matcher: Matcher,
    needle_buf: Vec<char>,
    hay_buf: Vec<char>,
    idx_buf: Vec<u32>,
}

impl Default for QueryScorer {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryScorer {
    pub fn new() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT),
            needle_buf: Vec::new(),
            hay_buf: Vec::new(),
            idx_buf: Vec::new(),
        }
    }

    fn fuzzy_score(&mut self, needle_lower: &str, hay_lower: &str) -> Option<u32> {
        let Self {
            matcher,
            needle_buf,
            hay_buf,
            ..
        } = self;
        let needle = Utf32Str::new(needle_lower, needle_buf);
        let hay = Utf32Str::new(hay_lower, hay_buf);
        matcher.fuzzy_match(hay, needle).map(u32::from)
    }

    fn fuzzy_positions(&mut self, needle_lower: &str, hay_lower: &str) -> Vec<usize> {
        let Self {
            matcher,
            needle_buf,
            hay_buf,
            idx_buf,
        } = self;
        idx_buf.clear();
        let needle = Utf32Str::new(needle_lower, needle_buf);
        let hay = Utf32Str::new(hay_lower, hay_buf);
        if matcher.fuzzy_indices(hay, needle, idx_buf).is_none() {
            return Vec::new();
        }
        idx_buf.sort_unstable();
        idx_buf.dedup();
        idx_buf.iter().map(|&i| i as usize).collect()
    }
}

/// A query compiled once for a given mode + string. Regex/glob are compiled a
/// single time here rather than per candidate.
pub enum CompiledQuery {
    Empty,
    Contains(String),
    Exact(String),
    Prefix(String),
    Suffix(String),
    Fuzzy(String),
    Regex(Regex),
    Glob(Regex),
}

impl CompiledQuery {
    pub fn compile(mode: QueryMode, query: &str) -> Result<Self> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(Self::Empty);
        }
        let lower = trimmed.to_ascii_lowercase();
        Ok(match mode {
            QueryMode::Contains => Self::Contains(lower),
            QueryMode::Exact => Self::Exact(lower),
            QueryMode::Prefix => Self::Prefix(lower),
            QueryMode::Suffix => Self::Suffix(lower),
            QueryMode::Fuzzy => Self::Fuzzy(lower),
            QueryMode::Regex => Self::Regex(
                Regex::new(trimmed)
                    .map_err(|error| anyhow!("invalid regex '{trimmed}': {error}"))?,
            ),
            QueryMode::Glob => Self::Glob(glob_regex(trimmed)?),
        })
    }

    pub fn is_match(&self, scorer: &mut QueryScorer, candidate: &str) -> bool {
        match self {
            Self::Empty => true,
            Self::Contains(q) => candidate.to_ascii_lowercase().contains(q),
            Self::Exact(q) => candidate.to_ascii_lowercase() == *q,
            Self::Prefix(q) => candidate.to_ascii_lowercase().starts_with(q),
            Self::Suffix(q) => {
                let lower = candidate.to_ascii_lowercase();
                let stem_matches = std::path::Path::new(&lower)
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| stem.ends_with(q));
                lower.ends_with(q) || stem_matches
            }
            Self::Fuzzy(q) => scorer
                .fuzzy_score(q, &candidate.to_ascii_lowercase())
                .is_some(),
            Self::Regex(re) | Self::Glob(re) => re.is_match(candidate),
        }
    }

    pub fn matches_any<'a>(
        &self,
        scorer: &mut QueryScorer,
        candidates: impl IntoIterator<Item = &'a str>,
    ) -> bool {
        candidates
            .into_iter()
            .any(|candidate| self.is_match(scorer, candidate))
    }

    /// Best fuzzy score across the given candidates (typically name + path).
    /// Higher is better. `None` when nothing matches. Only meaningful in Fuzzy
    /// mode; other modes return `None`.
    pub fn fuzzy_rank<'a>(
        &self,
        scorer: &mut QueryScorer,
        candidates: impl IntoIterator<Item = &'a str>,
    ) -> Option<u32> {
        let Self::Fuzzy(q) = self else {
            return None;
        };
        candidates
            .into_iter()
            .filter_map(|candidate| scorer.fuzzy_score(q, &candidate.to_ascii_lowercase()))
            .max()
    }

    /// Char positions in `text` that participate in the match, for highlighting.
    pub fn match_positions(&self, scorer: &mut QueryScorer, text: &str) -> Vec<usize> {
        match self {
            Self::Empty => Vec::new(),
            Self::Contains(q) | Self::Exact(q) | Self::Prefix(q) => substring_positions(text, q),
            Self::Suffix(q) => substring_positions(text, q),
            Self::Fuzzy(q) => scorer.fuzzy_positions(q, &text.to_ascii_lowercase()),
            Self::Regex(re) | Self::Glob(re) => regex_positions(re, text),
        }
    }
}

/// Char indices of the first occurrence of `needle_lower` within `text`
/// (case-insensitive). ASCII-lowercasing preserves char alignment.
fn substring_positions(text: &str, needle_lower: &str) -> Vec<usize> {
    if needle_lower.is_empty() {
        return Vec::new();
    }
    let lower = text.to_ascii_lowercase();
    let Some(byte_pos) = lower.find(needle_lower) else {
        return Vec::new();
    };
    let start_char = lower[..byte_pos].chars().count();
    let len_chars = needle_lower.chars().count();
    (start_char..start_char + len_chars).collect()
}

fn regex_positions(re: &Regex, text: &str) -> Vec<usize> {
    let Some(m) = re.find(text) else {
        return Vec::new();
    };
    let start_char = text[..m.start()].chars().count();
    let end_char = text[..m.end()].chars().count();
    (start_char..end_char).collect()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum SortField {
    #[default]
    Relevance,
    Name,
    Path,
    Kind,
    Size,
    Created,
    Modified,
}

impl SortField {
    pub fn label(self) -> &'static str {
        match self {
            Self::Relevance => "relevance",
            Self::Name => "name",
            Self::Path => "path",
            Self::Kind => "kind",
            Self::Size => "size",
            Self::Created => "created",
            Self::Modified => "modified",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Relevance => Self::Name,
            Self::Name => Self::Path,
            Self::Path => Self::Kind,
            Self::Kind => Self::Size,
            Self::Size => Self::Created,
            Self::Created => Self::Modified,
            Self::Modified => Self::Relevance,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileKind {
    Pdf,
    Image,
    Video,
    Audio,
    Text,
    Archive,
    Folder,
    Other(String),
}

impl FileKind {
    pub fn parse(value: &str) -> Result<Self> {
        let normalized = value.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            return Err(anyhow!("file type cannot be empty"));
        }

        Ok(match normalized.as_str() {
            "pdf" => Self::Pdf,
            "image" | "img" => Self::Image,
            "video" => Self::Video,
            "audio" => Self::Audio,
            "text" | "txt" => Self::Text,
            "archive" | "zip" => Self::Archive,
            "folder" | "dir" | "directory" => Self::Folder,
            other => Self::Other(other.to_string()),
        })
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Pdf => "pdf",
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Text => "text",
            Self::Archive => "archive",
            Self::Folder => "folder",
            Self::Other(value) => value.as_str(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeBound {
    pub bytes: u64,
}

impl SizeBound {
    pub fn parse(value: &str) -> Result<Self> {
        parse_size(value).map(|bytes| Self { bytes })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchFilters {
    pub kind: Option<FileKind>,
    pub exts: Vec<String>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub created_after: Option<DateTime<Utc>>,
    pub created_before: Option<DateTime<Utc>>,
    pub modified_after: Option<DateTime<Utc>>,
    pub modified_before: Option<DateTime<Utc>>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOptions {
    pub query: String,
    pub mode: QueryMode,
    pub sort: SortField,
    pub reverse: bool,
    pub filters: SearchFilters,
    pub limit: usize,
}

impl SearchOptions {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            mode: QueryMode::Contains,
            sort: SortField::Relevance,
            reverse: false,
            filters: SearchFilters::new(),
            limit: 50,
        }
    }

    pub fn with_mode(mut self, mode: QueryMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_sort(mut self, sort: SortField) -> Self {
        self.sort = sort;
        self
    }

    pub fn with_reverse(mut self, reverse: bool) -> Self {
        self.reverse = reverse;
        self
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    pub fn with_filters(mut self, filters: SearchFilters) -> Self {
        self.filters = filters;
        self
    }

    pub fn with_kind(mut self, value: &str) -> Result<Self> {
        self.filters = self.filters.with_kind(value)?;
        Ok(self)
    }

    pub fn with_exts(mut self, value: &str) -> Result<Self> {
        self.filters = self.filters.with_exts(value)?;
        Ok(self)
    }

    pub fn with_min_size(mut self, value: &str) -> Result<Self> {
        self.filters = self.filters.with_min_size(value)?;
        Ok(self)
    }

    pub fn with_max_size(mut self, value: &str) -> Result<Self> {
        self.filters = self.filters.with_max_size(value)?;
        Ok(self)
    }

    pub fn with_created_after(mut self, value: &str) -> Result<Self> {
        self.filters = self.filters.with_created_after(value)?;
        Ok(self)
    }

    pub fn with_created_before(mut self, value: &str) -> Result<Self> {
        self.filters = self.filters.with_created_before(value)?;
        Ok(self)
    }

    pub fn with_modified_after(mut self, value: &str) -> Result<Self> {
        self.filters = self.filters.with_modified_after(value)?;
        Ok(self)
    }

    pub fn with_modified_before(mut self, value: &str) -> Result<Self> {
        self.filters = self.filters.with_modified_before(value)?;
        Ok(self)
    }

    pub fn with_name(mut self, value: &str) -> Result<Self> {
        self.filters = self.filters.with_name(value)?;
        Ok(self)
    }

    pub fn validate(&self) -> Result<()> {
        match self.mode {
            QueryMode::Regex => {
                Regex::new(self.query.trim())
                    .map_err(|error| anyhow!("invalid regex '{}': {error}", self.query.trim()))?;
            }
            QueryMode::Glob => {
                glob_regex(self.query.trim())?;
            }
            _ => {}
        }
        Ok(())
    }
}

impl SearchFilters {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_kind(mut self, value: &str) -> Result<Self> {
        self.kind = Some(FileKind::parse(value)?);
        Ok(self)
    }

    pub fn with_exts(mut self, value: &str) -> Result<Self> {
        let exts = value
            .split(',')
            .map(|ext| ext.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|ext| !ext.is_empty())
            .collect::<Vec<_>>();

        if exts.is_empty() {
            return Err(anyhow!("extension list cannot be empty"));
        }

        self.exts = exts;
        Ok(self)
    }

    pub fn with_min_size(mut self, value: &str) -> Result<Self> {
        self.min_size = Some(SizeBound::parse(value)?.bytes);
        Ok(self)
    }

    pub fn with_max_size(mut self, value: &str) -> Result<Self> {
        self.max_size = Some(SizeBound::parse(value)?.bytes);
        Ok(self)
    }

    pub fn with_created_after(mut self, value: &str) -> Result<Self> {
        self.created_after = Some(parse_date(value)?);
        Ok(self)
    }

    pub fn with_created_before(mut self, value: &str) -> Result<Self> {
        self.created_before = Some(parse_date(value)?);
        Ok(self)
    }

    pub fn with_modified_after(mut self, value: &str) -> Result<Self> {
        self.modified_after = Some(parse_date(value)?);
        Ok(self)
    }

    pub fn with_modified_before(mut self, value: &str) -> Result<Self> {
        self.modified_before = Some(parse_date(value)?);
        Ok(self)
    }

    pub fn with_name(mut self, value: &str) -> Result<Self> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(anyhow!("name filter cannot be empty"));
        }
        self.name = Some(trimmed.to_string());
        Ok(self)
    }
}

pub fn parse_date(value: &str) -> Result<DateTime<Utc>> {
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| anyhow!("invalid date '{value}', expected YYYY-MM-DD"))?;
    Ok(date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| anyhow!("invalid date '{value}'"))?
        .and_utc())
}

fn parse_size(value: &str) -> Result<u64> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Err(anyhow!("invalid size '{value}'"));
    }

    let split_at = normalized
        .find(|ch: char| !ch.is_ascii_digit() && ch != '.')
        .unwrap_or(normalized.len());
    let (number, unit) = normalized.split_at(split_at);

    if number.is_empty() {
        return Err(anyhow!("invalid size '{value}'"));
    }

    let amount = number
        .parse::<f64>()
        .map_err(|_| anyhow!("invalid size '{value}'"))?;
    let multiplier = match unit.trim() {
        "" | "b" => 1.0,
        "k" | "kb" => 1_000.0,
        "m" | "mb" => 1_000_000.0,
        "g" | "gb" => 1_000_000_000.0,
        "t" | "tb" => 1_000_000_000_000.0,
        _ => return Err(anyhow!("invalid size '{value}'")),
    };

    Ok((amount * multiplier) as u64)
}

fn glob_regex(pattern: &str) -> Result<Regex> {
    if pattern.trim().is_empty() {
        return Err(anyhow!("glob pattern cannot be empty"));
    }
    let mut regex = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            '[' => {
                let mut class = String::from("[");
                let mut closed = false;
                for next in chars.by_ref() {
                    class.push(next);
                    if next == ']' {
                        closed = true;
                        break;
                    }
                }
                if !closed {
                    return Err(anyhow!(
                        "invalid glob '{pattern}': unclosed character class"
                    ));
                }
                regex.push_str(&class);
            }
            _ => regex.push_str(&regex::escape(&ch.to_string())),
        }
    }
    regex.push('$');

    RegexBuilder::new(&regex)
        .case_insensitive(true)
        .build()
        .map_err(|error| anyhow!("invalid glob '{pattern}': {error}"))
}
