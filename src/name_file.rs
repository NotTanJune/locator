//! The compact name file, `index.names`: every indexed path and its kind, and nothing else.
//!
//! Paths are stored as a folder table plus the rows of each folder. Rows are cut into blocks
//! of [`BLOCK_ROWS`], and each block holds three lz4 parts: the lowercased names, one case
//! bit per name byte, and one kind byte per row. A search runs a byte search over the
//! decoded names and rebuilds a path only for a row it keeps. The file is written whole
//! with [`install`] and never edited in place.
//!
//! Layout, all integers little-endian:
//!
//! ```text
//! header: magic "LCTRNAM2", format u32, reserved u32, snapshot_id u64, rows u64,
//!         folders u64, then 7 (offset u64, length u64) pairs, one per section
//! 0 kinds             each kind string followed by a 0 byte
//! 1 folder_parents    u32 per folder; u32::MAX for folder 0
//! 2 folder_name_ends  u32 per folder; end of the folder's name in section 3
//! 3 folder_names      the folders' own names, back to back
//! 4 folder_first_rows u32 per folder plus one; folder f owns rows first[f]..first[f + 1]
//! 5 block_index       per block, per part: offset u64, packed length u32, raw length u32
//! 6 blocks            lz4 blocks
//! ```

use std::cmp::Ordering;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf, MAIN_SEPARATOR};

use anyhow::{anyhow, bail, Context, Result};

pub const INDEX_FORMAT: u32 = 1;
pub const BLOCK_ROWS: usize = 4096;

const MAGIC: &[u8; 8] = b"LCTRNAM2";
const SECTIONS: usize = 7;
const HEADER_LEN: usize = 40 + SECTIONS * 16;
const SEP: u8 = MAIN_SEPARATOR as u8;
/// The largest raw size accepted for one block part, so a damaged index cannot ask for more.
const MAX_PART_BYTES: usize = 1 << 29;

const KINDS: usize = 0;
const PARENTS: usize = 1;
const NAME_ENDS: usize = 2;
const NAMES: usize = 3;
const FIRST_ROWS: usize = 4;
const BLOCK_INDEX: usize = 5;
const BLOCKS: usize = 6;

/// Where the name file for the database at `db_path` lives.
pub fn name_file_path(db_path: &Path) -> PathBuf {
    db_path.with_extension("names")
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    Some(u32::from_le_bytes(bytes.get(at..end)?.try_into().ok()?))
}

fn read_u64(bytes: &[u8], at: usize) -> Option<u64> {
    let end = at.checked_add(8)?;
    Some(u64::from_le_bytes(bytes.get(at..end)?.try_into().ok()?))
}

/// Puts the flagged bytes of a lowercased name back to upper case. `start` is the name's
/// offset in its block. Most names have no upper case, so the flag bytes are checked first.
fn restore_case(name: &mut [u8], case: &[u8], start: usize) {
    if name.is_empty() {
        return;
    }
    let last = (start + name.len() - 1) / 8;
    if case
        .get(start / 8..=last)
        .is_none_or(|flags| flags.iter().all(|flag| *flag == 0))
    {
        return;
    }
    for (offset, byte) in name.iter_mut().enumerate() {
        let bit = start + offset;
        if case
            .get(bit / 8)
            .is_some_and(|flags| flags & (1 << (bit % 8)) != 0)
        {
            byte.make_ascii_uppercase();
        }
    }
}

// ---------- building ----------

/// The rows of one folder, as `kind byte, name bytes, 0` repeated.
struct FolderRows {
    path: String,
    rows: Vec<u8>,
}

/// Collects rows and writes the file image. It keeps one buffer per folder, never one
/// `String` per row.
pub struct NameFileBuilder {
    /// Folder 0 is the top and has an empty path that is never looked up.
    folders: Vec<FolderRows>,
    folder_numbers: HashMap<String, usize>,
    /// The folder of the previous push, so a run of rows in one folder costs no lookup.
    last_folder: usize,
    kinds: Vec<String>,
    /// The first problem a push found; `finish` reports it.
    problem: Option<String>,
}

impl Default for NameFileBuilder {
    fn default() -> Self {
        Self {
            folders: vec![FolderRows::new("")],
            folder_numbers: HashMap::new(),
            last_folder: 0,
            kinds: Vec::new(),
            problem: None,
        }
    }
}

