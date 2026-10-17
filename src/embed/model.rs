//! Model files for the embedding runtime: pinned Hugging Face files, SHA-256
//! verification, resumable downloads, and the optional bundled text model.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::db::locator_data_dir;

pub const HF_REPO: &str = "ggml-org/embeddinggemma-2-GGUF";
pub const HF_REVISION: &str = "bfcd298762cc34d0357ece5ebdd31791a3a374d8";
const HF_BASE_DEFAULT: &str = "https://huggingface.co";
const CHUNK_BYTES: usize = 1024 * 1024;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// A model file pinned by name, exact size, and SHA-256.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelFile {
    pub name: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}

pub const TEXT_MODEL: ModelFile = ModelFile {
    name: "embeddinggemma-2-Q8_0.gguf",
    bytes: 309_855_456,
    sha256: "2188ac1deca4b77dffefd603c2776a9d76d9d74ec01841392982ebb840b09135",
};

pub const VISION_MODEL: ModelFile = ModelFile {
    name: "mmproj-embeddinggemma-2-Q8_0.gguf",
    bytes: 554_821_024,
    sha256: "c4a8a52691ecef40618438928bdf9e68379b854e24166f292592353db0aab64f",
};

/// Text GGUF embedded at build time. `LCTR_BUNDLED_MODEL` must name the
/// absolute path of `embeddinggemma-2-Q8_0.gguf`; without it the build fails
/// with rustc's "environment variable not defined" error.
#[cfg(feature = "bundled-model")]
static BUNDLED_TEXT_MODEL: &[u8] = include_bytes!(env!("LCTR_BUNDLED_MODEL"));

/// Bytes received so far for the file being fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadProgress {
    pub downloaded: u64,
    pub total: u64,
}

fn bundled_text_model() -> Option<&'static [u8]> {
    #[cfg(feature = "bundled-model")]
    {
        Some(BUNDLED_TEXT_MODEL)
    }
    #[cfg(not(feature = "bundled-model"))]
    {
        None
    }
}

pub fn models_dir() -> Result<PathBuf> {
    Ok(locator_data_dir()?.join("models"))
}

pub fn model_path(file: &ModelFile) -> Result<PathBuf> {
    Ok(models_dir()?.join(file.name))
}

/// Cheap check: the file has the pinned size and its `.verified` marker holds the pinned hash.
pub fn is_present(file: &ModelFile) -> bool {
    models_dir()
        .map(|dir| is_present_in(&dir, file))
        .unwrap_or(false)
}

pub fn ensure_text_model(progress: &mut dyn FnMut(DownloadProgress)) -> Result<PathBuf> {
    ensure_model_in(
        &models_dir()?,
        &TEXT_MODEL,
        &hf_base(),
        bundled_text_model(),
        progress,
    )
}

pub fn ensure_vision_model(progress: &mut dyn FnMut(DownloadProgress)) -> Result<PathBuf> {
    ensure_model_in(&models_dir()?, &VISION_MODEL, &hf_base(), None, progress)
}

/// Deletes both model files plus their partial and marker siblings. Returns bytes freed.
pub fn remove_models() -> Result<u64> {
    remove_models_in(&models_dir()?)
}

