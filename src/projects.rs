//! Avid projects: find them below a folder, read their bins and list the media files their clips need that are
//! not on the connected drives; write those as a request for MediaFiles Ferry.
use crate::avb::{Bin, MobId, MobKind, id_hex};
use crate::{PMR, folders, key, read_pmr_entries};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::fs;
use std::path::{Path, PathBuf};

fn name_of(p: &Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

fn visible_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir() && !name_of(p).starts_with('.')).collect())
        .unwrap_or_default();
    v.sort();
    v
}

fn has_avp(dir: &Path) -> bool {
    fs::read_dir(dir).map(|rd| rd.flatten().any(|e| e.file_name().to_string_lossy().to_lowercase().ends_with(".avp"))).unwrap_or(false)
}

/// Avid projects below a folder: folders with an .avp file. Inside a project no further projects are looked for
/// (shared projects keep more .avp files in AvidSharedData/<user>).
pub fn find_projects(root: &Path) -> Vec<PathBuf> {
    if has_avp(root) {
        return vec![root.to_path_buf()];
    }
    visible_dirs(root).iter().flat_map(|d| find_projects(d)).collect()
}

/// Bins of a project, without Avid's backups in "Attic".
pub fn bins(project: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let mut stack = vec![project.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let (p, n) = (e.path(), e.file_name().to_string_lossy().into_owned());
            if n.starts_with('.') || n.eq_ignore_ascii_case("Attic") {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else if n.to_lowercase().ends_with(".avb") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Media files of one drive by mob ID: from the Avid databases, counted only if the file is really in the folder;
/// MXF files that are in no database are identified by their header. Returns (ids, read from a header,
/// unidentified files). `tick` is called per folder.
/// `go` is asked between files; false ends the reading early.
fn drive_media(drive: &Path, go: &dyn Fn() -> bool) -> (HashSet<MobId>, usize, usize, HashMap<MobId, String>) {
    let mut ids = HashSet::new();
    let mut gone = HashMap::new();
    let (mut from_header, mut unknown) = (0, 0);
    for (_, dir) in folders(drive) {
        if !go() {
            break;
        }
        let on_disk: Vec<String> = fs::read_dir(&dir)
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| key(n).ends_with(".mxf") && !n.starts_with('.')).collect())
            .unwrap_or_default();
        let entries = fs::read(dir.join(PMR)).ok().and_then(|d| read_pmr_entries(&d).ok()).unwrap_or_default();
        let in_db: HashSet<String> = entries.iter().map(|e| key(&e.name)).collect();
        let present: HashSet<String> = on_disk.iter().map(|n| key(n)).collect();
        ids.extend(entries.iter().filter(|e| present.contains(&key(&e.name))).map(|e| e.id));
        gone.extend(entries.iter().filter(|e| !present.contains(&key(&e.name))).map(|e| (e.id, e.name.clone())));
        for n in on_disk.iter().filter(|n| !in_db.contains(&key(n))) {
            if !go() {
                break;
            }
            match crate::mxf::file_mob_id(&dir.join(n)) {
                Ok(id) => {
                    ids.insert(id);
                    from_header += 1;
                }
                Err(_) => unknown += 1,
            }
        }
    }
    (ids, from_header, unknown, gone)
}

pub struct Available {
    pub ids: HashSet<MobId>,
    /// MXF files in no Avid database, identified by their header
    pub from_header: usize,
    /// MXF files in no Avid database whose header could not be read
    pub unknown: usize,
    /// drives that stopped answering and were skipped
    pub skipped: Vec<PathBuf>,
    /// MXF file names of database entries whose file is gone – the only place a missing file's name is known
    pub gone: HashMap<MobId, String>,
}

/// Media files on the given drives. A drive that does not answer for `patience` (a hanging network or macFUSE
/// mount blocks a read forever) is skipped instead of stopping the scan; its reading thread is left behind. Paused
/// through `gate`, a drive is not counted as silent; cancelled: an incomplete result the caller throws away.
pub fn available_media(drives: &[PathBuf], patience: std::time::Duration, gate: &std::sync::Arc<crate::Gate>) -> Available {
    use std::sync::mpsc;
    let mut a = Available { ids: HashSet::new(), from_header: 0, unknown: 0, skipped: vec![], gone: HashMap::new() };
    for d in drives {
        if gate.cancelled() {
            break;
        }
        let (tx, rx) = mpsc::channel();
        let (drive, g) = (d.clone(), gate.clone());
        std::thread::spawn(move || {
            let tick = tx.clone();
            let result = drive_media(&drive, &move || {
                let _ = tick.send(None);
                !g.cancelled()
            });
            let _ = tx.send(Some(result));
        });
        loop {
            match rx.recv_timeout(patience) {
                Ok(None) => continue,
                Ok(Some((ids, from_header, unknown, gone))) => {
                    a.ids.extend(ids);
                    a.gone.extend(gone);
                    a.from_header += from_header;
                    a.unknown += unknown;
                    break;
                }
                Err(_) if gate.is_paused() => continue,
                Err(_) => {
                    a.skipped.push(d.clone());
                    break;
                }
            }
        }
    }
    a
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ClipKind {
    Master,
    Subclip,
    Sequence,
}

/// Maps with keys JSON cannot take (mob IDs, tuples) stored as a list of pairs (saved scans).
mod pairs {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    pub fn serialize<'a, S: Serializer, K: Serialize + 'a, V: Serialize + 'a, M>(m: &'a M, s: S) -> Result<S::Ok, S::Error>
    where
        &'a M: IntoIterator<Item = (&'a K, &'a V)>,
    {
        s.collect_seq(m)
    }
    pub fn deserialize<'de, D: Deserializer<'de>, K: Deserialize<'de>, V: Deserialize<'de>, M: FromIterator<(K, V)>>(d: D) -> Result<M, D::Error> {
        Ok(Vec::<(K, V)>::deserialize(d)?.into_iter().collect())
    }
}

/// Where a missing media file is used.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Use {
    pub project: String,
    /// bin path relative to the project
    pub bin: String,
    pub clip: String,
    pub kind: ClipKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Missing {
    pub id: MobId,
    /// the master clip the file belongs to, else the file's name in the bin, else the first clip that uses it
    pub name: String,
    /// the media file has no Avid media reference (MSML) – e.g. AAF import
    pub unmanaged: bool,
    /// another media file of the same source (camera clip / tape, picture or sound) is on the drives:
    /// the clip only lost its link, relinking in Media Composer finds it – not requested
    pub relink: bool,
    /// first clip found that uses it
    pub first: Use,
}

/// Missing media files of one bin with the same name (e.g. the video and audio files of one clip).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissingGroup {
    pub name: String,
    pub files: usize,
    /// all of them can be relinked (see `Missing::relink`)
    pub relink: bool,
    /// where in the bin they are used
    pub uses: Vec<(String, ClipKind)>,
    /// the media files themselves (`files` of them)
    pub ids: Vec<MobId>,
}