impl FolderRows {
    fn new(path: &str) -> Self {
        Self {
            path: path.to_string(),
            rows: Vec::new(),
        }
    }
}

impl NameFileBuilder {
    /// Adds one row. A later push of the same path replaces the earlier one.
    pub fn push(&mut self, path: &str, kind: &str) -> Result<()> {
        let (dir, name) = match path.rfind(MAIN_SEPARATOR) {
            Some(at) => (Some(&path[..at]), &path[at + 1..]),
            None => (None, path),
        };
        if name.len() > usize::from(u16::MAX) {
            self.note_problem(format!("file name is longer than 65535 bytes: {path}"));
        }
        if name.as_bytes().contains(&0) {
            self.note_problem(format!("file name holds a 0 byte: {path}"));
        }
        let kind = self.kind_number(kind);
        let folder = dir.map_or(0, |dir| self.folder_number(dir));
        if let Some(entry) = self.folders.get_mut(folder) {
            entry.rows.push(kind);
            entry.rows.extend_from_slice(name.as_bytes());
            entry.rows.push(0);
        }
        Ok(())
    }

    /// Whether a row at or beneath `path` has been pushed.
    pub fn has_rows_beneath(&self, path: &str) -> bool {
        let below = |dir: &str| {
            dir.strip_prefix(path).is_some_and(|rest| {
                path.ends_with(MAIN_SEPARATOR) || rest.starts_with(MAIN_SEPARATOR)
            })
        };
        if self
            .folders
            .iter()
            .any(|folder| !folder.rows.is_empty() && (folder.path == path || below(&folder.path)))
        {
            return true;
        }
        let (number, name) = match path.rfind(MAIN_SEPARATOR) {
            Some(at) => (
                self.folder_numbers.get(&path[..at]).copied(),
                &path[at + 1..],
            ),
            None => (Some(0), path),
        };
        let Some(rows) = number.and_then(|number| self.folders.get(number)) else {
            return false;
        };
        // Each row is a kind byte, the name and a 0 byte.
        let mut rest = rows.rows.as_slice();
        while let Some((_, tail)) = rest.split_first() {
            let len = tail
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(tail.len());
            if &tail[..len] == name.as_bytes() {
                return true;
            }
            rest = tail.get(len + 1..).unwrap_or(&[]);
        }
        false
    }

    fn note_problem(&mut self, message: String) {
        self.problem.get_or_insert(message);
    }

    fn kind_number(&mut self, kind: &str) -> u8 {
        if let Some(at) = self.kinds.iter().position(|known| known == kind) {
            return at as u8;
        }
        if self.kinds.len() >= 255 || kind.contains('\0') {
            self.note_problem("more than 255 distinct kinds, or a kind with a 0 byte".to_string());
            return 0;
        }
        self.kinds.push(kind.to_string());
        (self.kinds.len() - 1) as u8
    }

    fn folder_number(&mut self, dir: &str) -> usize {
        if self.last_folder != 0 && self.folders[self.last_folder].path == dir {
            return self.last_folder;
        }
        let number = match self.folder_numbers.get(dir) {
            Some(number) => *number,
            None => self.add_folder(dir),
        };
        self.last_folder = number;
        number
    }

    fn add_folder(&mut self, dir: &str) -> usize {
        let number = self.folders.len();
        self.folders.push(FolderRows::new(dir));
        self.folder_numbers.insert(dir.to_string(), number);
        number
    }

    /// Makes sure every ancestor of every folder is in the table.
    fn add_missing_ancestors(&mut self) {
        let mut next = 1;
        while next < self.folders.len() {
            let parent = self.folders[next]
                .path
                .rfind(MAIN_SEPARATOR)
                .map(|at| self.folders[next].path[..at].to_string());
            if let Some(parent) = parent {
                if !self.folder_numbers.contains_key(&parent) {
                    self.add_folder(&parent);
                }
            }
            next += 1;
        }
    }

