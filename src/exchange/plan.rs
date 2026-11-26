//! Vergleich zweier Manifeste: was muss von hier zur Gegenseite, und was muss der User von Hand erledigen.
use crate::exchange::*;
use std::collections::{HashMap, HashSet};
use std::fmt;

/// Avid legt ab etwa 5000 Dateien einen neuen Ordner an; größere Ordner sollten aufgeteilt werden.

#[derive(Default)]
pub struct Options {
    /// local volumes the user chose not to send
    pub skip: HashSet<String>,
    /// leave out render files / mixdowns (by the manifest's header kind)
    pub skip_renders: bool,
    pub skip_mixdowns: bool,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Copy {
    pub from_volume: String,
    pub from_folder: String,
    pub to_volume: String,
    pub to_folder: String,
    pub name: String,
    pub size: u64,
    /// None bei Datenbankdateien (die werden nicht gehasht)
    pub hash: Option<String>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub enum Task {
    RebuildDb { system: String, volume: String, folder: String },
    Conflict { system: String, volume: String, folder: String, name: String },
    /// requested files (MediaFiles Check) this system does not have; clips that use them
    NotFound { system: String, files: usize, clips: Vec<String> },
}

impl Task {
    /// Text in the app's language (en, de, fr, es); shown in the app and written to the delivery's task file.
    pub fn describe(&self, lang: &str) -> String {
        let t = |a: [&'static str; 4]| crate::pick(lang, a);
        match self {
            Task::RebuildDb { system, volume, folder } => format!("[{system}] {}: {volume}/{folder}", t(["Rebuild database", "Datenbank neu schreiben", "Reconstruire la base de données", "Reconstruir la base de datos"])),
            Task::Conflict { system, volume, folder, name } => format!("[{system}] {}: {volume}/{folder}/{name}", t(["Same name, different content – not copied, please check", "Gleicher Name, anderer Inhalt – nicht kopiert, bitte prüfen", "Même nom, contenu différent – pas copié, à vérifier", "Mismo nombre, contenido distinto: no se copió, compruébalo"])),
            Task::NotFound { system, files, clips } => {
                let mut shown = clips.iter().take(10).cloned().collect::<Vec<_>>().join(", ");
                if clips.len() > 10 {
                    shown += &t([" and {n} more", " und {n} weitere", " et {n} autres", " y {n} más"]).replace("{n}", &(clips.len() - 10).to_string());
                }
                format!("[{system}] {}{shown}", t(["{n} requested files not found here (scan up to date?) – clips: ", "{n} angeforderte Dateien hier nicht gefunden (Scan aktuell?) – Clips: ", "{n} fichiers demandés introuvables ici (analyse à jour ?) – clips : ", "{n} archivos solicitados no encontrados aquí (¿análisis al día?): clips: "]).replace("{n}", &files.to_string()))
            }
        }
    }
}

impl fmt::Display for Task {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.describe("en"))
    }
}

#[derive(Default, Debug, Clone)]
pub struct Plan {
    pub copies: Vec<Copy>,
    pub tasks: Vec<Task>,
}

struct Proposal<'a> {
    from_volume: &'a str,
    from: &'a Folder,
    to_volume: String,
    to_folder: String,
    missing: Vec<&'a FileEntry>,
    db_ok: bool,
}