/// Saved with a scan (History), so a loaded scan shows its missing media without reading the bins again.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ScanResult {
    pub projects: usize,
    pub bins: usize,
    /// bins that could not be read: (path, reason)
    pub failed: Vec<(PathBuf, String)>,
    pub clips: usize,
    /// every missing media file once (without the relinkable ones), sorted by its first use – the request
    pub missing: Vec<Missing>,
    /// missing files whose source is on the drives in another media file (relink in Media Composer)
    pub relink: usize,
    /// AMA-linked files – left out of the request
    pub ama: usize,
    /// (project, bin) -> the AMA-linked files its clips use, grouped by name (`relink` always false)
    #[serde(with = "pairs")]
    pub ama_by_bin: BTreeMap<(String, String), Vec<MissingGroup>>,
    /// AMA-linked files whose master clip has a namesake in the project with all its media on the drives
    /// (e.g. linked first, imported later) – they don't matter, only counted
    pub ama_covered: usize,
    /// file mobs without any locator: no media was ever made for them (e.g. the camera sound tracks of a master
    /// clip whose sound was never imported – Media Composer still shows the clip online) – left out
    pub no_media: usize,
    /// (project, bin) -> its missing media files, grouped by name
    #[serde(with = "pairs")]
    pub by_bin: BTreeMap<(String, String), Vec<MissingGroup>>,
    /// missing render files (precomputed effects) – not requested, Media Composer renders them again
    pub renders: usize,
    /// missing (and AMA) file -> name of its source: camera clip, tape or imported file (`Bin::source_name`)
    #[serde(with = "pairs")]
    pub originals: HashMap<MobId, String>,
    /// missing file -> its MXF file name, if a database on the drives still lists it (set by the caller from
    /// `Available::gone`; bins don't store it)
    #[serde(with = "pairs")]
    pub mxf_names: HashMap<MobId, String>,
}

fn kind_of(kind: MobKind, usage: i32) -> Option<ClipKind> {
    match (kind, usage) {
        (MobKind::Master, 2) | (MobKind::Composition, 2) => Some(ClipKind::Subclip),
        // usage 1: master clip of a render file (bins keep them hidden)
        (MobKind::Master, 1) => None,
        (MobKind::Master, _) => Some(ClipKind::Master),
        (MobKind::Composition, 0) => Some(ClipKind::Sequence),
        _ => None,
    }
}

/// A source mob of a bin (file, tape or import source) as far as the project scan needs it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
struct SourceMob {
    id: MobId,
    /// class of the descriptor's locator ("MSML" = Avid-managed media)
    locator: Option<String>,
    ama: bool,
    /// a media file (`Mob::is_file`), not a tape or import source
    file: bool,
    name: String,
    /// its tape / camera clip / imported file and whether it is sound (`Bin::source_name`)
    source: Option<(String, bool)>,
}

/// An item of a bin (what Media Composer lists, plus hidden mobs) with the media it reaches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
struct ItemSum {
    id: MobId,
    name: String,
    /// master clip, subclip or sequence (`kind_of`); None for render masters and other mobs
    clip: Option<ClipKind>,
    /// master clip of a render file (usage 1)
    render: bool,
    /// a composition (its media are not looked up from other bins)
    composition: bool,
    /// `Bin::media_of`: media files, and mobs it uses that are stored in another bin
    media: Vec<MobId>,
    unresolved: Vec<MobId>,
    /// `Bin::renders_of` likewise (only for clips)
    renders: Vec<MobId>,
    renders_unresolved: Vec<MobId>,
}

