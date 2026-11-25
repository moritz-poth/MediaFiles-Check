//! Fehlende Dateien in einen Lieferordner sammeln, Lieferungen prüfen und einspielen.
use crate::exchange::plan::{Plan, Task};
use crate::exchange::*;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{Read, Write};

pub const DELIVERY_FORMAT: &str = "mediafiles-exchange-delivery";
pub const DELIVERY_FILE: &str = "exchange-delivery.json";

/// Aufgabendatei der Lieferung, in der Sprache des Absenders
pub fn tasks_file(lang: &str) -> &'static str {
    if lang == "de" { "Aufgaben.txt" } else { "Tasks.txt" }
}
pub const BACKUP_DIR: &str = "Exchange-Backup";

#[derive(Serialize, Deserialize)]
pub struct Delivery {
    pub format: String,
    pub from: String,
    pub to: String,
    pub created: u64,
    pub files: Vec<Delivered>,
    /// Ordner, deren Datenbank mitgeliefert wird: "Volume/Ordner" -> MXF-Dateien, die danach genau dort liegen müssen
    #[serde(default)]
    pub db_folders: BTreeMap<String, Vec<String>>,
    pub tasks: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Delivered {
    /// relativ zum Lieferordner: Volume/Ordner/Datei
    pub path: String,
    pub size: u64,
    pub hash: String,
}

fn is_db(name: &str) -> bool {
    DB_FILES.iter().any(|d| d.eq_ignore_ascii_case(name))
}

/// Kopiert alle Dateien des Plans nach `dest/<Volume>/<Ordner>/`. Bricht bei einzelnen Fehlern nicht ab, sondern gibt sie zurück.
pub fn collect(local: &Manifest, to_system: &str, plan: &Plan, dest: &Path, lang: &str, progress: &mut dyn FnMut(&str)) -> io::Result<Vec<String>> {
    let mut files = vec![];
    let mut errors = vec![];
    let mut dbs: BTreeMap<String, (usize, Vec<String>)> = BTreeMap::new();
    let target = |c: &crate::exchange::plan::Copy| dest.join(&c.to_volume).join(&c.to_folder).join(&c.name);
    check_space(dest, plan.copies.iter().filter(|c| !target(c).exists()).map(|c| c.size).sum())?;
    let mut tasks = plan.tasks.clone();
    for (i, c) in plan.copies.iter().enumerate() {
        let rel = format!("{}/{}/{}", c.to_volume, c.to_folder, c.name);
        progress(&format!("[{}/{}] {rel}", i + 1, plan.copies.len()));
        let Some(vol) = local.volumes.iter().find(|v| v.name == c.from_volume) else {
            errors.push(format!("{rel}: volume {} missing in manifest", c.from_volume));
            continue;
        };
        let src = mxf_dir(Path::new(&vol.path)).join(&c.from_folder).join(&c.name);
        let dst = target(c);
        // a database describes its folder as it is now: only while that still is the scanned folder does it fit the delivery
        if c.hash.is_none() {
            let scanned = vol.folders.iter().find(|f| f.name == c.from_folder).map(|f| f.files.iter().map(|e| key(&e.name)).collect());
            if scanned.is_none() || mxf_names(src.parent().unwrap_or(&src)).ok() != scanned {
                let t = Task::RebuildDb { system: to_system.into(), volume: c.to_volume.clone(), folder: c.to_folder.clone() };
                if !tasks.contains(&t) {
                    tasks.push(t);
                }
                continue;
            }
        }
        match copy_verified(&src, &dst, c.hash.as_deref()) {
            Ok((size, hash)) => {
                files.push(Delivered { path: rel, size, hash });
                if c.hash.is_none() {
                    let source = vol.folders.iter().find(|f| f.name == c.from_folder);
                    let entry = dbs.entry(format!("{}/{}", c.to_volume, c.to_folder)).or_default();
                    entry.0 += 1;
                    entry.1 = source.map(|f| f.files.iter().map(|e| e.name.clone()).collect()).unwrap_or_default();
                }
            }
            Err(e) => errors.push(format!("{rel}: {e}")),
        }
    }
    // nur vollständige Datenbanken (beide Dateien) dürfen beim Einspielen ersetzt werden
    let db_folders: BTreeMap<String, Vec<String>> = dbs.into_iter().filter(|(_, (n, _))| *n == DB_FILES.len()).map(|(k, (_, f))| (k, f)).collect();

    fs::create_dir_all(dest)?;
    let tasks: Vec<String> = tasks.iter().map(|t| t.describe(lang)).collect();
    let t = |a: [&'static str; 4]| crate::pick(lang, a);
    let mut text = t([
        "MediaFiles Check – delivery from {from} for {to}\n\n\
         IMPORT WITH MEDIAFILES CHECK: Exchange › Import (quit Media Composer first).\n\n\
         IMPORT BY HAND:\n\
         1. Quit Media Composer.\n\
         2. Every folder of this delivery is named <volume>/<folder>. On that volume open\n   \
            \"Avid MediaFiles/MXF/<folder>\" (create it if it is missing).\n\
         3. Copy ONLY THE CONTENTS of the delivery folder into it – NEVER drag the whole folder onto an\n   \
            existing folder! Finder and Explorer would replace it including all media files.\n\
         4. If you are asked to replace .mxf files: DON'T – cancel and check.\n\
         5. Replace msmFMID.pmr and msmMMOB.mdb only in these folders:\n",
        "MediaFiles Check – Lieferung von {from} für {to}\n\n\
         IMPORTIEREN MIT MEDIAFILES CHECK: Austausch › Importieren (Media Composer vorher schließen).\n\n\
         VON HAND IMPORTIEREN:\n\
         1. Media Composer schließen.\n\
         2. Jeder Ordner dieser Lieferung heißt <Volume>/<Ordner>. Auf dem Volume den Ordner\n   \
            \"Avid MediaFiles/MXF/<Ordner>\" öffnen (fehlt er, neu anlegen).\n\
         3. NUR DEN INHALT des Lieferordners hineinkopieren – NIEMALS den ganzen Ordner auf einen\n   \
            bestehenden Ordner ziehen! Finder und Explorer ersetzen ihn sonst samt allen Mediendateien.\n\
         4. Fragt das System, ob .mxf-Dateien ersetzt werden sollen: NICHT ersetzen, abbrechen und prüfen.\n\
         5. msmFMID.pmr und msmMMOB.mdb nur in diesen Ordnern ersetzen:\n",
        "MediaFiles Check – livraison de {from} pour {to}\n\n\
         IMPORTER AVEC MEDIAFILES CHECK : Échange › Importer (quittez d’abord Media Composer).\n\n\
         IMPORTER À LA MAIN :\n\
         1. Quittez Media Composer.\n\
         2. Chaque dossier de cette livraison s’appelle <volume>/<dossier>. Sur ce volume, ouvrez\n   \
            « Avid MediaFiles/MXF/<dossier> » (créez-le s’il manque).\n\
         3. Copiez SEULEMENT LE CONTENU du dossier de livraison – ne glissez JAMAIS le dossier entier sur un\n   \
            dossier existant ! Le Finder et l’Explorateur le remplaceraient avec tous les fichiers média.\n\
         4. Si l’on vous demande de remplacer des fichiers .mxf : NE LE FAITES PAS – annulez et vérifiez.\n\
         5. Remplacez msmFMID.pmr et msmMMOB.mdb seulement dans ces dossiers :\n",
        "MediaFiles Check – entrega de {from} para {to}\n\n\
         IMPORTAR CON MEDIAFILES CHECK: Intercambio › Importar (cierra antes Media Composer).\n\n\
         IMPORTAR A MANO:\n\
         1. Cierra Media Composer.\n\
         2. Cada carpeta de esta entrega se llama <volumen>/<carpeta>. En ese volumen abre\n   \
            «Avid MediaFiles/MXF/<carpeta>» (créala si falta).\n\
         3. Copia SOLO EL CONTENIDO de la carpeta de entrega; NUNCA arrastres la carpeta entera sobre una\n   \
            carpeta existente. El Finder y el Explorador la reemplazarían con todos los archivos de medios.\n\
         4. Si se te pregunta si reemplazar archivos .mxf: NO lo hagas; cancela y comprueba.\n\
         5. Reemplaza msmFMID.pmr y msmMMOB.mdb solo en estas carpetas:\n",
    ])
    .replace("{from}", &local.system)
    .replace("{to}", to_system);
    for k in db_folders.keys() {
        text += &format!("   - {k}\n");
    }
    if db_folders.is_empty() {
        text += t(["   (none)\n", "   (keine)\n", "   (aucun)\n", "   (ninguna)\n"]);
    }
    text += t(["\nTASKS:\n", "\nAUFGABEN:\n", "\nTÂCHES :\n", "\nTAREAS:\n"]);
    for task in &tasks {
        text += &format!("[ ] {task}\n");
    }
    for e in &errors {
        text += &format!("[ ] {} {e}\n", t(["Not copied:", "Nicht kopiert:", "Pas copié :", "No copiado:"]));
    }
    fs::write(dest.join(tasks_file(lang)), text)?;
    let delivery = Delivery {
        format: DELIVERY_FORMAT.into(),
        from: local.system.clone(),
        to: to_system.into(),
        created: now_secs(),
        files,
        db_folders,
        tasks,
    };
    write_atomic(&dest.join(DELIVERY_FILE), &serde_json::to_vec_pretty(&delivery)?)?;
    Ok(errors)
}

fn load(dest: &Path) -> io::Result<Delivery> {
    let file = Some(dest.join(DELIVERY_FILE)).filter(|p| p.exists());
    let not_delivery = || io::Error::new(io::ErrorKind::InvalidData, format!("{} is not an Exchange delivery", dest.display()));
    let d: Delivery = serde_json::from_slice(&fs::read(file.ok_or_else(not_delivery)?)?).map_err(|_| not_delivery())?;
    if d.format != DELIVERY_FORMAT {
        return Err(not_delivery());
    }
    // the delivery comes from another system: every path must be exactly "volume/folder/file", nothing that
    // leads out of the delivery or out of the target MXF folder
    if let Some(bad) = d.files.iter().find(|f| !safe_path(&f.path)) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{}: invalid path in the delivery", bad.path)));
    }
    Ok(d)
}

fn safe_path(p: &str) -> bool {
    let parts: Vec<&str> = p.split('/').collect();
    parts.len() == 3 && parts.iter().all(|x| path_part(x))
}

/// Lieferung lesen, ohne etwas zu prüfen (für die Übersicht vor dem Einspielen).
pub fn load_delivery(dest: &Path) -> io::Result<Delivery> {
    load(dest)
}

/// Prüft eine Lieferung gegen ihr Manifest. Gibt die gefundenen Probleme zurück (leer = alles in Ordnung).
pub fn verify(dest: &Path, progress: &mut dyn FnMut(&str)) -> io::Result<Vec<String>> {
    let d = load(dest)?;
    let mut problems = vec![];
    for (i, f) in d.files.iter().enumerate() {
        progress(&format!("[{}/{}] {}", i + 1, d.files.len(), f.path));
        match hash_file(&dest.join(&f.path)) {
            Ok(h) if h == f.hash => {}
            Ok(_) => problems.push(format!("{}: damaged (checksum does not match)", f.path)),
            Err(e) => problems.push(format!("{}: missing or unreadable ({e})", f.path)),
        }
    }
    Ok(problems)
}

pub struct Imported {
    /// Probleme beim Einspielen (nicht eingespielte Dateien, nicht ersetzte Datenbanken)
    pub problems: Vec<String>,
    /// Aufgaben aus der Lieferung, die der User noch von Hand erledigen muss
    pub tasks: Vec<String>,
}

/// Where the files of a delivery go on this system (chosen when importing).
#[derive(Default, Clone)]
pub struct Targets {
    /// delivery volume -> drive (without an entry: the drive of the same name)
    pub media: HashMap<String, PathBuf>,
    /// delivery volume -> drive for its render files; with an entry the volume is separated
    pub render: HashMap<String, PathBuf>,
    /// delivery folders ("Volume/Folder") that go into "<sender>_<Folder>" instead of the folder of the same name
    pub own_folder: HashSet<String>,
}

/// Folder for a delivery folder imported into a folder of its own; if it exists already, the files go into it.
pub fn own_folder_name(from: &str, folder: &str) -> String {
    format!("{}_{folder}", safe_name(from))
}

/// Spielt eine Lieferung in die MXF-Ordner ein. Fügt nur hinzu und überschreibt keine Mediendatei;
/// ersetzte Datenbanken wandern nach "Avid MediaFiles/Exchange-Backup/". Ein Ordner, der Dateien bekommt, aber
/// keine passende Datenbank, bekommt die Aufgabe "Datenbank neu schreiben" (in `lang`).
pub fn import(dest: &Path, t: &Targets, lang: &str, progress: &mut dyn FnMut(&str)) -> io::Result<Imported> {
    let d = load(dest)?;
    let split = |p: &str| -> (String, String, String) {
        let mut it = p.splitn(3, '/').map(String::from);
        (it.next().unwrap_or_default(), it.next().unwrap_or_default(), it.next().unwrap_or_default())
    };

    // erst alle Volumes auflösen, damit nichts halb eingespielt wird
    let found = scan::find_volumes();
    let mut volumes: HashMap<String, PathBuf> = HashMap::new();
    for f in &d.files {
        let (vol, _, _) = split(&f.path);
        if volumes.contains_key(&vol) {
            continue;
        }
        let path = t
            .media
            .get(&vol)
            .cloned()
            .or_else(|| found.iter().find(|p| p.file_name().is_some_and(|n| key(&n.to_string_lossy()) == key(&vol))).cloned())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("drive \"{vol}\" not found – connect or assign it")))?;
        volumes.insert(vol, path);
    }

    // where every media file goes; render files to their own drive if the volume is separated
    // shortcut: a file whose header says nothing counts as media, like Media › Overview's rule-based moves leave it
    let mut media = vec![];
    for f in d.files.iter().filter(|f| !is_db(&split(&f.path).2)) {
        let (vol, folder, name) = split(&f.path);
        let id = format!("{vol}/{folder}");
        let render = t.render.get(&vol).filter(|_| crate::mxf::file_info(&dest.join(&f.path)).is_ok_and(|i| i.render == Some(true)));
        let drive = render.unwrap_or(&volumes[&vol]).clone();
        let to_folder = if t.own_folder.contains(&id) { own_folder_name(&d.from, &folder) } else { folder };
        media.push((f, id, drive, to_folder, name));
    }
    // nothing starts unless every drive keeps its reserve afterwards
    let mut needed: BTreeMap<&PathBuf, u64> = BTreeMap::new();
    for (f, _, drive, folder, name) in &media {
        if !mxf_dir(drive).join(folder).join(name).exists() {
            *needed.entry(drive).or_default() += f.size;
        }
    }
    for (drive, bytes) in needed {
        check_space(drive, bytes)?;
    }

    let mut problems = vec![];
    let total = d.files.len();
    let mut done = 0;
    // delivery folder -> folders its files went to; folder -> (drive name, folder name)
    let mut landed: BTreeMap<String, BTreeSet<PathBuf>> = BTreeMap::new();
    let mut touched: BTreeMap<PathBuf, (String, String)> = BTreeMap::new();
    // Mediendateien zuerst: eine Datenbank darf erst ersetzt werden, wenn ihre Dateien da sind
    for (f, id, drive, to_folder, name) in media {
        done += 1;
        progress(&format!("[{done}/{total}] {}", f.path));
        let dir = mxf_dir(&drive).join(&to_folder);
        if let Err(e) = copy_verified(&dest.join(&f.path), &dir.join(&name), Some(&f.hash)) {
            problems.push(format!("{}: {e}", f.path));
        }
        landed.entry(id).or_default().insert(dir.clone());
        touched.insert(dir, (scan::volume_name(&drive), to_folder));
    }

    let backup = format!("{}_{}", yymmdd(now_secs()), now_secs());
    let mut db_done: HashSet<PathBuf> = HashSet::new();
    for (id, expected) in &d.db_folders {
        // the database fits one folder only: separated into media and render files it fits none
        let Some(dir) = landed.get(id).filter(|s| s.len() == 1).and_then(|s| s.first()) else { continue };
        // Hat sich der Ordner seit dem Scan verändert, passt die gelieferte Datenbank nicht mehr.
        let want: HashSet<String> = expected.iter().map(|n| key(n)).collect();
        match mxf_names(dir) {
            Ok(have) if have == want => {}
            Ok(_) => {
                problems.push(format!("{id}: folder contains other files than expected – database not replaced, please rebuild it"));
                continue;
            }
            Err(e) => {
                problems.push(format!("{id}: {e} – database not replaced, please rebuild it"));
                continue;
            }
        }
        // both new files first go next to the old ones under a hidden name: if one fails, the old database stays whole
        let mut staged = vec![];
        for f in d.files.iter().filter(|f| f.path.starts_with(&format!("{id}/")) && is_db(&split(&f.path).2)) {
            done += 1;
            progress(&format!("[{done}/{total}] {}", f.path));
            let name = split(&f.path).2;
            let new = dir.join(format!(".{name}.exchange-new"));
            let _ = fs::remove_file(&new); // left over from an interrupted import
            match copy_verified(&dest.join(&f.path), &new, Some(&f.hash)) {
                Ok(_) => staged.push((new, name, f.hash.clone())),
                Err(e) => problems.push(format!("{}: {e} – database not replaced, please rebuild it", f.path)),
            }
        }
        if staged.len() != DB_FILES.len() {
            staged.iter().for_each(|(new, _, _)| { let _ = fs::remove_file(new); });
            continue;
        }
        let drive = dir.ancestors().nth(3).unwrap_or(dir);
        let saved = drive.join("Avid MediaFiles").join(BACKUP_DIR).join(&backup).join(dir.file_name().unwrap_or_default());
        let mut ok = true;
        for (new, name, hash) in staged {
            let target = dir.join(&name);
            let swap = || -> io::Result<()> {
                if target.exists() {
                    if hash_file(&target)? == hash {
                        return fs::remove_file(&new);
                    }
                    fs::create_dir_all(&saved)?;
                    crate::move_new(&target, &saved.join(&name))?;
                }
                crate::move_new(&new, &target)
            };
            if let Err(e) = swap() {
                problems.push(format!("{id}/{name}: {e} – old database is in {BACKUP_DIR}/{backup}"));
                let _ = fs::remove_file(&new);
                ok = false;
            }
        }
        if ok {
            db_done.insert(dir.clone());
        }
    }

    // The sender's "rebuild database" tasks name the folders as planned; the folders that really got files replace them.
    let rebuild = |volume: &str, folder: &str, l: &str| Task::RebuildDb { system: d.to.clone(), volume: volume.into(), folder: folder.into() }.describe(l);
    let planned: HashSet<String> = landed
        .keys()
        .flat_map(|id| {
            let (v, f, _) = split(&format!("{id}/"));
            ["en", "de", "fr", "es"].map(|l| rebuild(&v, &f, l))
        })
        .collect();
    let mut tasks: Vec<String> = d.tasks.iter().filter(|t| !planned.contains(*t)).cloned().collect();
    tasks.extend(touched.iter().filter(|(dir, _)| !db_done.contains(*dir)).map(|(_, (v, f))| rebuild(v, f, lang)));
    Ok(Imported { problems, tasks })
}

