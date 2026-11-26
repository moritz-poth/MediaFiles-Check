//! Shared by the Exchange I/O tests: test drives A (sender) and B (receiver), snapshots, rule checks.
#![allow(dead_code)]
use mediafiles_check::Gate;
use mediafiles_check::exchange::collect::{self, Targets, BACKUP_DIR};
use mediafiles_check::exchange::plan::{self, Options, Plan};
use mediafiles_check::exchange::{hash_file, mxf_dir, scan, Manifest};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const DB: [&str; 2] = ["msmFMID.pmr", "msmMMOB.mdb"];

/// Every file below `root`: relative path -> checksum.
pub type Snap = BTreeMap<String, String>;

pub fn snap(root: &Path) -> Snap {
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

pub struct Lab {
    pub root: PathBuf,
    /// drives of the receiver B (own file system in scripts/exchange-disks.sh: `MFC_IO_B`)
    pub b: PathBuf,
}

impl Lab {
    pub fn new(name: &str) -> Lab {
        let base = std::env::var_os("MFC_IO_ROOT").map_or_else(std::env::temp_dir, PathBuf::from);
        let root = base.join(format!("mfc-io-{name}-{}", std::process::id()));
        let b = std::env::var_os("MFC_IO_B").map_or_else(|| root.join("B"), |d| PathBuf::from(d).join(format!("mfc-io-{name}-{}", std::process::id())));
        for d in [&root, &b] {
            let _ = fs::remove_dir_all(d);
            fs::create_dir_all(d).unwrap();
        }
        Lab { root, b }
    }
    /// the lab of another process (a child that gets killed), nothing created or removed
    pub fn open(root: PathBuf, b: PathBuf) -> std::mem::ManuallyDrop<Lab> {
        std::mem::ManuallyDrop::new(Lab { root, b })
    }
    fn side(&self, side: &str) -> PathBuf {
        if side == "B" { self.b.clone() } else { self.root.join(side) }
    }
    /// drive `side/vol` (side = "A" sender, "B" receiver)
    pub fn drive(&self, side: &str, vol: &str) -> PathBuf {
        self.side(side).join(vol)
    }
    /// folder with files "name" (content = name) or "name=content"; "+db" adds both database files
    pub fn folder(&self, side: &str, vol: &str, folder: &str, files: &[&str]) {
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
    pub fn scan(&self, side: &str) -> Manifest {
        let mut vols: Vec<PathBuf> = fs::read_dir(self.side(side)).unwrap().flatten().map(|e| e.path()).collect();
        vols.sort();
        let mut m = scan::scan(&vols, false, None, &mut |_, _, _| {}, &Gate::default()).unwrap().manifest;
        m.system = side.into();
        m
    }
    pub fn dest(&self) -> PathBuf {
        self.root.join("Delivery")
    }
    pub fn targets(&self, vols: &[&str]) -> Targets {
        Targets { media: vols.iter().map(|v| (v.to_string(), self.drive("B", v))).collect(), ..Default::default() }
    }
    pub fn read(&self, side: &str, vol: &str, rel: &str) -> Option<String> {
        fs::read_to_string(mxf_dir(&self.drive(side, vol)).join(rel)).ok()
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
        let _ = fs::remove_dir_all(&self.b);
    }
}

/// Compare A with B, collect, check: A unchanged, delivery verifies, no part files.
pub fn collect_checked(lab: &Lab, opts: &Options) -> Plan {
    let (a, b) = (lab.scan("A"), lab.scan("B"));
    let p = plan::plan(&a, &b, opts);
    let before = snap(&lab.root.join("A"));
    let (errors, _) = collect::collect(&a, "B", &p, &lab.dest(), "en", &mut |_| {}).unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(snap(&lab.root.join("A")), before, "collecting changed the sender");
    assert!(collect::verify(&lab.dest(), &mut |_| {}).unwrap().is_empty());
    no_temp_files(&lab.root);
    p
}

/// Import and check the receiving side's rules; returns (problems, tasks).
pub fn import_checked(lab: &Lab, t: &Targets) -> (Vec<String>, Vec<String>) {
    let b = lab.b.clone();
    let before = snap(&b);
    let r = collect::import(&lab.dest(), t, "en", &mut |_| {}).unwrap();
    let after = snap(&b);
    for (path, hash) in &before {
        match after.get(path) {
            Some(h) if h == hash => {}
            // the import's own temporary files from an interrupted run are written again or cleared
            _ if path.ends_with(".exchange-part") || path.ends_with(".exchange-new") => {}
            _ => {
                // only a database may change, and then its old version is in the backup
                assert!(DB.iter().any(|d| path.ends_with(d)), "{path} was changed or removed");
                assert!(after.iter().any(|(p, h)| p.contains(BACKUP_DIR) && h == hash), "{path}: old database not in the backup");
            }
        }
    }
    no_temp_files(&lab.root);
    no_temp_files(&lab.b);
    (r.problems, r.tasks)
}

pub fn no_temp_files(root: &Path) {
    let left: Vec<String> = snap(root).into_keys().filter(|p| p.contains(".exchange-part") || p.contains(".exchange-new") || p.ends_with(".tmp")).collect();
    assert!(left.is_empty(), "left behind: {left:?}");
}