/// Everything the project scans take from one bin – read once, kept in the bin cache while the bin is unchanged.
#[derive(Debug, Clone, Default)]
pub struct BinSummary {
    // shared: the same file mobs and clips are copied into many bins (a test project: 6 million items, most of
    // them copies – unshared they took 9 GB of memory)
    sources: Vec<Arc<SourceMob>>,
    items: Vec<Arc<ItemSum>>,
}

/// Hands out one shared copy of each equal value.
#[derive(Default)]
struct Pool {
    sources: HashSet<Arc<SourceMob>>,
    items: HashSet<Arc<ItemSum>>,
}

impl Pool {
    fn source(&mut self, m: SourceMob) -> Arc<SourceMob> {
        if let Some(a) = self.sources.get(&m) {
            return a.clone();
        }
        let a = Arc::new(m);
        self.sources.insert(a.clone());
        a
    }
    fn item(&mut self, it: ItemSum) -> Arc<ItemSum> {
        if let Some(a) = self.items.get(&it) {
            return a.clone();
        }
        let a = Arc::new(it);
        self.items.insert(a.clone());
        a
    }
}

impl BinSummary {
    fn of(bin: &Bin, pool: &mut Pool) -> BinSummary {
        let sources = bin
            .mobs
            .iter()
            .filter(|m| m.kind == MobKind::Source)
            .map(|m| SourceMob {
                id: m.id,
                locator: m.locator.clone(),
                ama: m.ama,
                file: m.is_file(),
                name: m.name.clone(),
                source: if m.is_file() { bin.source_name(m).map(|n| (n, matches!(m.descriptor.as_deref(), Some("PCMA" | "WAVE" | "AIFC")))) } else { None },
            })
            .map(|m| pool.source(m))
            .collect();
        let items = bin
            .items
            .iter()
            .map(|&item| {
                let mob = &bin.mobs[item];
                let clip = kind_of(mob.kind, mob.usage);
                let (mut unresolved, mut renders_unresolved) = (HashSet::new(), HashSet::new());
                let media = bin.media_of(item, &mut unresolved);
                let renders = if clip.is_some() { bin.renders_of(item, &mut renders_unresolved) } else { vec![] };
                ItemSum {
                    id: mob.id,
                    name: mob.name.clone(),
                    clip,
                    render: mob.kind == MobKind::Master && mob.usage == 1,
                    composition: mob.kind == MobKind::Composition,
                    media,
                    unresolved: sorted(unresolved),
                    renders,
                    renders_unresolved: sorted(renders_unresolved),
                }
            })
            .map(|it| pool.item(it))
            .collect();
        BinSummary { sources, items }
    }
}

fn sorted(set: HashSet<MobId>) -> Vec<MobId> {
    let mut v: Vec<MobId> = set.into_iter().collect();
    v.sort();
    v
}

/// What one bin contributes to `Refs`: with `uses`, the media and renders of its clips, subclips and sequences (mobs
/// from other bins go to `waiting`); always its clips' media into `media_of`, for clips used from other bins.
fn use_bin(bin: &BinSummary, r: &mut Refs, media_of: &mut HashMap<MobId, Vec<MobId>>, waiting: &mut Vec<(MobId, bool)>, uses: bool) {
    // only Avid-managed media (AMA-linked and other linked files are not in Avid MediaFiles)
    let src: HashMap<MobId, &SourceMob> = bin.sources.iter().map(|s| (s.id, s.as_ref())).collect();
    let managed = |ids: &[MobId]| -> Vec<MobId> { ids.iter().copied().filter(|id| src.get(id).is_none_or(|m| !m.ama && m.locator.as_deref().is_none_or(|l| l == "MSML"))).collect() };
    for item in &bin.items {
        let media = managed(&item.media);
        if !item.composition {
            media_of.insert(item.id, media.clone());
        }
        // a render's own master clip is no use of it – only an effect that still points to it is
        if !uses || item.clip.is_none() {
            continue;
        }
        r.renders.extend(managed(&item.renders));
        r.media.extend(media);
        waiting.extend(item.unresolved.iter().map(|id| (*id, false)));
        waiting.extend(item.renders_unresolved.iter().map(|id| (*id, true)));
    }
}

/// Media and renders one bin uses; clips it takes from other bins of its project are looked up there.
pub fn bin_refs(path: &Path) -> Result<Refs, String> {
    let mut pool = Pool::default();
    let bin = BinSummary::of(&Bin::read(path)?, &mut pool);
    let mut r = Refs { bins: 1, ..Default::default() };
    let (mut media_of, mut waiting) = (HashMap::new(), vec![]);
    use_bin(&bin, &mut r, &mut media_of, &mut waiting, true);
    if !waiting.is_empty()
        && let Some(project) = path.ancestors().skip(1).find(|d| has_avp(d))
    {
        for other in bins(project).iter().filter(|b| b.as_path() != path) {
            match Bin::read(other) {
                Ok(b) => use_bin(&BinSummary::of(&b, &mut pool), &mut Refs::default(), &mut media_of, &mut vec![], false),
                Err(e) => r.failed.push((other.clone(), e)),
            }
        }
    }
    for (mob, render) in waiting {
        let set = if render { &mut r.renders } else { &mut r.media };
        set.extend(media_of.get(&mob).into_iter().flatten());
    }
    Ok(r)
}