    /// The complete file image. `snapshot_id` 0 is stored as 1.
    pub fn finish(mut self, snapshot_id: u64) -> Result<Vec<u8>> {
        if let Some(problem) = self.problem.take() {
            bail!("{problem}");
        }
        self.add_missing_ancestors();
        if self.folders.len() > u32::MAX as usize {
            bail!("more than {} folders", u32::MAX);
        }
        // Folder 0 first, then the others by full path as bytes.
        let mut order: Vec<usize> = (1..self.folders.len()).collect();
        order.sort_unstable_by(|a, b| self.folders[*a].path.cmp(&self.folders[*b].path));
        order.insert(0, 0);
        let mut new_number = vec![0u32; order.len()];
        for (new, old) in order.iter().enumerate() {
            new_number[*old] = new as u32;
        }

        let mut parents = Vec::new();
        let mut name_ends = Vec::new();
        let mut names = Vec::new();
        let mut first_rows = Vec::new();
        let mut blocks = BlockWriter::new();
        for old in &order {
            let folder = &self.folders[*old];
            let (parent, name) = match folder.path.rfind(MAIN_SEPARATOR) {
                _ if *old == 0 => (u32::MAX, ""),
                Some(at) => {
                    let above = self.folder_numbers.get(&folder.path[..at]);
                    (
                        new_number[*above.context("a folder lost its parent")?],
                        &folder.path[at + 1..],
                    )
                }
                None => (0, folder.path.as_str()),
            };
            parents.extend_from_slice(&parent.to_le_bytes());
            names.extend_from_slice(name.as_bytes());
            let end = u32::try_from(names.len()).context("folder names pass 4 GB")?;
            name_ends.extend_from_slice(&end.to_le_bytes());
            first_rows.extend_from_slice(&u32::try_from(blocks.rows)?.to_le_bytes());
            for (name, kind) in merged_rows(&folder.rows) {
                blocks.push(name, kind)?;
            }
        }
        first_rows.extend_from_slice(
            &u32::try_from(blocks.rows)
                .context("more than 4294967295 rows")?
                .to_le_bytes(),
        );
        let rows = blocks.rows as u64;
        let (block_index, block_data) = blocks.finish()?;

        let mut kinds = Vec::new();
        for kind in &self.kinds {
            kinds.extend_from_slice(kind.as_bytes());
            kinds.push(0);
        }
        let sections = [
            kinds,
            parents,
            name_ends,
            names,
            first_rows,
            block_index,
            block_data,
        ];
        Ok(assemble(snapshot_id, rows, order.len() as u64, &sections))
    }
}

/// A folder's rows sorted by name, with duplicates merged so the later push wins.
fn merged_rows(buffer: &[u8]) -> Vec<(&[u8], u8)> {
    let mut rows: Vec<(&[u8], u8)> = Vec::new();
    let mut at = 0;
    while let Some((kind, rest)) = buffer.get(at).zip(buffer.get(at + 1..)) {
        let Some(len) = memchr::memchr(0, rest) else {
            break;
        };
        rows.push((&rest[..len], *kind));
        at += len + 2;
    }
    rows.sort_by(|a, b| a.0.cmp(b.0));
    let mut merged: Vec<(&[u8], u8)> = Vec::with_capacity(rows.len());
    for row in rows {
        match merged.last_mut() {
            Some(last) if last.0 == row.0 => *last = row,
            _ => merged.push(row),
        }
    }
    merged
}

fn assemble(snapshot_id: u64, rows: u64, folders: u64, sections: &[Vec<u8>; SECTIONS]) -> Vec<u8> {
    let mut image = Vec::with_capacity(HEADER_LEN + sections.iter().map(Vec::len).sum::<usize>());
    image.extend_from_slice(MAGIC);
    image.extend_from_slice(&INDEX_FORMAT.to_le_bytes());
    image.extend_from_slice(&0u32.to_le_bytes());
    for value in [snapshot_id.max(1), rows, folders] {
        image.extend_from_slice(&value.to_le_bytes());
    }
    let mut offset = HEADER_LEN as u64;
    for section in sections {
        image.extend_from_slice(&offset.to_le_bytes());
        image.extend_from_slice(&(section.len() as u64).to_le_bytes());
        offset += section.len() as u64;
    }
    for section in sections {
        image.extend_from_slice(section);
    }
    image
}

/// Collects rows in file order and compresses them a block at a time.
struct BlockWriter {
    names: Vec<u8>,
    case: Vec<u8>,
    kinds: Vec<u8>,
    index: Vec<u8>,
    data: Vec<u8>,
    rows: usize,
}

