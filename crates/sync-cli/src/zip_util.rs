use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::Path;
use walkdir::WalkDir;
use zip::{write::SimpleFileOptions, ZipWriter};

/// Walk `skill_dir` and produce an in-memory zip. Returns (zip_bytes, sha256_hex).
pub fn zip_skill_dir(skill_dir: &Path) -> Result<(Vec<u8>, String)> {
    let buf = Vec::new();
    let cursor = std::io::Cursor::new(buf);
    let mut zip = ZipWriter::new(cursor);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    for entry in WalkDir::new(skill_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let rel = entry
            .path()
            .strip_prefix(skill_dir)
            .context("strip prefix")?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        zip.start_file(&rel_str, options)
            .context("zip start file")?;
        let mut f = std::fs::File::open(entry.path()).context("open file")?;
        let mut data = Vec::new();
        f.read_to_end(&mut data).context("read file")?;
        zip.write_all(&data).context("write to zip")?;
    }

    let cursor = zip.finish().context("finish zip")?;
    let bytes = cursor.into_inner();

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let checksum = hex::encode(hasher.finalize());

    Ok((bytes, checksum))
}

/// Extract zip bytes into `dest_dir/<skill_name>/`.
pub fn unzip_skill(name: &str, zip_bytes: &[u8], dest_dir: &Path) -> Result<std::path::PathBuf> {
    let cursor = std::io::Cursor::new(zip_bytes);
    let mut archive = zip::ZipArchive::new(cursor).context("open zip")?;
    let out = dest_dir.join(name);
    std::fs::create_dir_all(&out).context("create output directory")?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).context("read zip entry")?;
        if file.is_dir() {
            continue;
        }
        let out_path = out.join(file.name());
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).context("create dir")?;
        }
        let mut out_file = std::fs::File::create(&out_path).context("create output file")?;
        std::io::copy(&mut file, &mut out_file).context("extract file")?;
    }
    Ok(out)
}

/// Wrap a single file into an in-memory zip. Returns (zip_bytes, sha256_hex).
/// The file is stored at the root of the archive using its original filename.
pub fn zip_single_file(file_path: &Path) -> Result<(Vec<u8>, String)> {
    let buf = Vec::new();
    let cursor = std::io::Cursor::new(buf);
    let mut zip = ZipWriter::new(cursor);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let file_name = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .context("file has no name")?;
    zip.start_file(file_name, options)
        .context("zip start file")?;
    let mut f = std::fs::File::open(file_path).context("open file")?;
    let mut data = Vec::new();
    f.read_to_end(&mut data).context("read file")?;
    zip.write_all(&data).context("write to zip")?;

    let cursor = zip.finish().context("finish zip")?;
    let bytes = cursor.into_inner();

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let checksum = hex::encode(hasher.finalize());

    Ok((bytes, checksum))
}

/// Compute SHA-256 of a skill directory (same as zip_skill_dir but returns only the checksum).
pub fn checksum_skill_dir(skill_dir: &Path) -> Result<String> {
    let (_, checksum) = zip_skill_dir(skill_dir)?;
    Ok(checksum)
}