/// The media files the bins of some projects use: `media` through their clips, subclips and sequences, `renders`
/// through the effects of those (a render whose effect is gone from every sequence is in neither).
#[derive(Debug, Default)]
pub struct Refs {
    pub media: HashSet<MobId>,
    pub renders: HashSet<MobId>,
    pub bins: usize,
    /// bins that could not be read: (bin, error) – their media count as unused, so the caller must say so
    pub failed: Vec<(PathBuf, String)>,
}

/// Raised when `BinSummary` or the file layout changes: an older cache is read anew.
const BIN_CACHE_VERSION: u32 = 5;

/// Bin path -> (size, modification date, summary). Media Composer rewrites the whole bin file when it saves it, so a
/// bin with the same size and date has not changed and is not read again.
#[derive(Default)]
struct BinCache {
    bins: HashMap<String, (u64, u128, BinSummary)>,
    pool: Pool,
}

type DiskSource = (u32, Option<String>, bool, bool, String, Option<(String, bool)>);
/// id, name, flags (clip kind 1 master, 2 subclip, 3 sequence; +4 render master, +8 composition), and as indexes
/// into `lists`: media, unresolved, renders, renders unresolved
type DiskItem = (u32, String, u8, u32, u32, u32, u32);

/// On disk everything is stored once and referred to by index: mob IDs (`ids`, hex), lists of them, source mobs and
/// items – the same clips, file mobs and sequences are copied into many bins (a test project with 1845 bins:
/// 6 million items, most of them copies).
#[derive(Serialize, Deserialize, Default)]
struct BinCacheFile {
    version: u32,
    ids: Vec<String>,
    lists: Vec<Vec<u32>>,
    sources: Vec<DiskSource>,
    items: Vec<DiskItem>,
    /// path, size, date, its sources and its items (indexes into `sources` / `items`)
    bins: Vec<(String, u64, u128, Vec<u32>, Vec<u32>)>,
}

fn from_hex(t: &str) -> MobId {
    let bytes: Vec<u8> = (0..t.len()).step_by(2).filter_map(|i| t.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok())).collect();
    bytes.try_into().unwrap_or([0; 32])
}

/// Hands out the index of a value, storing it on first sight.
struct Table<T: std::hash::Hash + Eq + Clone> {
    values: Vec<T>,
    index: HashMap<T, u32>,
}

impl<T: std::hash::Hash + Eq + Clone> Table<T> {
    fn new() -> Self {
        Table { values: vec![], index: HashMap::new() }
    }
    fn at(&mut self, v: T) -> u32 {
        if let Some(i) = self.index.get(&v) {
            return *i;
        }
        self.values.push(v.clone());
        self.index.insert(v, self.values.len() as u32 - 1);
        self.values.len() as u32 - 1
    }
}

impl BinCache {
    fn load(path: &Path) -> BinCache {
        let Some(f) = fs::read(path).ok().and_then(|b| serde_json::from_slice::<BinCacheFile>(&b).ok()).filter(|f| f.version == BIN_CACHE_VERSION) else { return BinCache::default() };
        let ids: Vec<MobId> = f.ids.iter().map(|h| from_hex(h)).collect();
        let id = |i: &u32| ids.get(*i as usize).copied().unwrap_or([0; 32]);
        let list = |i: &u32| f.lists.get(*i as usize).map(|v| v.iter().map(id).collect::<Vec<_>>()).unwrap_or_default();
        let sources: Vec<Arc<SourceMob>> = f.sources.iter().map(|(i, locator, ama, file, name, source)| Arc::new(SourceMob { id: id(i), locator: locator.clone(), ama: *ama, file: *file, name: name.clone(), source: source.clone() })).collect();
        let items: Vec<Arc<ItemSum>> = f
            .items
            .iter()
            .map(|(i, name, flags, media, unresolved, renders, renders_unresolved)| ItemSum {
                id: id(i),
                name: name.clone(),
                clip: match flags & 3 {
                    1 => Some(ClipKind::Master),
                    2 => Some(ClipKind::Subclip),
                    3 => Some(ClipKind::Sequence),
                    _ => None,
                },
                render: flags & 4 != 0,
                composition: flags & 8 != 0,
                media: list(media),
                unresolved: list(unresolved),
                renders: list(renders),
                renders_unresolved: list(renders_unresolved),
            })
            .map(Arc::new)
            .collect();
        let bins = f
            .bins
            .into_iter()
            .map(|(p, size, date, s, it)| {
                let summary = BinSummary {
                    sources: s.iter().filter_map(|i| sources.get(*i as usize).cloned()).collect(),
                    items: it.iter().filter_map(|i| items.get(*i as usize).cloned()).collect(),
                };
                (p, (size, date, summary))
            })
            .collect();
        let pool = Pool { sources: sources.into_iter().collect(), items: items.into_iter().collect() };
        BinCache { bins, pool }
    }

