//! Exchange: keeps the Avid MXF media of two unconnected systems in step through manifest files.
pub mod collect;
pub mod flow;
pub mod plan;
pub mod request;
pub mod scan;

use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use unicode_normalization::UnicodeNormalization;

pub const FORMAT: &str = "mediafiles-exchange-manifest";
pub const DB_FILES: [&str; 2] = ["msmFMID.pmr", "msmMMOB.mdb"];

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Manifest {
    pub format: String,
    pub version: u32,
    pub system: String,
    pub created: u64,
    pub volumes: Vec<Volume>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Volume {
    pub name: String,
    /// Einhängepunkt auf dem System, das das Manifest geschrieben hat
    pub path: String,
    pub folders: Vec<Folder>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Folder {
    pub name: String,
    /// beide Datenbankdateien vorhanden
    pub has_db: bool,
    pub files: Vec<FileEntry>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct FileEntry {
    pub name: String,
    pub size: u64,
    pub hash: String,
}

impl Manifest {
    pub fn load(path: &Path) -> io::Result<Self> {
        let m: Manifest = serde_json::from_slice(&fs::read(path)?)?;
        if m.format != FORMAT {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{} is not an Exchange manifest", path.display())));
        }
        // the other side's volume and folder names become paths of the delivery: each must stay one path part
        let names = m.volumes.iter().flat_map(|v| std::iter::once(&v.name).chain(v.folders.iter().flat_map(|f| std::iter::once(&f.name).chain(f.files.iter().map(|e| &e.name)))));
        if let Some(bad) = names.into_iter().find(|n| !path_part(n)) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{}: invalid name \"{bad}\"", path.display())));
        }
        Ok(m)
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        write_atomic(path, &serde_json::to_vec(self)?)
    }
}

/// Vergleichsschlüssel: macOS speichert Umlaute zerlegt (NFD), Windows zusammengesetzt (NFC) und ignoriert Groß-/Kleinschreibung.
pub fn key(s: &str) -> String {
    s.nfc().collect::<String>().to_lowercase()
}

/// One plain part of a path: not empty, no separator, not "." or "..".
pub fn path_part(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\', '\0'])
}

/// MXF file names in a folder as compare keys, without dot files (AppleDouble "._x.mxf" on exFAT and network drives).
pub fn mxf_names(dir: &Path) -> io::Result<std::collections::HashSet<String>> {
    Ok(fs::read_dir(dir)?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.') && n.to_lowercase().ends_with(".mxf"))
        .map(|n| key(&n))
        .collect())
}

/// Kept free on every drive the exchange writes to: Media Composer still has to write its databases.
pub const RESERVE: u64 = 1 << 30;

/// Fails when `needed` bytes plus `RESERVE` do not fit on the drive of `path` (or its nearest existing parent).
pub fn check_space(path: &Path, needed: u64) -> io::Result<()> {
    let Some(free) = path.ancestors().find(|p| p.exists()).and_then(free_bytes) else { return Ok(()) };
    // test lab only (scripts/exchange-disks.sh): small disk images can't keep 1 GB free
    let reserve = std::env::var("MFC_LAB_RESERVE").ok().and_then(|v| v.parse().ok()).unwrap_or(RESERVE);
    if free < needed.saturating_add(reserve) {
        return Err(io::Error::new(io::ErrorKind::StorageFull, format!("{}: not enough space – needs {} MB plus {} MB reserve, {} MB free", path.display(), needed >> 20, reserve >> 20, free >> 20)));
    }
    Ok(())
}

#[cfg(unix)]
fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid NUL-terminated path and a zeroed statvfs to fill
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
}

#[cfg(not(unix))]
fn free_bytes(_: &Path) -> Option<u64> {
    None
}

/// Tells macOS not to keep this file's data in memory, so a read-back really comes from the drive.
pub fn no_cache(f: &File) {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: plain fcntl on an open descriptor; failing (e.g. some network file systems) only means cached reads
        unsafe { libc::fcntl(f.as_raw_fd(), libc::F_NOCACHE, 1) };
    }
    let _ = f;
}

pub fn mxf_dir(volume: &Path) -> PathBuf {
    volume.join("Avid MediaFiles").join("MXF")
}

/// Schreibt erst in eine Nachbardatei und benennt dann um: ein Absturz hinterlässt nie eine halbe Datei.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let tmp = path.with_file_name(format!("{}.tmp", path.file_name().unwrap_or_default().to_string_lossy()));
    let mut f = File::create(&tmp)?;
    io::Write::write_all(&mut f, data)?;
    f.sync_all()?;
    fs::rename(&tmp, path)
}

pub fn hash_file(path: &Path) -> io::Result<String> {
    let mut h = blake3::Hasher::new();
    h.update_reader(File::open(path)?)?;
    Ok(h.finalize().to_hex().to_string())
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub const MANIFEST_EXT: &str = "mfexchange";

/// Zeitpunkt in Ortszeit, z. B. Format "%y%m%d" -> "261031".
pub fn local_time(secs: u64, fmt: &str) -> String {
    chrono::DateTime::from_timestamp(secs as i64, 0).map(|t| t.with_timezone(&chrono::Local).format(fmt).to_string()).unwrap_or_default()
}

/// Datum als JJMMTT für eindeutige Ordnernamen.
pub fn yymmdd(secs: u64) -> String {
    local_time(secs, "%y%m%d")
}

/// Ersetzt Zeichen, die in Datei- und Ordnernamen auf Mac oder Windows nicht erlaubt sind.
pub fn safe_name(s: &str) -> String {
    s.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_")
}

/// Dateiname eines Manifests, z. B. "261101-1312_Mac Studio_Manifest.mfexchange" (Inhalt ist JSON).
pub fn manifest_file_name(m: &Manifest) -> String {
    format!("{}_{}_Manifest.{MANIFEST_EXT}", local_time(m.created, "%y%m%d-%H%M"), safe_name(&m.system))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_and_key() {
        assert_eq!(yymmdd(1793448000), "261031");
        assert_eq!(yymmdd(951825600), "000229");
        assert_eq!(key("A\u{0308}vid8.1"), key("ävid8.1"));
    }
}