/// Was von `local` auf `remote` fehlt. Beide Seiten rechnen das unabhängig voneinander mit dem Manifest der anderen.
pub fn plan(local: &Manifest, remote: &Manifest, opts: &Options) -> Plan {
    let mut p = Plan::default();

    // Eine Datei gilt als vorhanden, wenn sie irgendwo auf der Gegenseite mit gleichem Namen und Hash liegt.
    let mut remote_files: HashMap<String, HashSet<&str>> = HashMap::new();
    for e in remote.volumes.iter().flat_map(|v| &v.folders).flat_map(|f| &f.files) {
        remote_files.entry(key(&e.name)).or_default().insert(&e.hash);
    }
    let mut proposals = vec![];

    let left_out = |e: &FileEntry| match e.kind {
        Some(FileKind::Render) => opts.skip_renders,
        Some(FileKind::Mixdown) => opts.skip_mixdowns,
        _ => false,
    };
    // every drive goes under its own name; the receiver assigns it to one of its drives when importing (user 2026-11-26)
    for lv in local.volumes.iter().filter(|v| !opts.skip.contains(&v.name)) {
        let rv = remote.volumes.iter().find(|rv| key(&rv.name) == key(&lv.name));
        let to_volume = rv.map_or_else(|| lv.name.clone(), |rv| rv.name.clone());
        for f in &lv.folders {
            let mut missing = vec![];
            for e in f.files.iter().filter(|e| !left_out(e)) {
                match remote_files.get(&key(&e.name)) {
                    None => missing.push(e),
                    Some(hashes) if hashes.contains(e.hash.as_str()) => {}
                    Some(_) => p.tasks.push(Task::Conflict {
                        system: local.system.clone(),
                        volume: lv.name.clone(),
                        folder: f.name.clone(),
                        name: e.name.clone(),
                    }),
                }
            }
            if missing.is_empty() {
                continue;
            }
            // the folder over there: on the drive of the same name; without one, the only folder of that name on any drive
            // there (two of them: not known which one it lands in – no database, a rebuild task)
            let named: Vec<&Folder> = remote.volumes.iter().flat_map(|v| &v.folders).filter(|rf| key(&rf.name) == key(&f.name)).collect();
            let same = match rv {
                Some(rv) => rv.folders.iter().find(|rf| key(&rf.name) == key(&f.name)),
                None => named.first().copied().filter(|_| named.len() == 1),
            };
            let unknown = rv.is_none() && named.len() > 1;
            // the receiver decides on import whether a folder merges or goes into a folder of its own
            let (to_folder, existing) = (same.map_or_else(|| f.name.clone(), |s| s.name.clone()), same);
            let mine: HashSet<(String, &str)> = f.files.iter().map(|e| (key(&e.name), e.hash.as_str())).collect();
            let subset = existing.is_none_or(|x| x.files.iter().all(|e| mine.contains(&(key(&e.name), e.hash.as_str()))));
            let existing = existing.map_or(0, |x| x.files.len());
            // Die DB passt nur, wenn der Zielordner danach exakt dem Quellordner entspricht.
            let db_ok = f.has_db && !unknown && subset && existing + missing.len() == f.files.len();
            proposals.push(Proposal { from_volume: &lv.name, from: f, to_volume: to_volume.clone(), to_folder, missing, db_ok });
        }
    }

    // Mehrere lokale Ordner können im selben Zielordner landen (z. B. zwei lokale Volumes -> ein Volume drüben).
    let target = |pr: &Proposal| (key(&pr.to_volume), key(&pr.to_folder));
    let mut incoming: HashMap<(String, String), usize> = HashMap::new(); // Quellordner je Zielordner
    for pr in &proposals {
        *incoming.entry(target(pr)).or_default() += 1;
    }
    for pr in &proposals {
        let sources = incoming[&target(pr)];
        let copy = |name: &str, size, hash| Copy {
            from_volume: pr.from_volume.into(),
            from_folder: pr.from.name.clone(),
            to_volume: pr.to_volume.clone(),
            to_folder: pr.to_folder.clone(),
            name: name.into(),
            size,
            hash,
        };
        p.copies.extend(pr.missing.iter().map(|e| copy(&e.name, e.size, Some(e.hash.clone()))));
        if pr.db_ok && sources == 1 {
            p.copies.extend(DB_FILES.iter().map(|db| copy(db, 0, None)));
        } else {
            push_once(&mut p.tasks, Task::RebuildDb { system: remote.system.clone(), volume: pr.to_volume.clone(), folder: pr.to_folder.clone() });
        }
    }
    // only tasks the exchange itself causes (user 2026-11-16): no hints about full folders or duplicates
    p
}