impl BlockWriter {
    fn new() -> Self {
        Self {
            names: Vec::new(),
            case: Vec::new(),
            kinds: Vec::new(),
            index: Vec::new(),
            data: Vec::new(),
            rows: 0,
        }
    }

    fn push(&mut self, name: &[u8], kind: u8) -> Result<()> {
        self.case
            .resize((self.names.len() + name.len() + 1).div_ceil(8), 0);
        for byte in name {
            let bit = self.names.len();
            if let Some(flags) = self
                .case
                .get_mut(bit / 8)
                .filter(|_| byte.is_ascii_uppercase())
            {
                *flags |= 1 << (bit % 8);
            }
            self.names.push(byte.to_ascii_lowercase());
        }
        self.names.push(0);
        self.kinds.push(kind);
        self.rows += 1;
        if self.kinds.len() == BLOCK_ROWS {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.kinds.is_empty() {
            return Ok(());
        }
        for part in [&self.names, &self.case, &self.kinds] {
            let packed = lz4_flex::block::compress(part);
            self.index
                .extend_from_slice(&(self.data.len() as u64).to_le_bytes());
            self.index
                .extend_from_slice(&u32::try_from(packed.len())?.to_le_bytes());
            self.index
                .extend_from_slice(&u32::try_from(part.len())?.to_le_bytes());
            self.data.extend_from_slice(&packed);
        }
        self.names.clear();
        self.case.clear();
        self.kinds.clear();
        Ok(())
    }

    fn finish(mut self) -> Result<(Vec<u8>, Vec<u8>)> {
        self.flush()?;
        Ok((self.index, self.data))
    }
}

/// Writes `image` to `<final_path>.tmp`, syncs it and renames it into place.
pub fn install(image: &[u8], final_path: &Path) -> std::io::Result<()> {
    let mut tmp_name = OsString::from(final_path.as_os_str());
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);

    let written = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(image)?;
        // Without the sync a power loss could leave a renamed file that is only partly on disk.
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, final_path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

// ---------- reading ----------

enum Bytes {
    #[cfg(unix)]
    Mapped(memmap2::Mmap),
    Owned(Vec<u8>),
}

impl Bytes {
    fn as_slice(&self) -> &[u8] {
        match self {
            #[cfg(unix)]
            Bytes::Mapped(map) => map,
            Bytes::Owned(bytes) => bytes,
        }
    }
}

/// A validated name file, either mapped from disk (Unix) or held in memory.
pub struct NameFile {
    bytes: Bytes,
    snapshot_id: u64,
    rows: usize,
    folders: usize,
    kinds: Vec<String>,
    sections: [Range<usize>; SECTIONS],
}

impl NameFile {
    pub fn open(path: &Path) -> Result<Self> {
        #[cfg(unix)]
        {
            let file = fs::File::open(path)
                .with_context(|| format!("open name file {}", path.display()))?;
            // SAFETY: the file is never modified in place. Writers build a new file and
            // rename it over this one, so the mapped bytes stay unchanged for as long as
            // this mapping lives.
            let map = unsafe { memmap2::Mmap::map(&file) }
                .with_context(|| format!("map name file {}", path.display()))?;
            Self::parse(Bytes::Mapped(map))
        }
        #[cfg(not(unix))]
        {
            // No process keeps the file open, so a rename over it can succeed.
            let bytes =
                fs::read(path).with_context(|| format!("read name file {}", path.display()))?;
            Self::parse(Bytes::Owned(bytes))
        }
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        Self::parse(Bytes::Owned(bytes))
    }

