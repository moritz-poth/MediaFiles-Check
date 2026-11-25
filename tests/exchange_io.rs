//! Exchange I/O tests on real folders: every run records each file (path, size, checksum) before and after
//! and checks the rules – nothing on the sending side changes, on the receiving side files are only added,
//! a replaced database is in Exchange-Backup, and no temporary file is left behind.
use mediafiles_check::Gate;
use mediafiles_check::exchange::collect::{self, Targets, BACKUP_DIR, DELIVERY_FILE};
use mediafiles_check::exchange::plan::{self, Options, Plan};
use mediafiles_check::exchange::{hash_file, mxf_dir, scan, Manifest};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const DB: [&str; 2] = ["msmFMID.pmr", "msmMMOB.mdb"];

/// Every file below `root`: relative path -> checksum.
type Snap = BTreeMap<String, String>;

fn snap(root: &Path) -> Snap {
    fn walk(root: &Path, dir: &Path, out: &mut Snap) {
        for e in fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                out.insert(p.strip_prefix(root).unwrap().to_string_lossy().into_owned(), hash_file(&p).unwrap());
            }
        }
    }
    let mut s = Snap::new();
    walk(root, root, &mut s);
    s
}

struct Lab {
    root: PathBuf,
}

impl Lab {
    fn new(name: &str) -> Lab {
        let root = std::env::temp_dir().join(format!("mfc-io-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Lab { root }
    }
    /// drive `side/vol` (side = "A" sender, "B" receiver)
    fn drive(&self, side: &str, vol: &str) -> PathBuf {
        self.root.join(side).join(vol)
    }
    /// folder with files "name" (content = name) or "name=content"; "+db" adds both database files
    fn folder(&self, side: &str, vol: &str, folder: &str, files: &[&str]) {
        let dir = mxf_dir(&self.drive(side, vol)).join(folder);
        fs::create_dir_all(&dir).unwrap();
        for f in files {
            if *f == "+db" {
                for d in DB {
                    fs::write(dir.join(d), format!("{side} {folder} {d}")).unwrap();
                }
                continue;
            }
            let (n, c) = f.split_once('=').unwrap_or((f, f));
            fs::write(dir.join(n), c).unwrap();
        }
    }
    fn scan(&self, side: &str) -> Manifest {
        let mut vols: Vec<PathBuf> = fs::read_dir(self.root.join(side)).unwrap().flatten().map(|e| e.path()).collect();
        vols.sort();
        let mut m = scan::scan(&vols, false, None, &mut |_, _, _| {}, &Gate::default()).unwrap().manifest;
        m.system = side.into();
        m
    }
    fn dest(&self) -> PathBuf {
        self.root.join("Delivery")
    }
    fn targets(&self, vols: &[&str]) -> Targets {
        Targets { media: vols.iter().map(|v| (v.to_string(), self.drive("B", v))).collect(), ..Default::default() }
    }
    fn read(&self, side: &str, vol: &str, rel: &str) -> Option<String> {
        fs::read_to_string(mxf_dir(&self.drive(side, vol)).join(rel)).ok()
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Compare A with B, collect, check: A unchanged, delivery verifies, no part files.
fn collect_checked(lab: &Lab, opts: &Options) -> Plan {
    let (a, b) = (lab.scan("A"), lab.scan("B"));
    let p = plan::plan(&a, &b, opts);
    let before = snap(&lab.root.join("A"));
    let errors = collect::collect(&a, "B", &p, &lab.dest(), "en", &mut |_| {}).unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(snap(&lab.root.join("A")), before, "collecting changed the sender");
    assert!(collect::verify(&lab.dest(), &mut |_| {}).unwrap().is_empty());
    no_temp_files(&lab.root);
    p
}

/// Import and check the receiving side's rules; returns (problems, tasks).
fn import_checked(lab: &Lab, t: &Targets) -> (Vec<String>, Vec<String>) {
    let b = lab.root.join("B");
    let before = snap(&b);
    let r = collect::import(&lab.dest(), t, "en", &mut |_| {}).unwrap();
    let after = snap(&b);
    for (path, hash) in &before {
        match after.get(path) {
            Some(h) if h == hash => {}
            // the import's own half-written file from an interrupted run is written again
            _ if path.ends_with(".exchange-part") => {}
            _ => {
                // only a database may change, and then its old version is in the backup
                assert!(DB.iter().any(|d| path.ends_with(d)), "{path} was changed or removed");
                assert!(after.iter().any(|(p, h)| p.contains(BACKUP_DIR) && h == hash), "{path}: old database not in the backup");
            }
        }
    }
    no_temp_files(&lab.root);
    (r.problems, r.tasks)
}

fn no_temp_files(root: &Path) {
    let left: Vec<String> = snap(root).into_keys().filter(|p| p.contains(".exchange-part") || p.contains(".exchange-new") || p.ends_with(".tmp")).collect();
    assert!(left.is_empty(), "left behind: {left:?}");
}

// ── normal runs ──

#[test]
fn identical_sides_deliver_nothing() {
    let lab = Lab::new("same");
    for s in ["A", "B"] {
        lab.folder(s, "V", "1", &["a.mxf", "b.mxf", "+db"]);
    }
    let p = collect_checked(&lab, &Options::default());
    assert!(p.copies.is_empty() && p.tasks.is_empty());
}

#[test]
fn missing_folder_arrives_exactly_with_its_database() {
    let lab = Lab::new("missing");
    lab.folder("A", "V", "1", &["a.mxf", "b.mxf", "+db"]);
    fs::create_dir_all(mxf_dir(&lab.drive("B", "V"))).unwrap();
    collect_checked(&lab, &Options::default());
    let (problems, tasks) = import_checked(&lab, &lab.targets(&["V"]));
    assert!(problems.is_empty() && tasks.is_empty(), "{problems:?} {tasks:?}");
    let folder = |s| snap(&mxf_dir(&lab.drive(s, "V")).join("1"));
    assert_eq!(folder("A"), folder("B"));
}

#[test]
fn receiver_with_more_files_gets_files_and_a_rebuild_task() {
    let lab = Lab::new("more");
    lab.folder("A", "V", "1", &["a.mxf", "b.mxf", "+db"]);
    lab.folder("B", "V", "1", &["a.mxf", "own.mxf", "+db"]);
    collect_checked(&lab, &Options::default());
    let (problems, tasks) = import_checked(&lab, &lab.targets(&["V"]));
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(tasks, ["[B] Rebuild database: V/1"]);
    assert_eq!(lab.read("B", "V", "1/b.mxf").as_deref(), Some("b.mxf"));
    assert_eq!(lab.read("B", "V", "1/msmFMID.pmr").as_deref(), Some("B 1 msmFMID.pmr"), "database kept");
}

#[test]
fn file_in_another_folder_over_there_is_not_sent() {
    let lab = Lab::new("elsewhere");
    lab.folder("A", "V", "1", &["a.mxf", "+db"]);
    lab.folder("B", "V", "7", &["a.mxf"]);
    let p = collect_checked(&lab, &Options::default());
    assert!(p.copies.is_empty());
}

#[test]
fn same_name_other_content_is_never_overwritten() {
    let lab = Lab::new("conflict");
    lab.folder("A", "V", "1", &["a.mxf=new", "b.mxf", "+db"]);
    lab.folder("B", "V", "1", &["a.mxf=old"]);
    let p = collect_checked(&lab, &Options::default());
    assert_eq!(p.copies.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["b.mxf"]);
    assert!(p.tasks.iter().any(|t| t.to_string().contains("Same name, different content")));
    let (_, tasks) = import_checked(&lab, &lab.targets(&["V"]));
    assert_eq!(lab.read("B", "V", "1/a.mxf").as_deref(), Some("old"));
    assert!(tasks.iter().any(|t| t.contains("Same name, different content")), "{tasks:?}");
}

#[test]
fn two_volumes_into_one() {
    let lab = Lab::new("twoone");
    lab.folder("A", "V1", "1", &["a.mxf", "+db"]);
    lab.folder("A", "V2", "1", &["b.mxf", "+db"]);
    fs::create_dir_all(mxf_dir(&lab.drive("B", "W"))).unwrap();
    let opts = Options { volume_map: [("V1".into(), "W".into()), ("V2".into(), "W".into())].into() };
    collect_checked(&lab, &opts);
    let (problems, tasks) = import_checked(&lab, &lab.targets(&["W"]));
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(tasks, ["[B] Rebuild database: W/1"], "two databases never fit one folder");
    assert!(lab.read("B", "W", "1/a.mxf").is_some() && lab.read("B", "W", "1/b.mxf").is_some());
    assert!(lab.read("B", "W", "1/msmFMID.pmr").is_none());
}

#[test]
fn importing_twice_changes_nothing_more() {
    let lab = Lab::new("twice");
    lab.folder("A", "V", "1", &["a.mxf", "b.mxf", "+db"]);
    fs::create_dir_all(mxf_dir(&lab.drive("B", "V"))).unwrap();
    collect_checked(&lab, &Options::default());
    import_checked(&lab, &lab.targets(&["V"]));
    let first = snap(&lab.root.join("B"));
    let (problems, _) = import_checked(&lab, &lab.targets(&["V"]));
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(snap(&lab.root.join("B")), first);
}

#[test]
fn interrupted_import_continues() {
    let lab = Lab::new("resume");
    lab.folder("A", "V", "1", &["a.mxf", "b.mxf", "c.mxf", "+db"]);
    fs::create_dir_all(mxf_dir(&lab.drive("B", "V"))).unwrap();
    collect_checked(&lab, &Options::default());
    // the first run stopped after a.mxf, with b.mxf half written
    lab.folder("B", "V", "1", &["a.mxf", "b.mxf.exchange-part=b."]);
    let (problems, tasks) = import_checked(&lab, &lab.targets(&["V"]));
    assert!(problems.is_empty() && tasks.is_empty(), "{problems:?} {tasks:?}");
    assert_eq!(snap(&mxf_dir(&lab.drive("A", "V")).join("1")), snap(&mxf_dir(&lab.drive("B", "V")).join("1")));
}

#[test]
fn folder_changed_on_the_receiver_keeps_its_database() {
    let lab = Lab::new("recvchanged");
    lab.folder("A", "V", "1", &["a.mxf", "b.mxf", "+db"]);
    lab.folder("B", "V", "1", &["a.mxf", "+db"]);
    collect_checked(&lab, &Options::default());
    lab.folder("B", "V", "1", &["new.mxf"]);
    let (problems, tasks) = import_checked(&lab, &lab.targets(&["V"]));
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(tasks, ["[B] Rebuild database: V/1"]);
    assert_eq!(lab.read("B", "V", "1/msmFMID.pmr").as_deref(), Some("B 1 msmFMID.pmr"));
}

#[test]
fn own_folder_and_separated_render_drive() {
    let lab = Lab::new("own");
    lab.folder("A", "V", "1", &["a.mxf", "+db"]);
    fs::create_dir_all(mxf_dir(&lab.drive("B", "V"))).unwrap();
    fs::create_dir_all(mxf_dir(&lab.drive("B", "R"))).unwrap();
    collect_checked(&lab, &Options::default());
    let mut t = lab.targets(&["V"]);
    t.own_folder.insert("V/1".into());
    // no file here is a render file (no MXF header): everything stays on the media drive
    t.render.insert("V".into(), lab.drive("B", "R"));
    let (problems, tasks) = import_checked(&lab, &t);
    assert!(problems.is_empty() && tasks.is_empty(), "{problems:?} {tasks:?}");
    let own = collect::own_folder_name("A", "1");
    assert!(lab.read("B", "V", &format!("{own}/a.mxf")).is_some());
    assert!(lab.read("B", "V", &format!("{own}/msmMMOB.mdb")).is_some());
    assert!(!mxf_dir(&lab.drive("B", "V")).join("1").exists());
    assert!(snap(&lab.drive("B", "R")).is_empty());
}

#[test]
fn unicode_and_case_in_names() {
    let lab = Lab::new("unicode");
    // NFD on the sender, NFC and other case on the receiver: the same file
    lab.folder("A", "V", "Mu\u{0308}ller.1", &["Ka\u{0308}se.mxf", "neu.mxf", "+db"]);
    lab.folder("B", "V", "müller.1", &["käse.MXF=Ka\u{0308}se.mxf"]);
    let p = collect_checked(&lab, &Options::default());
    assert_eq!(p.copies.iter().filter(|c| c.hash.is_some()).map(|c| c.name.as_str()).collect::<Vec<_>>(), ["neu.mxf"]);
    let (problems, _) = import_checked(&lab, &lab.targets(&["V"]));
    assert!(problems.is_empty(), "{problems:?}");
    assert!(lab.read("B", "V", "müller.1/neu.mxf").is_some(), "goes into the existing folder");
}

// ── faulty deliveries ──

fn delivered(lab: &Lab) {
    lab.folder("A", "V", "1", &["a.mxf", "b.mxf", "+db"]);
    lab.folder("B", "V", "1", &["a.mxf", "+db"]);
    collect_checked(&lab, &Options::default());
}

#[test]
fn damaged_file_is_found_and_never_imported() {
    let lab = Lab::new("damaged");
    delivered(&lab);
    fs::write(lab.dest().join("V/1/b.mxf"), "b.mxX").unwrap();
    assert_eq!(collect::verify(&lab.dest(), &mut |_| {}).unwrap().len(), 1);
    let (problems, tasks) = import_checked(&lab, &lab.targets(&["V"]));
    assert!(!problems.is_empty());
    assert!(lab.read("B", "V", "1/b.mxf").is_none());
    assert_eq!(lab.read("B", "V", "1/msmFMID.pmr").as_deref(), Some("B 1 msmFMID.pmr"));
    assert_eq!(tasks, ["[B] Rebuild database: V/1"]);
}

#[test]
fn missing_file_is_found() {
    let lab = Lab::new("gone");
    delivered(&lab);
    fs::remove_file(lab.dest().join("V/1/b.mxf")).unwrap();
    assert_eq!(collect::verify(&lab.dest(), &mut |_| {}).unwrap().len(), 1);
    let (problems, _) = import_checked(&lab, &lab.targets(&["V"]));
    assert!(!problems.is_empty());
    assert_eq!(lab.read("B", "V", "1/msmFMID.pmr").as_deref(), Some("B 1 msmFMID.pmr"));
}

#[test]
fn extra_file_in_the_delivery_is_ignored() {
    let lab = Lab::new("extra");
    delivered(&lab);
    fs::write(lab.dest().join("V/1/smuggled.mxf"), "x").unwrap();
    assert!(collect::verify(&lab.dest(), &mut |_| {}).unwrap().is_empty());
    let (problems, _) = import_checked(&lab, &lab.targets(&["V"]));
    assert!(problems.is_empty(), "{problems:?}");
    assert!(lab.read("B", "V", "1/smuggled.mxf").is_none());
}

#[test]
fn broken_delivery_file_is_refused() {
    let lab = Lab::new("broken");
    delivered(&lab);
    let file = lab.dest().join(DELIVERY_FILE);
    let json = fs::read_to_string(&file).unwrap();
    let before = snap(&lab.root.join("B"));
    for bad in [json[..json.len() / 2].to_string(), json.replace("mediafiles-exchange-delivery", "other"), json.replace("\"V/1/b.mxf\"", "\"V/../../b.mxf\""), json.replace("\"V/1/b.mxf\"", "\"V\\\\1/b.mxf\"")] {
        fs::write(&file, bad).unwrap();
        assert!(collect::verify(&lab.dest(), &mut |_| {}).is_err());
        assert!(collect::import(&lab.dest(), &lab.targets(&["V"]), "en", &mut |_| {}).is_err());
    }
    assert_eq!(snap(&lab.root.join("B")), before);
}

// ── the sender changes between scan and collect ──

#[test]
fn source_changed_after_the_scan_is_not_delivered() {
    let lab = Lab::new("srcchanged");
    lab.folder("A", "V", "1", &["a.mxf", "b.mxf", "c.mxf", "+db"]);
    fs::create_dir_all(mxf_dir(&lab.drive("B", "V"))).unwrap();
    let (a, b) = (lab.scan("A"), lab.scan("B"));
    let p = plan::plan(&a, &b, &Options::default());
    lab.folder("A", "V", "1", &["a.mxf=changed"]);
    fs::remove_file(mxf_dir(&lab.drive("A", "V")).join("1/b.mxf")).unwrap();
    let errors = collect::collect(&a, "B", &p, &lab.dest(), "en", &mut |_| {}).unwrap();
    assert_eq!(errors.len(), 2, "{errors:?}");
    // what is in the delivery is consistent; the database is left out (folder differs from the scan)
    assert!(collect::verify(&lab.dest(), &mut |_| {}).unwrap().is_empty());
    no_temp_files(&lab.root);
    let (problems, tasks) = import_checked(&lab, &lab.targets(&["V"]));
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(tasks, ["[B] Rebuild database: V/1"]);
    assert_eq!(snap(&mxf_dir(&lab.drive("B", "V")).join("1")).into_keys().collect::<Vec<_>>(), ["c.mxf"]);
}