    fn save(&self, path: &Path) {
        let (mut ids, mut lists, mut sources, mut items) = (Table::new(), Table::new(), Table::new(), Table::new());
        let mut list = |v: &[MobId], ids: &mut Table<MobId>| -> u32 { lists.at(v.iter().map(|m| ids.at(*m)).collect::<Vec<u32>>()) };
        let mut bins = vec![];
        for (p, (size, date, s)) in &self.bins {
            let src: Vec<u32> = s.sources.iter().map(|m| sources.at((ids.at(m.id), m.locator.clone(), m.ama, m.file, m.name.clone(), m.source.clone()))).collect();
            let its: Vec<u32> = s
                .items
                .iter()
                .map(|it| {
                    let kind = match it.clip {
                        Some(ClipKind::Master) => 1,
                        Some(ClipKind::Subclip) => 2,
                        Some(ClipKind::Sequence) => 3,
                        None => 0,
                    };
                    let flags = kind | if it.render { 4 } else { 0 } | if it.composition { 8 } else { 0 };
                    let entry = (ids.at(it.id), it.name.clone(), flags, list(&it.media, &mut ids), list(&it.unresolved, &mut ids), list(&it.renders, &mut ids), list(&it.renders_unresolved, &mut ids));
                    items.at(entry)
                })
                .collect();
            bins.push((p.clone(), *size, *date, src, its));
        }
        let file = BinCacheFile { version: BIN_CACHE_VERSION, ids: ids.values.iter().map(id_hex).collect(), lists: lists.values, sources: sources.values, items: items.values, bins };
        if let Ok(data) = serde_json::to_vec(&file) {
            let tmp = path.with_extension("tmp");
            if fs::write(&tmp, data).is_ok() {
                let _ = fs::rename(tmp, path);
            }
        }
    }
}

fn stamp(path: &Path) -> Option<(u64, u128)> {
    let m = fs::metadata(path).ok()?;
    Some((m.len(), m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos()))
}

/// The bins of some projects, read (or taken from the cache) once for `references_of` and `scan_of`:
/// per project its folder and its bins with their summary or why they could not be read.
pub type ProjectBins = Vec<(PathBuf, Vec<(PathBuf, Result<BinSummary, String>)>)>;

/// Reads the bins of the projects. `cache` (a JSON file) keeps every bin's summary: a bin with the same size and
/// date is not read again. progress gets (bins done, bins total, project/bin). Cancelled through `gate`: the bins
/// not reached are left out (the caller throws the result away).
pub fn read_projects(projects: &[PathBuf], cache: Option<&Path>, progress: &mut dyn FnMut(usize, usize, &str), gate: &crate::Gate) -> ProjectBins {
    let mut bc = cache.map(BinCache::load).unwrap_or_default();
    let lists: Vec<Vec<PathBuf>> = projects.iter().map(|p| bins(p)).collect();
    let total = lists.iter().map(Vec::len).sum();
    let (mut done, mut changed) = (0, false);
    let mut out = vec![];
    for (project, list) in projects.iter().zip(lists) {
        let mut read = vec![];
        for path in list {
            if gate.cancelled() {
                break;
            }
            let rel = path.strip_prefix(project).map(|p| p.display().to_string()).unwrap_or_else(|_| name_of(&path));
            progress(done, total, &format!("{}/{rel}", name_of(project)));
            done += 1;
            let key = path.to_string_lossy().into_owned();
            let st = stamp(&path);
            let summary = match (st, bc.bins.get(&key)) {
                (Some((size, date)), Some((s, d, sum))) if size == *s && date == *d => Ok(sum.clone()),
                _ => Bin::read(&path).map(|b| BinSummary::of(&b, &mut bc.pool)),
            };
            if let (Some((size, date)), Ok(s)) = (st, &summary)
                && bc.bins.get(&key).is_none_or(|(s0, d0, _)| (*s0, *d0) != (size, date))
            {
                bc.bins.insert(key, (size, date, s.clone()));
                changed = true;
            }
            read.push((path, summary));
        }
        out.push((project.clone(), read));
    }
    if let Some(path) = cache {
        // bins that are gone (deleted, or a project folder that moved) would only make the file grow
        let before = bc.bins.len();
        bc.bins.retain(|p, _| Path::new(p).exists());
        // writing it costs memory and seconds: only when a bin changed
        if changed || bc.bins.len() != before || !path.exists() {
            bc.save(path);
        }
    }
    out
}

/// Every bin of the projects (see `Refs`). Clips stored in another bin of the same project are resolved over the
/// project.
pub fn references_of(projects: &ProjectBins) -> Refs {
    let mut r = Refs { bins: projects.iter().map(|p| p.1.len()).sum(), ..Default::default() };
    for (_, list) in projects {
        // per project: mob -> its media files; mobs from another bin, waiting for the end of the project (render?)
        let (mut media_of, mut waiting) = (HashMap::new(), vec![]);
        for (path, bin) in list {
            match bin {
                Ok(b) => use_bin(b, &mut r, &mut media_of, &mut waiting, true),
                Err(e) => r.failed.push((path.clone(), e.clone())),
            }
        }
        for (mob, render) in waiting {
            let set = if render { &mut r.renders } else { &mut r.media };
            set.extend(media_of.get(&mob).into_iter().flatten());
        }
    }
    r
}

/// `read_projects` + `references_of`.
pub fn references(projects: &[PathBuf], cache: Option<&Path>, progress: &mut dyn FnMut(usize, usize, &str), gate: &crate::Gate) -> Refs {
    references_of(&read_projects(projects, cache, progress, gate))
}