fn push_once(tasks: &mut Vec<Task>, t: Task) {
    if !tasks.contains(&t) {
        tasks.push(t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dateien als "name" (Hash = Name) oder "name=hash"
    fn fo(name: &str, files: &[&str]) -> Folder {
        let files = files
            .iter()
            .map(|f| {
                let (n, h) = f.split_once('=').unwrap_or((f, f));
                FileEntry { name: n.into(), size: 1, hash: h.into(), kind: None }
            })
            .collect();
        Folder { name: name.into(), has_db: true, files }
    }

    fn ma(system: &str, vols: Vec<(&str, Vec<Folder>)>) -> Manifest {
        let volumes = vols.into_iter().map(|(n, folders)| Volume { name: n.into(), path: String::new(), folders }).collect();
        Manifest { system: system.into(), volumes, ..Default::default() }
    }

    fn run(local: &Manifest, remote: &Manifest, opts: &Options) -> (Vec<String>, Plan) {
        let p = plan(local, remote, opts);
        (p.copies.iter().map(|c| format!("{}/{}/{}", c.to_volume, c.to_folder, c.name)).collect(), p)
    }

    fn rebuild(volume: &str, folder: &str) -> Task {
        Task::RebuildDb { system: "B".into(), volume: volume.into(), folder: folder.into() }
    }

    #[test]
    fn identical_folders_copy_nothing() {
        let a = ma("A", vec![("V", vec![fo("1", &["a", "b"])])]);
        let (copies, p) = run(&a, &a.clone(), &Options::default());
        assert!(copies.is_empty() && p.tasks.is_empty());
    }

    #[test]
    fn missing_folder_is_copied_with_db() {
        let a = ma("A", vec![("V", vec![fo("1", &["a", "b"])])]);
        let b = ma("B", vec![("V", vec![])]);
        let (copies, p) = run(&a, &b, &Options::default());
        assert_eq!(copies, ["V/1/a", "V/1/b", "V/1/msmFMID.pmr", "V/1/msmMMOB.mdb"]);
        assert!(p.tasks.is_empty());
    }

    #[test]
    fn superset_brings_its_db() {
        let a = ma("A", vec![("V", vec![fo("1", &["a", "b"])])]);
        let b = ma("B", vec![("V", vec![fo("1", &["a"])])]);
        let (copies, p) = run(&a, &b, &Options::default());
        assert_eq!(copies, ["V/1/b", "V/1/msmFMID.pmr", "V/1/msmMMOB.mdb"]);
        assert!(p.tasks.is_empty());
    }

    #[test]
    fn both_sides_unique_means_rebuild_task() {
        let a = ma("A", vec![("V", vec![fo("1", &["a", "b"])])]);
        let b = ma("B", vec![("V", vec![fo("1", &["a", "c"])])]);
        let (copies, p) = run(&a, &b, &Options::default());
        assert_eq!(copies, ["V/1/b"]);
        assert_eq!(p.tasks, [rebuild("V", "1")]);
    }

    #[test]
    fn file_elsewhere_counts_as_present() {
        let a = ma("A", vec![("V", vec![fo("1", &["a", "b", "c"])])]);
        let b = ma("B", vec![("V", vec![fo("1", &["a"]), fo("2", &["b"])]), ("W", vec![])]);
        let (copies, p) = run(&a, &b, &Options::default());
        assert_eq!(copies, ["V/1/c"]);
        assert_eq!(p.tasks, [rebuild("V", "1")]);
    }

    #[test]
    fn same_name_other_content_is_not_copied() {
        let a = ma("A", vec![("V", vec![fo("1", &["a=h1"])])]);
        let b = ma("B", vec![("V", vec![fo("1", &["a=h2"])])]);
        let (copies, p) = run(&a, &b, &Options::default());
        assert!(copies.is_empty());
        assert!(matches!(&p.tasks[..], [Task::Conflict { name, .. }] if name == "a"));
    }

    #[test]
    fn a_drive_without_counterpart_goes_under_its_own_name() {
        // the receiver assigns it when importing
        let a = ma("A", vec![("X", vec![fo("1", &["a"])])]);
        let b = ma("B", vec![("Y", vec![])]);
        let (copies, p) = run(&a, &b, &Options::default());
        assert_eq!(copies, ["X/1/a", "X/1/msmFMID.pmr", "X/1/msmMMOB.mdb"]);
        assert!(p.tasks.is_empty());
    }

    #[test]
    fn a_folder_of_the_same_name_on_another_drive_over_there_decides_the_database() {
        // over there "1" is on "Y" with a file of its own: the database does not fit
        let a = ma("A", vec![("X", vec![fo("1", &["a", "b"])])]);
        let b = ma("B", vec![("Y", vec![fo("1", &["a", "c"])])]);
        let (copies, p) = run(&a, &b, &Options::default());
        assert_eq!(copies, ["X/1/b"]);
        assert_eq!(p.tasks, [rebuild("X", "1")]);
        // "1" on two drives over there: not known where it lands, so no database
        let b = ma("B", vec![("Y", vec![fo("1", &["a"])]), ("Z", vec![fo("1", &["z"])])]);
        let (copies, p) = run(&a, &b, &Options::default());
        assert_eq!(copies, ["X/1/b"]);
        assert_eq!(p.tasks, [rebuild("X", "1")]);
    }

    #[test]
    fn render_files_and_mixdowns_can_be_left_out() {
        let mut f = fo("1", &["a", "r", "m"]);
        f.files[1].kind = Some(FileKind::Render);
        f.files[2].kind = Some(FileKind::Mixdown);
        let a = ma("A", vec![("V", vec![f])]);
        let b = ma("B", vec![("V", vec![])]);
        assert_eq!(run(&a, &b, &Options::default()).0, ["V/1/a", "V/1/r", "V/1/m", "V/1/msmFMID.pmr", "V/1/msmMMOB.mdb"]);
        let (copies, p) = run(&a, &b, &Options { skip_renders: true, ..Default::default() });
        // the folder ends up without the render file: its database does not fit
        assert_eq!(copies, ["V/1/a", "V/1/m"]);
        assert_eq!(p.tasks, [rebuild("V", "1")]);
        let (copies, _) = run(&a, &b, &Options { skip_renders: true, skip_mixdowns: true, ..Default::default() });
        assert_eq!(copies, ["V/1/a"]);
    }

    #[test]
    fn names_compare_unicode_and_case_insensitive() {
        let a = ma("A", vec![("V", vec![fo("Avid8.1", &["A\u{0308}.mxf"])])]);
        let b = ma("B", vec![("V", vec![fo("avid8.1", &["\u{00e4}.mxf=A\u{0308}.mxf"])])]);
        assert!(run(&a, &b, &Options::default()).0.is_empty());
    }

    #[test]
    fn only_exchange_tasks() {
        // a full folder and a duplicate on this side are no task of the exchange
        let many: Vec<String> = (0..=5000).map(|i| format!("f{i}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        let a = ma("A", vec![("V", vec![fo("1", &many), fo("2", &["f1", "f2"])])]);
        let b = ma("B", vec![("V", vec![])]);
        let (copies, p) = run(&a, &b, &Options::default());
        assert_eq!(copies.len(), 5001 + 2 + 2 + 2); // beide Ordner fehlen drüben: Dateien + je 2 DB
        assert!(p.tasks.is_empty());
    }
}