    fn parse(bytes: Bytes) -> Result<Self> {
        let data = bytes.as_slice();
        if data.get(..8) != Some(MAGIC.as_slice()) {
            bail!("not a name file");
        }
        let format = read_u32(data, 8).context("name file header is cut short")?;
        if format > INDEX_FORMAT {
            bail!(
                "this name file was written by a newer lctr (format {format}); run `lctr update`"
            );
        }
        if format == 0 || data.len() < HEADER_LEN {
            bail!("name file header is not valid");
        }
        let field = |at: usize| read_u64(data, at).context("name file header is cut short");
        let snapshot_id = field(16)?;
        if snapshot_id == 0 {
            bail!("name file has no snapshot id");
        }
        let rows = usize::try_from(field(24)?).context("row count is too large")?;
        let folders = usize::try_from(field(32)?).context("folder count is too large")?;
        if rows > u32::MAX as usize || folders == 0 || folders > u32::MAX as usize {
            bail!("name file counts are not valid");
        }
        let mut sections = [0..0, 0..0, 0..0, 0..0, 0..0, 0..0, 0..0];
        for (number, section) in sections.iter_mut().enumerate() {
            let offset = usize::try_from(field(40 + number * 16)?).context("section offset")?;
            let length = usize::try_from(field(48 + number * 16)?).context("section length")?;
            let end = offset.checked_add(length).context("section is too large")?;
            if offset < HEADER_LEN || end > data.len() {
                bail!("name file section {number} lies outside the file");
            }
            *section = offset..end;
        }
        let blocks = rows.div_ceil(BLOCK_ROWS);
        let expected = [
            (PARENTS, folders.saturating_mul(4)),
            (NAME_ENDS, folders.saturating_mul(4)),
            (FIRST_ROWS, (folders + 1).saturating_mul(4)),
            (BLOCK_INDEX, blocks.saturating_mul(48)),
        ];
        for (number, length) in expected {
            if sections[number].len() != length {
                bail!("name file section {number} has the wrong length");
            }
        }
        let kinds = parse_kinds(data.get(sections[KINDS].clone()).unwrap_or(&[]))?;
        let file = Self {
            bytes,
            snapshot_id,
            rows,
            folders,
            kinds,
            sections,
        };
        file.validate_folder_tables()?;
        Ok(file)
    }

    /// Checks what path building relies on, so a damaged table cannot loop or run off the end.
    fn validate_folder_tables(&self) -> Result<()> {
        let names_len = self.section(NAMES).len();
        let (mut last_end, mut last_first) = (0, 0);
        let bad = || anyhow!("name file folder table is not valid");
        if self.table(FIRST_ROWS, 0) != Some(0) {
            return Err(bad());
        }
        for folder in 0..self.folders {
            let end = self.table(NAME_ENDS, folder).ok_or_else(bad)?;
            let parent = self.table(PARENTS, folder).ok_or_else(bad)?;
            let first = self.table(FIRST_ROWS, folder + 1).ok_or_else(bad)?;
            let parent_ok = if folder == 0 {
                parent == u32::MAX as usize && end == 0
            } else {
                parent < folder
            };
            if end < last_end || end > names_len || first < last_first || !parent_ok {
                return Err(bad());
            }
            (last_end, last_first) = (end, first);
        }
        if last_first != self.rows {
            return Err(bad());
        }
        Ok(())
    }

    pub fn snapshot_id(&self) -> u64 {
        self.snapshot_id
    }

    pub fn len(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    pub fn block_count(&self) -> usize {
        self.rows.div_ceil(BLOCK_ROWS)
    }

    /// A cursor for one thread. It owns the decode buffers.
    pub fn reader(&self) -> Reader<'_> {
        Reader {
            file: self,
            block: None,
            block_ok: false,
            case: Vec::new(),
            kinds: Vec::new(),
            folder_rows: 0..0,
            path: String::new(),
            prefix_len: 0,
            folder_ok: true,
            name: Vec::new(),
            chain: Vec::new(),
            lookup: LookupCache::default(),
        }
    }

    /// The kind stored for exactly `path`. `cache` keeps every block it decodes, so many
    /// lookups in one query decode each block once.
    pub fn kind_of(&self, cache: &mut LookupCache, path: &str) -> Option<&str> {
        let row = self.locate(cache, path)?;
        let kind = *cache
            .block(self, row / BLOCK_ROWS)?
            .kinds
            .get(row % BLOCK_ROWS)?;
        self.kinds.get(usize::from(kind)).map(String::as_str)
    }

    fn section(&self, which: usize) -> &[u8] {
        let range = self.sections.get(which).cloned().unwrap_or(0..0);
        self.bytes.as_slice().get(range).unwrap_or(&[])
    }

    fn table(&self, which: usize, index: usize) -> Option<usize> {
        read_u32(self.section(which), index.checked_mul(4)?).map(|value| value as usize)
    }