/// Reads every bin of the projects and collects the media files their clips, subclips and sequences need that are
/// not in `available` (`read_projects` + `scan_of`).
pub fn scan(projects: &[PathBuf], available: &HashSet<MobId>, progress: &mut dyn FnMut(usize, usize, &str), gate: &crate::Gate) -> ScanResult {
    scan_of(&read_projects(projects, None, progress, gate), available)
}

/// The media files the clips, subclips and sequences of the bins need that are not in `available`. References to
/// clips stored in another bin of the same project are resolved over the project.
pub fn scan_of(projects: &ProjectBins, available: &HashSet<MobId>) -> ScanResult {
    let mut r = ScanResult { projects: projects.len(), bins: projects.iter().map(|p| p.1.len()).sum(), ..Default::default() };
    let mut missing: HashMap<MobId, Missing> = HashMap::new();
    let mut by_bin: BTreeMap<(String, String), BTreeMap<MobId, BTreeSet<(String, ClipKind)>>> = BTreeMap::new();
    // missing file mobs: locator class, name, AMA, source; and the sources of the files that are on the drives
    let mut files: HashMap<MobId, (Option<String>, String, bool, Option<(String, bool)>)> = HashMap::new();
    let mut on_drives: HashSet<(String, bool)> = HashSet::new();
    let mut renders: HashSet<MobId> = HashSet::new();
    // missing file -> the master clip it belongs to
    let mut owners: HashMap<MobId, String> = HashMap::new();
    // (project, key of the name) of master clips whose media are all on the drives
    let mut online_masters: HashSet<(String, String)> = HashSet::new();

    let mut record = |id: MobId, u: Use, missing: &mut HashMap<MobId, Missing>| {
        if available.contains(&id) {
            return;
        }
        by_bin.entry((u.project.clone(), u.bin.clone())).or_default().entry(id).or_default().insert((u.clip.clone(), u.kind));
        if u.kind == ClipKind::Master {
            owners.entry(id).or_insert_with(|| u.clip.clone());
        }
        missing.entry(id).or_insert_with(|| Missing { id, name: String::new(), unmanaged: false, relink: false, first: u });
    };

    for (project, list) in projects {
        // per project: master/subclip mob -> media files, and uses still waiting for a mob from another bin
        let mut media_of: HashMap<MobId, Vec<MobId>> = HashMap::new();
        let mut waiting: Vec<(MobId, Use)> = vec![];
        for (path, bin) in list {
            let bin = match bin {
                Ok(b) => b,
                Err(e) => {
                    r.failed.push((path.clone(), e.clone()));
                    continue;
                }
            };
            let rel = path.strip_prefix(project).map(|p| p.display().to_string()).unwrap_or_else(|_| name_of(path));
            for m in bin.sources.iter().filter(|m| m.file) {
                if available.contains(&m.id) {
                    on_drives.extend(m.source.clone());
                } else {
                    files.entry(m.id).or_insert_with(|| (m.locator.clone(), m.name.clone(), m.ama, m.source.clone()));
                }
            }
            for item in &bin.items {
                if item.render {
                    renders.extend(item.media.iter().filter(|id| !available.contains(*id)));
                    continue;
                }
                let Some(kind) = item.clip else { continue };
                r.clips += 1;
                let u = Use { project: name_of(project), bin: rel.clone(), clip: item.name.clone(), kind };
                if kind == ClipKind::Master && !item.media.is_empty() && item.unresolved.is_empty() && item.media.iter().all(|id| available.contains(id)) {
                    online_masters.insert((u.project.clone(), crate::key(&item.name)));
                }
                if kind != ClipKind::Sequence {
                    media_of.insert(item.id, item.media.clone());
                }
                for id in &item.media {
                    record(*id, u.clone(), &mut missing);
                }
                waiting.extend(item.unresolved.iter().map(|id| (*id, u.clone())));
            }
        }
        for (mob, u) in waiting {
            for id in media_of.get(&mob).into_iter().flatten() {
                record(*id, u.clone(), &mut missing);
            }
        }
    }
    drop(record);

    // named after its master clip (file mobs are often called just "File"), else the file mob, else the first use.
    // shortcut: relink matches source name and picture/sound only, not the timecode range – a newer file that
    // covers less of the source still counts; compare timecodes if that shows up in practice.
    renders.retain(|id| !missing.contains_key(id));
    let is_ama = |id: &MobId| files.get(id).is_some_and(|f| f.2);
    let mut ama_names: HashMap<MobId, String> = missing
        .iter()
        .filter(|(id, _)| is_ama(id))
        .map(|(id, m)| (*id, owners.get(id).cloned().or_else(|| files.get(id).map(|f| f.1.clone()).filter(|n| !n.trim().is_empty())).unwrap_or_else(|| m.first.clip.clone())))
        .collect();
    let before = ama_names.len();
    ama_names.retain(|id, name| !online_masters.contains(&(missing[id].first.project.clone(), crate::key(name))));
    r.ama_covered = before - ama_names.len();
    r.originals = missing.keys().filter_map(|id| Some((*id, files.get(id)?.3.as_ref()?.0.clone()))).collect();
    r.ama = ama_names.len();
    missing.retain(|id, _| !is_ama(id));
    let no_locator = |id: &MobId| files.get(id).is_some_and(|f| f.0.is_none());
    r.no_media = missing.keys().filter(|id| no_locator(id)).count();
    missing.retain(|id, _| !no_locator(id));
    for m in missing.values_mut() {
        let (locator, name, _, src) = files.remove(&m.id).unwrap_or((Some("MSML".into()), String::new(), false, None));
        m.unmanaged = locator.as_deref() != Some("MSML");
        m.relink = src.is_some_and(|s| on_drives.contains(&s));
        m.name = owners.remove(&m.id).or(Some(name).filter(|n| !n.trim().is_empty())).unwrap_or_else(|| m.first.clip.clone());
    }
    r.renders = renders.len();
    // per bin: the files `info` knows (name, relink), grouped by name with all their uses
    let per_bin = |info: &dyn Fn(&MobId) -> Option<(String, bool)>| -> BTreeMap<(String, String), Vec<MissingGroup>> {
        by_bin
            .iter()
            .map(|(k, ids)| {
                let mut groups: BTreeMap<(String, bool), (Vec<MobId>, BTreeSet<(String, ClipKind)>)> = BTreeMap::new();
                for (id, uses) in ids {
                    let Some(key) = info(id) else { continue };
                    let g = groups.entry(key).or_default();
                    g.0.push(*id);
                    g.1.extend(uses.iter().cloned());
                }
                let groups: Vec<MissingGroup> = groups.into_iter().map(|((name, relink), (ids, uses))| MissingGroup { name, files: ids.len(), relink, uses: uses.into_iter().collect(), ids }).collect();
                (k.clone(), groups)
            })
            .filter(|(_, g)| !g.is_empty())
            .collect()
    };
    r.by_bin = per_bin(&|id| missing.get(id).map(|m| (m.name.clone(), m.relink)));
    r.ama_by_bin = per_bin(&|id| ama_names.get(id).map(|n| (n.clone(), false)));
    r.relink = missing.values().filter(|m| m.relink).count();
    let mut list: Vec<Missing> = missing.into_values().filter(|m| !m.relink).collect();
    list.sort_by(|a, b| (&a.first.project, &a.first.bin, &a.first.clip, a.id).cmp(&(&b.first.project, &b.first.bin, &b.first.clip, b.id)));
    r.missing = list;
    r
}