/// Kopiert über eine Teildatei, prüft Quelle (gegen den Scan) und Ziel (nachgelesen) per Hash und überschreibt nie.
pub(crate) fn copy_verified(src: &Path, dst: &Path, expected: Option<&str>) -> io::Result<(u64, String)> {
    if dst.exists() {
        // Wiederaufnahme: gleiche Datei schon da -> überspringen
        let have = hash_file(dst)?;
        let want = match expected {
            Some(e) => e.to_string(),
            None => hash_file(src)?,
        };
        if have != want {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "target already exists with different content – not overwritten"));
        }
        return Ok((fs::metadata(dst)?.len(), have));
    }
    fs::create_dir_all(dst.parent().unwrap())?;
    let part = dst.with_file_name(format!("{}.exchange-part", dst.file_name().unwrap().to_string_lossy()));
    let result = write_part(src, &part, expected).and_then(|r| crate::move_new(&part, dst).map(|_| r));
    if result.is_err() {
        let _ = fs::remove_file(&part);
    }
    result
}

/// Copies `src` to `part`, then reads `part` back from the drive; error if source or copy differ.
fn write_part(src: &Path, part: &Path, expected: Option<&str>) -> io::Result<(u64, String)> {
    let mut hasher = blake3::Hasher::new();
    let mut input = File::open(src)?;
    let mut out = File::create(part)?;
    no_cache(&out);
    let mut buf = vec![0u8; 1 << 20];
    let mut size = 0;
    loop {
        let n = match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        size += n as u64;
    }
    out.sync_all()?;
    drop(out);

    let hash = hasher.finalize().to_hex().to_string();
    if expected.is_some_and(|e| e != hash) {
        return Err(io::Error::other("source differs from the scan (changed or damaged) – please scan again"));
    }
    let back = File::open(part)?;
    no_cache(&back);
    let mut h = blake3::Hasher::new();
    h.update_reader(back)?;
    if h.finalize().to_hex().to_string() != hash {
        return Err(io::Error::other("copy is faulty – check the target drive"));
    }
    Ok((size, hash))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::plan::{self, Options};

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("exchange-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_folder(dir: &Path, files: &[(&str, &str)]) {
        fs::create_dir_all(dir).unwrap();
        for (n, c) in files {
            fs::write(dir.join(n), c).unwrap();
        }
    }

    #[test]
    fn scan_collect_verify_end_to_end() {
        let root = tmp("e2e");
        let vol = root.join("V");
        let folder = mxf_dir(&vol).join("Avid8.1");
        fs::create_dir_all(folder.join("Creating/creating1")).unwrap();
        write_folder(&folder, &[("a.mxf", "aaa"), ("b.MXF", "bbb"), ("msmFMID.pmr", "db1"), ("msmMMOB.mdb", "db2"), ("notes.txt", "x")]);
        let cache = root.join("cache");
        let s = scan::scan(&[vol.clone()], false, Some(&cache), &mut |_, _, _| {}, &crate::Gate::default()).unwrap();
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
        let f = &s.manifest.volumes[0].folders[0];
        assert_eq!((f.files.len(), f.has_db), (2, true));

        let remote = Manifest { system: "B".into(), volumes: vec![Volume { name: "V".into(), ..Default::default() }], ..Default::default() };
        let p = plan::plan(&s.manifest, &remote, &Options::default());
        let dest = root.join("Lieferung");
        assert!(collect(&s.manifest, "B", &p, &dest, "de", &mut |_| {}).unwrap().is_empty());
        assert!(dest.join("V/Avid8.1/msmMMOB.mdb").exists());
        assert!(fs::read_to_string(dest.join(tasks_file("de"))).unwrap().contains("NUR DEN INHALT"));
        assert!(verify(&dest, &mut |_| {}).unwrap().is_empty());
        // zweiter Lauf setzt fort statt zu überschreiben
        assert!(collect(&s.manifest, "B", &p, &dest, "de", &mut |_| {}).unwrap().is_empty());

        fs::write(dest.join("V/Avid8.1/a.mxf"), "XXX").unwrap();
        assert_eq!(verify(&dest, &mut |_| {}).unwrap().len(), 1);
        assert_eq!(collect(&s.manifest, "B", &p, &dest, "de", &mut |_| {}).unwrap().len(), 1);

        // stille Veränderung (gleiche Größe, gleiches Datum) findet nur die vollständige Prüfung
        let a = folder.join("a.mxf");
        let mtime = fs::metadata(&a).unwrap().modified().unwrap();
        fs::write(&a, "zzz").unwrap();
        File::options().write(true).open(&a).unwrap().set_modified(mtime).unwrap();
        assert!(scan::scan(&[vol.clone()], false, Some(&cache), &mut |_, _, _| {}, &crate::Gate::default()).unwrap().warnings.is_empty());
        assert_eq!(scan::scan(&[vol], true, Some(&cache), &mut |_, _, _| {}, &crate::Gate::default()).unwrap().warnings.len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn import_adds_files_and_replaces_db_only_when_folder_matches() {
        let root = tmp("import");
        let (a, b) = (root.join("A/V"), root.join("B/V"));
        write_folder(&mxf_dir(&a).join("1"), &[("x.mxf", "x"), ("y.mxf", "y"), ("msmFMID.pmr", "newF"), ("msmMMOB.mdb", "newM")]);
        write_folder(&mxf_dir(&a).join("2"), &[("p.mxf", "p"), ("msmFMID.pmr", "p1"), ("msmMMOB.mdb", "p2")]);
        write_folder(&mxf_dir(&b).join("1"), &[("x.mxf", "x"), ("msmFMID.pmr", "oldF"), ("msmMMOB.mdb", "oldM")]);
        write_folder(&mxf_dir(&b).join("2"), &[]);

        let sa = scan::scan(&[a], false, None, &mut |_, _, _| {}, &crate::Gate::default()).unwrap().manifest;
        let sb = scan::scan(&[b.clone()], false, None, &mut |_, _, _| {}, &crate::Gate::default()).unwrap().manifest;
        let p = plan::plan(&sa, &sb, &Options::default());
        let dest = root.join("Lieferung");
        assert!(collect(&sa, "B", &p, &dest, "en", &mut |_| {}).unwrap().is_empty());
        assert!(fs::read_to_string(dest.join(tasks_file("en"))).unwrap().contains("ONLY THE CONTENTS"));

        // seit dem Scan ist auf B in Ordner 2 eine Datei dazugekommen -> dessen DB darf nicht ersetzt werden
        fs::write(mxf_dir(&b).join("2/neu.mxf"), "n").unwrap();
        let t = Targets { media: [("V".to_string(), b.clone())].into(), ..Default::default() };
        let r = import(&dest, &t, "en", &mut |_| {}).unwrap();
        assert_eq!(r.problems.len(), 1, "{:?}", r.problems);
        assert!(r.problems[0].starts_with("V/2:"));

        let read = |p: &str| fs::read_to_string(mxf_dir(&b).join(p)).unwrap();
        assert_eq!((read("1/y.mxf").as_str(), read("1/msmMMOB.mdb").as_str()), ("y", "newM"));
        assert_eq!(read("2/p.mxf"), "p");
        assert!(!mxf_dir(&b).join("2/msmMMOB.mdb").exists());
        let backup = fs::read_dir(b.join("Avid MediaFiles").join(BACKUP_DIR)).unwrap().next().unwrap().unwrap().path();
        assert_eq!(fs::read_to_string(backup.join("1/msmMMOB.mdb")).unwrap(), "oldM");

        // erneutes Einspielen ist harmlos
        assert_eq!(import(&dest, &t, "en", &mut |_| {}).unwrap().problems.len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn import_into_own_folder_with_or_without_database() {
        let root = tmp("ownfolder");
        let (a, b) = (root.join("A/V"), root.join("B/V"));
        write_folder(&mxf_dir(&a).join("1"), &[("x.mxf", "x"), ("y.mxf", "y"), ("msmFMID.pmr", "F1"), ("msmMMOB.mdb", "M1")]);
        write_folder(&mxf_dir(&a).join("2"), &[("p.mxf", "p"), ("msmFMID.pmr", "F2"), ("msmMMOB.mdb", "M2")]);
        let sa = scan::scan(&[a], false, None, &mut |_, _, _| {}, &crate::Gate::default()).unwrap().manifest;
        let (own1, own2) = (own_folder_name(&sa.system, "1"), own_folder_name(&sa.system, "2"));
        // B has the own folder for "2" already (an earlier delivery) with a file of its own
        write_folder(&mxf_dir(&b).join(&own2), &[("old.mxf", "o")]);
        let sb = scan::scan(&[b.clone()], false, None, &mut |_, _, _| {}, &crate::Gate::default()).unwrap().manifest;
        let p = plan::plan(&sa, &sb, &Options::default());
        let dest = root.join("Lieferung");
        assert!(collect(&sa, "B", &p, &dest, "en", &mut |_| {}).unwrap().is_empty());

        let t = Targets { media: [("V".to_string(), b.clone())].into(), own_folder: ["V/1".to_string(), "V/2".to_string()].into(), ..Default::default() };
        let r = import(&dest, &t, "en", &mut |_| {}).unwrap();
        let m = mxf_dir(&b);
        // new folder: exact copy of the source, so its database comes along
        assert_eq!(fs::read_to_string(m.join(&own1).join("msmMMOB.mdb")).unwrap(), "M1");
        assert!(!m.join("1").exists(), "nothing goes into the folder of the same name");
        // existing own folder: files go in, the database does not fit -> task for that folder
        assert_eq!(fs::read_to_string(m.join(&own2).join("p.mxf")).unwrap(), "p");
        assert!(!m.join(&own2).join("msmMMOB.mdb").exists());
        let rebuild: Vec<_> = r.tasks.iter().filter(|t| t.contains("Rebuild database")).cloned().collect();
        assert_eq!(rebuild, [format!("[B] Rebuild database: V/{own2}")]);
        fs::remove_dir_all(root).unwrap();
    }

    /// A on drive V folder 1 (x, y + database), B has x in folder 1 with an old database; scanned and planned.
    fn a_to_b(root: &Path) -> (PathBuf, PathBuf, Manifest, Plan) {
        let (a, b) = (root.join("A/V"), root.join("B/V"));
        write_folder(&mxf_dir(&a).join("1"), &[("x.mxf", "x"), ("y.mxf", "y"), ("msmFMID.pmr", "newF"), ("msmMMOB.mdb", "newM")]);
        write_folder(&mxf_dir(&b).join("1"), &[("x.mxf", "x"), ("msmFMID.pmr", "oldF"), ("msmMMOB.mdb", "oldM")]);
        let sa = scan::scan(&[a.clone()], false, None, &mut |_, _, _| {}, &crate::Gate::default()).unwrap().manifest;
        let sb = scan::scan(&[b.clone()], false, None, &mut |_, _, _| {}, &crate::Gate::default()).unwrap().manifest;
        let p = plan::plan(&sa, &sb, &Options::default());
        (a, b, sa, p)
    }

    #[test]
    fn database_of_a_folder_changed_since_the_scan_is_not_delivered() {
        let root = tmp("changed");
        let (a, _, sa, p) = a_to_b(&root);
        // Media Composer wrote a new file (and its database) after the scan
        fs::write(mxf_dir(&a).join("1/z.mxf"), "z").unwrap();
        let dest = root.join("Lieferung");
        assert!(collect(&sa, "B", &p, &dest, "en", &mut |_| {}).unwrap().is_empty());
        let d = load(&dest).unwrap();
        assert!(d.db_folders.is_empty() && !dest.join("V/1/msmFMID.pmr").exists());
        assert_eq!(d.tasks, ["[B] Rebuild database: V/1"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn old_database_stays_whole_when_the_new_one_fails() {
        let root = tmp("halfdb");
        let (_, b, sa, p) = a_to_b(&root);
        let dest = root.join("Lieferung");
        assert!(collect(&sa, "B", &p, &dest, "en", &mut |_| {}).unwrap().is_empty());
        fs::write(dest.join("V/1/msmMMOB.mdb"), "damaged").unwrap();
        let t = Targets { media: [("V".to_string(), b.clone())].into(), ..Default::default() };
        let r = import(&dest, &t, "en", &mut |_| {}).unwrap();
        let read = |n: &str| fs::read_to_string(mxf_dir(&b).join("1").join(n)).unwrap();
        assert_eq!((read("y.mxf").as_str(), read("msmFMID.pmr").as_str(), read("msmMMOB.mdb").as_str()), ("y", "oldF", "oldM"));
        assert_eq!(r.problems.len(), 1, "{:?}", r.problems);
        assert_eq!(r.tasks, ["[B] Rebuild database: V/1"]);
        let left: Vec<_> = fs::read_dir(mxf_dir(&b).join("1")).unwrap().flatten().map(|e| e.file_name()).filter(|n| n.to_string_lossy().contains("exchange")).collect();
        assert!(left.is_empty(), "{left:?}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn apple_double_files_do_not_block_the_database() {
        let root = tmp("appledouble");
        let (_, b, sa, p) = a_to_b(&root);
        fs::write(mxf_dir(&b).join("1/._x.mxf"), "resource fork").unwrap();
        let dest = root.join("Lieferung");
        assert!(collect(&sa, "B", &p, &dest, "en", &mut |_| {}).unwrap().is_empty());
        let t = Targets { media: [("V".to_string(), b.clone())].into(), ..Default::default() };
        assert!(import(&dest, &t, "en", &mut |_| {}).unwrap().problems.is_empty());
        assert_eq!(fs::read_to_string(mxf_dir(&b).join("1/msmMMOB.mdb")).unwrap(), "newM");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nothing_is_written_without_space() {
        let root = tmp("space");
        let (_, _, sa, mut p) = a_to_b(&root);
        p.copies[0].size = u64::MAX / 2;
        let dest = root.join("Lieferung");
        assert_eq!(collect(&sa, "B", &p, &dest, "en", &mut |_| {}).unwrap_err().kind(), io::ErrorKind::StorageFull);
        assert!(!dest.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_failed_copy_leaves_no_part_file() {
        let root = tmp("part");
        write_folder(&root, &[("a.mxf", "aaa")]);
        assert!(copy_verified(&root.join("a.mxf"), &root.join("t/a.mxf"), Some("not the hash")).is_err());
        assert_eq!(fs::read_dir(root.join("t")).unwrap().count(), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manifest_names_must_be_plain() {
        let root = tmp("names");
        for (folder, ok) in [("Avid.1", true), ("..", false), ("../x", false), ("a\\b", false)] {
            let m = Manifest { format: FORMAT.into(), volumes: vec![Volume { name: "V".into(), folders: vec![Folder { name: folder.into(), ..Default::default() }], ..Default::default() }], ..Default::default() };
            m.save(&root.join("m.json")).unwrap();
            assert_eq!(Manifest::load(&root.join("m.json")).is_ok(), ok, "{folder}");
        }
        fs::remove_dir_all(root).unwrap();
    }


    #[test]
    fn delivery_paths_must_stay_inside() {
        for ok in ["V/1/a.mxf", "V/Mac Studio_1/msmFMID.pmr"] {
            assert!(safe_path(ok), "{ok}");
        }
        for bad in ["../x/a.mxf", "V/../a.mxf", "V/1/../../a", "/V/1/a.mxf", "V/1/sub/a.mxf", "V//a.mxf", "V/1/.", "V/1", "V\\1/x/a"] {
            assert!(!safe_path(bad), "{bad}");
        }
        let dir = std::env::temp_dir().join(format!("mfc-delivery-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let d = Delivery { format: DELIVERY_FORMAT.into(), from: "A".into(), to: "B".into(), created: 0, files: vec![Delivered { path: "V/../../evil".into(), size: 1, hash: "x".into() }], db_folders: Default::default(), tasks: vec![] };
        fs::write(dir.join(DELIVERY_FILE), serde_json::to_vec(&d).unwrap()).unwrap();
        assert!(load(&dir).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
