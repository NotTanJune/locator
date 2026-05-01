use anyhow::{anyhow, Result};
use chrono::{DateTime, NaiveDate, Utc};
use clap::ValueEnum;
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

    pub fn matches(self, query: &str, candidate: &str) -> Result<bool> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(true);
        }

        let candidate_lower = candidate.to_ascii_lowercase();
        let query_lower = query.to_ascii_lowercase();
        Ok(match self {
            Self::Contains => candidate_lower.contains(&query_lower),
            Self::Exact => candidate_lower == query_lower,
            Self::Prefix => candidate_lower.starts_with(&query_lower),
            Self::Suffix => {
                let stem_matches = std::path::Path::new(&candidate_lower)
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| stem.ends_with(&query_lower));
                candidate_lower.ends_with(&query_lower) || stem_matches
            }
            Self::Fuzzy => fuzzy_match(&query_lower, &candidate_lower),
            Self::Regex => Regex::new(query)
                .map_err(|error| anyhow!("invalid regex '{query}': {error}"))?
                .is_match(candidate),
            Self::Glob => glob_regex(query)?.is_match(candidate),
        })
    }
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

fn fuzzy_match(needle: &str, candidate: &str) -> bool {
    if needle.is_empty() {
        return true;
    }

    let mut chars = needle.chars();
    let mut current = chars.next();
    for candidate_char in candidate.chars() {
        if Some(candidate_char) == current {
            current = chars.next();
            if current.is_none() {
                return true;
            }
        }
    }
    false
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