    fn first_row(&self, folder: usize) -> usize {
        self.table(FIRST_ROWS, folder).unwrap_or(self.rows)
    }

    fn block_rows(&self, block: usize) -> usize {
        self.rows.saturating_sub(block * BLOCK_ROWS).min(BLOCK_ROWS)
    }

    fn folder_name(&self, folder: usize) -> &[u8] {
        let start = folder
            .checked_sub(1)
            .map_or(Some(0), |before| self.table(NAME_ENDS, before));
        let end = self.table(NAME_ENDS, folder);
        start
            .zip(end)
            .and_then(|(start, end)| self.section(NAMES).get(start..end))
            .unwrap_or(&[])
    }

    /// Writes the full path of `folder` (not folder 0) into `out`.
    fn write_folder_path(&self, folder: usize, chain: &mut Vec<usize>, out: &mut Vec<u8>) {
        chain.clear();
        let mut at = folder;
        while at != 0 && chain.len() <= self.folders {
            chain.push(at);
            at = self.table(PARENTS, at).unwrap_or(0);
        }
        out.clear();
        for (position, folder) in chain.iter().rev().enumerate() {
            if position > 0 {
                out.push(SEP);
            }
            out.extend_from_slice(self.folder_name(*folder));
        }
    }

    /// The first folder, from 1, whose full path is not below `target` in byte order.
    fn folder_lower_bound(&self, target: &[u8]) -> usize {
        let (mut low, mut high) = (1, self.folders);
        let (mut chain, mut path) = (Vec::new(), Vec::new());
        while low < high {
            let middle = low + (high - low) / 2;
            self.write_folder_path(middle, &mut chain, &mut path);
            if path.as_slice() < target {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }

    /// The folder whose full path is `path`, if there is one.
    fn find_folder(&self, path: &[u8]) -> Option<usize> {
        let folder = self.folder_lower_bound(path);
        let (mut chain, mut found) = (Vec::new(), Vec::new());
        if folder >= self.folders {
            return None;
        }
        self.write_folder_path(folder, &mut chain, &mut found);
        (found == path).then_some(folder)
    }

    /// The packed bytes of one part of a block (0 names, 1 case bits, 2 kinds) and the
    /// length they decode to.
    fn part(&self, block: usize, part: usize) -> Option<(&[u8], usize)> {
        let at = block.checked_mul(48)?.checked_add(part * 16)?;
        let index = self.section(BLOCK_INDEX);
        let offset = usize::try_from(read_u64(index, at)?).ok()?;
        let packed = read_u32(index, at + 8)? as usize;
        let raw = read_u32(index, at + 12)? as usize;
        let source = self
            .section(BLOCKS)
            .get(offset..offset.checked_add(packed)?)?;
        (raw <= MAX_PART_BYTES).then_some((source, raw))
    }

    /// Decodes one part of a block into `out`. `false` when the part cannot be read; `out`
    /// is then empty. `out` keeps its bytes between calls, so only growth is zeroed; a part
    /// that decodes overwrites every byte of it.
    fn decode_part(&self, block: usize, part: usize, out: &mut Vec<u8>) -> bool {
        let decoded = self.part(block, part).is_some_and(|(source, raw)| {
            out.resize(raw, 0);
            lz4_flex::block::decompress_into(source, out).is_ok_and(|written| written == raw)
        });
        if !decoded {
            out.clear();
        }
        decoded
    }

    /// The row number of exactly `path`, by binary search inside its folder.
    fn locate(&self, cache: &mut LookupCache, path: &str) -> Option<usize> {
        let (folder, name) = match path.rfind(MAIN_SEPARATOR) {
            Some(at) => (self.find_folder(&path.as_bytes()[..at])?, &path[at + 1..]),
            None => (0, path),
        };
        let (mut low, mut high) = (self.first_row(folder), self.first_row(folder + 1));
        let mut name_buffer = Vec::new();
        while low < high {
            let middle = low + (high - low) / 2;
            let block = cache.block(self, middle / BLOCK_ROWS)?;
            let index = middle % BLOCK_ROWS;
            name_buffer.clear();
            name_buffer.extend_from_slice(block.lower_name(index)?);
            restore_case(&mut name_buffer, &block.case, *block.starts.get(index)?);
            match name_buffer.as_slice().cmp(name.as_bytes()) {
                Ordering::Less => low = middle + 1,
                Ordering::Greater => high = middle,
                Ordering::Equal => return Some(middle),
            }
        }
        None
    }
}

fn parse_kinds(bytes: &[u8]) -> Result<Vec<String>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let text = std::str::from_utf8(bytes).context("kinds are not valid UTF-8")?;
    let text = text
        .strip_suffix('\0')
        .context("kinds section does not end with a 0 byte")?;
    let kinds: Vec<String> = text.split('\0').map(str::to_string).collect();
    if kinds.len() > 255 {
        bail!("name file lists more than 255 kinds");
    }
    Ok(kinds)
}

/// One decoded block, kept by [`LookupCache`].
struct DecodedBlock {
    names: Vec<u8>,
    case: Vec<u8>,
    kinds: Vec<u8>,
    /// Where each row's name starts in `names`.
    starts: Vec<usize>,
}

impl DecodedBlock {
    fn lower_name(&self, index: usize) -> Option<&[u8]> {
        let rest = self.names.get(*self.starts.get(index)?..)?;
        rest.get(..memchr::memchr(0, rest)?)
    }
}

/// Decoded blocks for [`NameFile::kind_of`], so many lookups decode each block once.
#[derive(Default)]
pub struct LookupCache {
    blocks: HashMap<usize, Option<DecodedBlock>>,
}

impl LookupCache {
    fn block(&mut self, file: &NameFile, block: usize) -> Option<&DecodedBlock> {
        self.blocks
            .entry(block)
            .or_insert_with(|| decode_block(file, block))
            .as_ref()
    }
}

/// Decodes the three parts of one block.
fn decode_block(file: &NameFile, block: usize) -> Option<DecodedBlock> {
    let (mut names, mut case, mut kinds) = (Vec::new(), Vec::new(), Vec::new());
    let decoded = file.decode_part(block, 0, &mut names)
        && file.decode_part(block, 1, &mut case)
        && file.decode_part(block, 2, &mut kinds);
    let mut starts = Vec::with_capacity(kinds.len());
    let mut at = 0;
    for end in memchr::memchr_iter(0, &names) {
        starts.push(at);
        at = end + 1;
    }
    let rows = file.block_rows(block);
    (decoded && kinds.len() == rows && starts.len() == rows).then_some(DecodedBlock {
        names,
        case,
        kinds,
        starts,
    })
}

pub struct Row<'a> {
    pub path: &'a str,
    /// The last component of `path`, in its original case.
    pub name: &'a str,
    pub kind: &'a str,
}

/// A cursor over one [`NameFile`] for one thread.
pub struct Reader<'a> {
    file: &'a NameFile,
    /// The block whose case bits and kinds are loaded, and whether they decoded.
    block: Option<usize>,
    block_ok: bool,
    case: Vec<u8>,
    kinds: Vec<u8>,
    /// The rows of the folder whose path is in `path[..prefix_len]`.
    folder_rows: Range<usize>,
    path: String,
    prefix_len: usize,
    /// Whether the folder path is valid UTF-8; the rows of a folder with a bad path are unreadable.
    folder_ok: bool,
    /// Scratch space for one name.
    name: Vec<u8>,
    chain: Vec<usize>,
    lookup: LookupCache,
}