fn hf_base() -> String {
    std::env::var("LCTR_HF_BASE")
        .unwrap_or_else(|_| HF_BASE_DEFAULT.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn part_name(file: &ModelFile) -> String {
    format!("{}.part", file.name)
}

fn marker_name(file: &ModelFile) -> String {
    format!("{}.verified", file.name)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("open {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; CHUNK_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("read {} for hashing", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

fn write_marker(dir: &Path, file: &ModelFile) -> Result<()> {
    let marker = dir.join(marker_name(file));
    fs::write(&marker, file.sha256).with_context(|| format!("write {}", marker.display()))
}

fn is_present_in(dir: &Path, file: &ModelFile) -> bool {
    let Ok(metadata) = fs::metadata(dir.join(file.name)) else {
        return false;
    };
    if metadata.len() != file.bytes {
        return false;
    }
    fs::read_to_string(dir.join(marker_name(file)))
        .map(|contents| contents.trim() == file.sha256)
        .unwrap_or(false)
}

fn ensure_model_in(
    dir: &Path,
    file: &ModelFile,
    base: &str,
    bundled: Option<&[u8]>,
    progress: &mut dyn FnMut(DownloadProgress),
) -> Result<PathBuf> {
    let final_path = dir.join(file.name);
    if is_present_in(dir, file) {
        return Ok(final_path);
    }
    fs::create_dir_all(dir)
        .with_context(|| format!("create models directory {}", dir.display()))?;

    if let Some(bytes) = bundled {
        return install_bundled(dir, file, bytes);
    }

    // A file without a marker is hashed once; it is kept only when it matches.
    if fs::metadata(&final_path).is_ok() {
        if file_matches(&final_path, file)? {
            write_marker(dir, file)?;
            return Ok(final_path);
        }
        fs::remove_file(&final_path)
            .with_context(|| format!("remove mismatched {}", final_path.display()))?;
    }

    download_in(dir, file, base, progress)
}

fn file_matches(path: &Path, file: &ModelFile) -> Result<bool> {
    let length = fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    if length != file.bytes {
        return Ok(false);
    }
    Ok(sha256_file(path)? == file.sha256)
}

fn install_bundled(dir: &Path, file: &ModelFile, bytes: &[u8]) -> Result<PathBuf> {
    let actual = hex(&Sha256::digest(bytes));
    if bytes.len() as u64 != file.bytes || actual != file.sha256 {
        bail!(
            "bundled {} failed verification (expected {} bytes, sha256 {}; found {} bytes, sha256 {}); this is a build error",
            file.name,
            file.bytes,
            file.sha256,
            bytes.len(),
            actual
        );
    }
    let part = dir.join(part_name(file));
    fs::write(&part, bytes)
        .with_context(|| format!("write {} (needs {} bytes free)", part.display(), file.bytes))?;
    let final_path = dir.join(file.name);
    // Windows does not rename over an existing file, so clear any old copy first.
    if final_path.exists() {
        fs::remove_file(&final_path)
            .with_context(|| format!("replace {}", final_path.display()))?;
    }
    fs::rename(&part, &final_path)
        .with_context(|| format!("move {} into place", part.display()))?;
    write_marker(dir, file)?;
    Ok(final_path)
}

fn download_in(
    dir: &Path,
    file: &ModelFile,
    base: &str,
    progress: &mut dyn FnMut(DownloadProgress),
) -> Result<PathBuf> {
    let part = dir.join(part_name(file));
    let final_path = dir.join(file.name);

    let mut existing = match fs::metadata(&part) {
        Ok(metadata) => metadata.len(),
        Err(_) => 0,
    };
    if existing > file.bytes {
        fs::remove_file(&part).with_context(|| format!("remove oversized {}", part.display()))?;
        existing = 0;
    }

    if existing < file.bytes {
        let url = format!("{base}/{HF_REPO}/resolve/{HF_REVISION}/{}", file.name);
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(60))
            .build();
        let mut request = agent
            .get(&url)
            .set("User-Agent", &format!("lctr/{}", env!("CARGO_PKG_VERSION")));
        if existing > 0 {
            request = request.set("Range", &format!("bytes={existing}-"));
        }
        let response = request.call().map_err(|error| match error {
            ureq::Error::Status(code, _) => {
                anyhow!("HTTP {code} while downloading {}", file.name)
            }
            other => anyhow!("download of {} failed: {other}", file.name),
        })?;

        let status = response.status();
        if status != 200 && status != 206 {
            bail!("unexpected HTTP {status} while downloading {}", file.name);
        }
        // 206 continues the partial file; 200 means the server ignored the range.
        let resumed = status == 206;
        if !resumed {
            existing = 0;
        }
        let mut out = if resumed {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&part)
                .with_context(|| format!("open {} for resume", part.display()))?
        } else {
            File::create(&part).with_context(|| format!("create {}", part.display()))?
        };

        let mut reader = response.into_reader();
        let mut buffer = vec![0u8; CHUNK_BYTES];
        let mut downloaded = existing;
        let mut last_report: Option<Instant> = None;
        loop {
            let read = reader.read(&mut buffer).with_context(|| {
                format!(
                    "download of {} interrupted at {} of {} bytes; run the command again to resume",
                    file.name, downloaded, file.bytes
                )
            })?;
            if read == 0 {
                break;
            }
            out.write_all(&buffer[..read]).with_context(|| {
                format!("write {} (needs {} bytes free)", part.display(), file.bytes)
            })?;
            downloaded += read as u64;
            if last_report.is_none_or(|at| at.elapsed() >= PROGRESS_INTERVAL) {
                progress(DownloadProgress {
                    downloaded,
                    total: file.bytes,
                });
                last_report = Some(Instant::now());
            }
        }
        out.flush()
            .with_context(|| format!("flush {}", part.display()))?;
        drop(out);
        progress(DownloadProgress {
            downloaded,
            total: file.bytes,
        });
    }

    let length = fs::metadata(&part)
        .with_context(|| format!("stat {}", part.display()))?
        .len();
    if length != file.bytes {
        let _ = fs::remove_file(&part);
        bail!(
            "download of {} has {} bytes, expected {}",
            file.name,
            length,
            file.bytes
        );
    }

    let actual = sha256_file(&part)?;
    if actual != file.sha256 {
        let _ = fs::remove_file(&part);
        bail!(
            "{} failed SHA-256 verification: expected {}, got {}",
            file.name,
            file.sha256,
            actual
        );
    }

    if final_path.exists() {
        fs::remove_file(&final_path)
            .with_context(|| format!("replace {}", final_path.display()))?;
    }
    fs::rename(&part, &final_path)
        .with_context(|| format!("move {} into place", part.display()))?;
    write_marker(dir, file)?;
    Ok(final_path)
}

fn remove_models_in(dir: &Path) -> Result<u64> {
    let mut freed = 0u64;
    for file in [&TEXT_MODEL, &VISION_MODEL] {
        for path in [
            dir.join(file.name),
            dir.join(part_name(file)),
            dir.join(marker_name(file)),
        ] {
            if let Ok(metadata) = fs::metadata(&path) {
                freed += metadata.len();
                fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
            }
        }
    }
    Ok(freed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};

    const MIB: usize = 1024 * 1024;

    type SeenRanges = Arc<Mutex<Vec<Option<String>>>>;

    fn sample_body(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i.wrapping_mul(31) % 251) as u8).collect()
    }

    fn fake_model(body: &[u8]) -> ModelFile {
        let sha: &'static str = Box::leak(hex(&Sha256::digest(body)).into_boxed_str());
        ModelFile {
            name: "fake-model.gguf",
            bytes: body.len() as u64,
            sha256: sha,
        }
    }

    /// Serves `requests` connections of `body`. `honor_range` answers Range
    /// requests with 206; `corrupt` flips one byte in the payload.
    fn serve(
        body: Vec<u8>,
        requests: usize,
        honor_range: bool,
        corrupt: bool,
    ) -> (String, SeenRanges, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let base = format!("http://{}", listener.local_addr().expect("local addr"));
        let seen: SeenRanges = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let handle = thread::spawn(move || {
            for _ in 0..requests {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                let mut range: Option<String> = None;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("read header");
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(value) = lower.strip_prefix("range:") {
                        range = Some(value.trim().to_string());
                    }
                }
                recorder.lock().expect("lock").push(range.clone());

                let start = match (&range, honor_range) {
                    (Some(value), true) => value
                        .trim_start_matches("bytes=")
                        .trim_end_matches('-')
                        .parse::<usize>()
                        .unwrap_or(0),
                    _ => 0,
                };
                let mut payload = body[start..].to_vec();
                if corrupt && !payload.is_empty() {
                    let middle = payload.len() / 2;
                    payload[middle] ^= 0xFF;
                }
                let status = if start > 0 {
                    "206 Partial Content"
                } else {
                    "200 OK"
                };
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                stream.write_all(header.as_bytes()).expect("write header");
                stream.write_all(&payload).expect("write body");
                let _ = stream.flush();
            }
        });
        (base, seen, handle)
    }

    #[test]
    fn full_download_verifies_and_writes_marker() {
        let body = sample_body(3 * MIB);
        let file = fake_model(&body);
        let (base, seen, handle) = serve(body.clone(), 1, true, false);
        let dir = tempfile::tempdir().expect("tempdir");

        let mut reports = 0usize;
        let path = ensure_model_in(dir.path(), &file, &base, None, &mut |_| reports += 1)
            .expect("download");
        handle.join().expect("server thread");

        assert_eq!(fs::read(&path).expect("read model"), body);
        assert!(is_present_in(dir.path(), &file));
        assert!(!dir.path().join(part_name(&file)).exists());
        assert!(reports >= 1, "expected at least one progress report");
        assert_eq!(seen.lock().expect("lock").as_slice(), &[None]);
    }

    #[test]
    fn interrupted_download_resumes_with_range_and_appends() {
        let body = sample_body(3 * MIB);
        let file = fake_model(&body);
        let (base, seen, handle) = serve(body.clone(), 1, true, false);
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join(part_name(&file)), &body[..MIB]).expect("seed partial");

        let path = ensure_model_in(dir.path(), &file, &base, None, &mut |_| {}).expect("resume");
        handle.join().expect("server thread");

        assert_eq!(fs::read(&path).expect("read model"), body);
        assert_eq!(
            seen.lock().expect("lock").as_slice(),
            &[Some(format!("bytes={MIB}-"))]
        );
    }

    #[test]
    fn server_ignoring_range_restarts_the_partial_file() {
        let body = sample_body(2 * MIB + 5);
        let file = fake_model(&body);
        let (base, _seen, handle) = serve(body.clone(), 1, false, false);
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join(part_name(&file)), &body[..MIB]).expect("seed partial");

        let path = ensure_model_in(dir.path(), &file, &base, None, &mut |_| {}).expect("restart");
        handle.join().expect("server thread");

        assert_eq!(fs::read(&path).expect("read model"), body);
    }

    #[test]
    fn sha_mismatch_deletes_the_partial_file() {
        let body = sample_body(MIB + 17);
        let file = fake_model(&body);
        let (base, _seen, handle) = serve(body, 1, true, true);
        let dir = tempfile::tempdir().expect("tempdir");

        let error = ensure_model_in(dir.path(), &file, &base, None, &mut |_| {})
            .expect_err("corrupted body must fail");
        handle.join().expect("server thread");

        let message = format!("{error:#}");
        assert!(message.contains("SHA-256"), "{message}");
        assert!(message.contains("expected"), "{message}");
        assert!(!dir.path().join(part_name(&file)).exists());
        assert!(!dir.path().join(file.name).exists());
    }

    #[test]
    fn complete_file_without_marker_is_verified_not_downloaded() {
        let body = sample_body(MIB + 3);
        let file = fake_model(&body);
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join(file.name), &body).expect("seed model");

        // Port 9 is closed here; any download attempt would fail the test.
        let path = ensure_model_in(dir.path(), &file, "http://127.0.0.1:9", None, &mut |_| {})
            .expect("verify existing file");

        assert_eq!(fs::read(&path).expect("read model"), body);
        assert!(is_present_in(dir.path(), &file));
    }

    #[test]
    fn bundled_bytes_are_verified_and_installed() {
        let body = sample_body(MIB + 9);
        let file = fake_model(&body);
        let dir = tempfile::tempdir().expect("tempdir");

        let path = install_bundled(dir.path(), &file, &body).expect("install bundled");
        assert_eq!(fs::read(&path).expect("read model"), body);
        assert!(is_present_in(dir.path(), &file));

        let other = sample_body(MIB + 10);
        let error = install_bundled(dir.path(), &file, &other).expect_err("wrong bytes");
        assert!(format!("{error:#}").contains("build error"));
    }

    #[test]
    fn remove_models_deletes_files_and_reports_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join(TEXT_MODEL.name), vec![0u8; 100]).expect("seed text");
        fs::write(dir.path().join(part_name(&VISION_MODEL)), vec![0u8; 50]).expect("seed part");

        let freed = remove_models_in(dir.path()).expect("remove");
        assert_eq!(freed, 150);
        assert!(!dir.path().join(TEXT_MODEL.name).exists());
        assert!(!dir.path().join(part_name(&VISION_MODEL)).exists());
    }
}