/// Request for MediaFiles Ferry (format "mediafiles-ferry-request", see Ferry's src/request.rs).
#[derive(Serialize)]
struct Request<'a> {
    format: &'a str,
    version: u32,
    system: &'a str,
    created: u64,
    files: Vec<Wanted<'a>>,
}

#[derive(Serialize)]
struct Wanted<'a> {
    id: String,
    clip: &'a str,
    bin: &'a str,
    project: &'a str,
}

pub fn request_json(system: &str, created: u64, missing: &[Missing]) -> Vec<u8> {
    let files = missing
        .iter()
        .map(|m| Wanted { id: id_hex(&m.id), clip: &m.first.clip, bin: &m.first.bin, project: &m.first.project })
        .collect();
    serde_json::to_vec(&Request { format: "mediafiles-ferry-request", version: 1, system, created, files }).unwrap_or_default()
}

/// The missing media as text: per project and bin the clips and their missing files (ids).
pub fn list_text(r: &ScanResult, lang: &str) -> String {
    let t = |texts: [&str; 4]| crate::pick(lang, texts).to_string();
    let mut out = t([
        "MediaFiles Check – {n} media files missing ({p} projects, {b} bins, {c} clips checked)",
        "MediaFiles Check – {n} Mediendateien fehlen ({p} Projekte, {b} Bins, {c} Clips geprüft)",
        "MediaFiles Check – {n} fichiers médias manquants ({p} projets, {b} chutiers, {c} clips vérifiés)",
        "MediaFiles Check – faltan {n} archivos de medios ({p} proyectos, {b} bins, {c} clips comprobados)",
    ])
    .replace("{n}", &r.missing.len().to_string())
    .replace("{p}", &r.projects.to_string())
    .replace("{b}", &r.bins.to_string())
    .replace("{c}", &r.clips.to_string())
        + "\n";
    let kind = |k: &ClipKind| match k {
        ClipKind::Master => String::new(),
        ClipKind::Subclip => t([" (subclip)", " (Subclip)", " (sous-clip)", " (subclip)"]),
        ClipKind::Sequence => t([" (sequence)", " (Sequenz)", " (séquence)", " (secuencia)"]),
    };
    let bins = |out: &mut String, by_bin: &BTreeMap<(String, String), Vec<MissingGroup>>| {
        for ((project, bin), groups) in by_bin {
            out.push_str(&format!("\n{project} / {bin}\n"));
            for g in groups {
                let files = if g.files > 1 { format!(" ({})", t(["{} files", "{} Dateien", "{} fichiers", "{} archivos"]).replace("{}", &g.files.to_string())) } else { String::new() };
                let relink = if g.relink { t([" – on the drives, relink", " – auf den Laufwerken, neu verlinken", " – sur les disques, à relier", " – en los discos, revincular"]) } else { String::new() };
                out.push_str(&format!("  {}{files}{relink}\n", g.name));
                for (clip, k) in &g.uses {
                    out.push_str(&format!("      {clip}{}\n", kind(k)));
                }
            }
        }
    };
    bins(&mut out, &r.by_bin);
    if !r.ama_by_bin.is_empty() {
        out.push_str(&t(["\n\nAMA-linked clips per bin:\n", "\n\nAMA-verlinkte Clips pro Bin:\n", "\n\nClips liés par AMA par chutier :\n", "\n\nClips vinculados por AMA por bin:\n"]));
        bins(&mut out, &r.ama_by_bin);
    }
    if r.renders > 0 {
        out.push_str(&t([
            "\n{} render files are missing too – not requested, Media Composer renders them again.\n",
            "\nAußerdem fehlen {} Render-Dateien – nicht angefordert, Media Composer rendert sie neu.\n",
            "\n{} fichiers de rendu manquent aussi – non demandés, Media Composer les recalcule.\n",
            "\nTambién faltan {} archivos de render: no se solicitan, Media Composer los vuelve a renderizar.\n",
        ]).replace("{}", &r.renders.to_string()));
    }
    if r.relink > 0 {
        out.push_str(&t([
            "\n{} missing files are on the drives as another media file of the same source – relink them in Media Composer; not requested.\n",
            "\n{} fehlende Dateien liegen als andere Mediendatei derselben Quelle auf den Laufwerken – in Media Composer neu verlinken; nicht angefordert.\n",
            "\n{} fichiers manquants sont sur les disques sous un autre fichier média de la même source – reliez-les dans Media Composer ; non demandés.\n",
            "\n{} archivos que faltan están en los discos como otro archivo de medios de la misma fuente: revincúlalos en Media Composer; no se solicitan.\n",
        ]).replace("{}", &r.relink.to_string()));
    }
    if r.ama > 0 {
        out.push_str(&t([
            "\n{} AMA-linked files left out.\n",
            "\n{} AMA-verlinkte Dateien ausgelassen.\n",
            "\n{} fichiers liés par AMA ignorés.\n",
            "\n{} archivos vinculados por AMA omitidos.\n",
        ]).replace("{}", &r.ama.to_string()));
    }
    if r.ama_covered > 0 {
        out.push_str(&t([
            "\n{} more AMA-linked files have a master clip of the same name with its media on the drives – left out.\n",
            "\n{} weitere AMA-verlinkte Dateien haben einen gleichnamigen Masterclip mit Medien auf den Laufwerken – ausgelassen.\n",
            "\n{} autres fichiers liés par AMA ont un clip master du même nom avec ses médias sur les disques – ignorés.\n",
            "\n{} archivos vinculados por AMA más tienen un clip master con el mismo nombre y sus medios en los discos: omitidos.\n",
        ]).replace("{}", &r.ama_covered.to_string()));
    }
    if r.no_media > 0 {
        out.push_str(&t([
            "\n{} references without media left out (e.g. camera sound that was never imported).\n",
            "\n{} Verweise ohne Mediendatei ausgelassen (z. B. nie importierter Kameraton).\n",
            "\n{} références sans fichier média ignorées (p. ex. son caméra jamais importé).\n",
            "\n{} referencias sin archivo de medios omitidas (p. ej. sonido de cámara nunca importado).\n",
        ]).replace("{}", &r.no_media.to_string()));
    }
    let unmanaged = r.missing.iter().filter(|m| m.unmanaged).count();
    if unmanaged > 0 {
        out.push_str(&t([
            "\n{} of the missing files have no Avid media reference – e.g. imported via AAF.\n",
            "\n{} der fehlenden Dateien haben keinen Avid-Medienverweis – z. B. über AAF importiert.\n",
            "\n{} des fichiers manquants n'ont pas de référence média Avid – p. ex. importés par AAF.\n",
            "\n{} de los archivos que faltan no tienen referencia de medios de Avid: p. ej. importados por AAF.\n",
        ]).replace("{}", &unmanaged.to_string()));
    }
    for (p, e) in &r.failed {
        out.push_str(&format!("\n! {}: {e}", p.display()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_are_folders_with_an_avp_and_bins_skip_the_attic() {
        let root = std::env::temp_dir().join(format!("mfc-projects-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for d in ["Avid Projects/A/AvidSharedData/user", "Avid Projects/A/Bins/Attic", "Avid Projects/Archiv/B", "Avid Projects/C"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        for f in ["Avid Projects/A/A.avp", "Avid Projects/A/AvidSharedData/user/A.avp", "Avid Projects/Archiv/B/B.avp", "Avid Projects/A/Reel 1.avb", "Avid Projects/A/Bins/Cuts.avb", "Avid Projects/A/Bins/Attic/Cuts.avb.1"] {
            fs::write(root.join(f), b"").unwrap();
        }
        let projects: Vec<String> = find_projects(&root.join("Avid Projects")).iter().map(|p| name_of(p)).collect();
        assert_eq!(projects, ["A", "B"]); // C has no .avp, AvidSharedData is inside A
        let b: Vec<String> = bins(&root.join("Avid Projects/A")).iter().map(|p| name_of(p)).collect();
        assert_eq!(b, ["Cuts.avb", "Reel 1.avb"]);
        // unreadable bins are reported, not fatal
        let r = scan(&[root.join("Avid Projects/A")], &HashSet::new(), &mut |_, _, _| {}, &crate::Gate::default());
        assert_eq!((r.bins, r.failed.len(), r.missing.len()), (2, 2, 0));
        fs::remove_dir_all(&root).unwrap();
    }
}