impl Reader<'_> {
    /// Decodes the lowercased names of `block` into `names`, each followed by a 0 byte.
    /// `false` when the block cannot be read; `names` is then empty.
    pub fn load_names(&mut self, block: usize, names: &mut Vec<u8>) -> bool {
        self.file.decode_part(block, 0, names)
    }

    /// Row `index` of `block`. `lower_name` is its lowercased name as found in the block's
    /// names and `name_start` is where that name starts in them.
    pub fn row(
        &mut self,
        block: usize,
        index: usize,
        lower_name: &[u8],
        name_start: usize,
    ) -> Option<Row<'_>> {
        let row = block.checked_mul(BLOCK_ROWS)?.checked_add(index)?;
        if index >= BLOCK_ROWS || row >= self.file.rows || !self.load_block(block) {
            return None;
        }
        let kind = self.file.kinds.get(usize::from(*self.kinds.get(index)?))?;
        self.enter_folder(row);
        if !self.folder_ok {
            return None;
        }
        self.name.clear();
        self.name.extend_from_slice(lower_name);
        restore_case(&mut self.name, &self.case, name_start);
        let name = std::str::from_utf8(&self.name).ok()?;
        self.path.truncate(self.prefix_len);
        self.path.push_str(name);
        Some(Row {
            path: &self.path,
            name,
            kind,
        })
    }

    /// Loads the case bits and kinds of `block` the first time a row of it is asked for.
    fn load_block(&mut self, block: usize) -> bool {
        if self.block != Some(block) {
            let file = self.file;
            self.block = Some(block);
            self.block_ok = file.decode_part(block, 1, &mut self.case)
                && file.decode_part(block, 2, &mut self.kinds)
                && self.kinds.len() == file.block_rows(block);
        }
        self.block_ok
    }

    /// Makes `path[..prefix_len]` the path of the folder that holds `row`, plus a separator.
    fn enter_folder(&mut self, row: usize) {
        if self.folder_rows.contains(&row) {
            return;
        }
        let file = self.file;
        // The last folder whose first row is at or before `row`.
        let (mut low, mut high) = (0, file.folders);
        while high - low > 1 {
            let middle = low + (high - low) / 2;
            if file.first_row(middle) <= row {
                low = middle;
            } else {
                high = middle;
            }
        }
        self.folder_rows = file.first_row(low)..file.first_row(low + 1);
        self.path.clear();
        self.name.clear();
        if low != 0 {
            file.write_folder_path(low, &mut self.chain, &mut self.name);
            self.name.push(SEP);
        }
        self.folder_ok = std::str::from_utf8(&self.name).is_ok_and(|folder| {
            self.path.push_str(folder);
            true
        });
        self.prefix_len = self.path.len();
    }

    /// Every row in `rows`, in file order, until `visit` returns false.
    pub fn for_each(&mut self, rows: Range<usize>, visit: &mut dyn FnMut(Row<'_>) -> bool) {
        self.visit_rows(rows, visit);
    }

    /// Returns false when `visit` asked to stop.
    fn visit_rows(&mut self, rows: Range<usize>, visit: &mut dyn FnMut(Row<'_>) -> bool) -> bool {
        let end = rows.end.min(self.file.rows);
        let mut row = rows.start;
        let mut names = Vec::new();
        while row < end {
            let block = row / BLOCK_ROWS;
            let block_end = ((block + 1) * BLOCK_ROWS).min(end);
            if !self.load_names(block, &mut names) {
                row = block_end;
                continue;
            }
            let mut start = 0;
            for _ in 0..row % BLOCK_ROWS {
                match memchr::memchr(0, names.get(start..).unwrap_or(&[])) {
                    Some(at) => start += at + 1,
                    None => break,
                }
            }
            while row < block_end {
                let Some(len) = memchr::memchr(0, names.get(start..).unwrap_or(&[])) else {
                    break;
                };
                let Some(lower) = names.get(start..start + len) else {
                    break;
                };
                if let Some(found) = self.row(block, row % BLOCK_ROWS, lower, start) {
                    if !visit(found) {
                        return false;
                    }
                }
                start += len + 1;
                row += 1;
            }
            row = block_end;
        }
        true
    }

    /// The row for `path` itself and every row beneath it, until `visit` returns false.
    pub fn for_each_under(&mut self, path: &str, visit: &mut dyn FnMut(Row<'_>) -> bool) {
        let file = self.file;
        if let Some(row) = file.locate(&mut self.lookup, path) {
            if !self.visit_rows(row..row + 1, visit) {
                return;
            }
        }
        if let Some(folder) = file.find_folder(path.as_bytes()) {
            if !self.visit_rows(file.first_row(folder)..file.first_row(folder + 1), visit) {
                return;
            }
        }
        // Folders below `path` sort as one run, from `path + SEP` up to `path + (SEP + 1)`.
        let mut below = path.as_bytes().to_vec();
        below.push(SEP);
        let low = file.folder_lower_bound(&below);
        below.pop();
        below.push(SEP + 1);
        let high = file.folder_lower_bound(&below);
        if low < high {
            self.visit_rows(file.first_row(low)..file.first_row(high), visit);
        }
    }
}
