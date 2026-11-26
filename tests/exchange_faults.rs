//! Exchange when things go wrong on real drives: full drive, read-only drive, drive pulled out during the import,
//! app killed during collect and import. Needs disk images – run through scripts/exchange-disks.sh
//! (`MFC_IO_ROOT` = sender + delivery, `MFC_IO_B` = receiver drive, `MFC_IO_B_IMAGE` = its image file).
//! After every fault: no file under its final name has wrong content, and running again finishes the job.
mod common;
use common::*;
use mediafiles_check::exchange::collect::{self, Delivery, DELIVERY_FILE};
use mediafiles_check::exchange::plan::{self, Options};
use mediafiles_check::exchange::{hash_file, mxf_dir};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::{env, fs};

const MB: usize = 1 << 20;

fn b_image() -> PathBuf {
    PathBuf::from(env::var_os("MFC_IO_B_IMAGE").expect("run through scripts/exchange-disks.sh"))
}

fn hdiutil(args: &[&str]) {
    let ok = Command::new("hdiutil").args(args).stdout(Stdio::null()).status().unwrap().success();
    assert!(ok, "hdiutil {args:?}");
}

/// Mounts the receiver image again (read-only or not) at `MFC_IO_B`.
fn remount_b(read_only: bool) {
    let mnt = env::var("MFC_IO_B").unwrap();
    let _ = Command::new("hdiutil").args(["detach", "-force", &mnt]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    // like /Volumes: the folder the drive hangs in is not writable, so writing to a pulled drive fails
    let parent = PathBuf::from(&mnt).parent().unwrap().to_path_buf();
    set_mode(&parent, 0o755);
    fs::create_dir_all(&mnt).unwrap();
    let img = b_image();
    let mut args = vec!["attach", "-nobrowse", "-mountpoint", &mnt, img.to_str().unwrap()];
    if read_only {
        args.insert(1, "-readonly");
    }
    hdiutil(&args);
    set_mode(&parent, 0o555);
}

fn set_mode(p: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
}

/// Sender folder "1" with `n` media files of `size` bytes (different content each) + database; empty receiver drive.
fn big(lab: &Lab, n: usize, size: usize) {
    let dir = mxf_dir(&lab.drive("A", "V")).join("1");
    fs::create_dir_all(&dir).unwrap();
    for i in 0..n {
        let mut data = vec![(i % 251) as u8; size];
        data[..8].copy_from_slice(&(i as u64).to_le_bytes());
        fs::write(dir.join(format!("clip{i:04}.mxf")), data).unwrap();
    }
    for d in DB {
        fs::write(dir.join(d), format!("A {d}")).unwrap();
    }
    fs::create_dir_all(mxf_dir(&lab.drive("B", "V"))).unwrap();
}

/// Every file of the delivery that is on B under its final name has exactly the delivered content.
fn no_wrong_file(lab: &Lab) {
    let d: Delivery = serde_json::from_slice(&fs::read(lab.dest().join(DELIVERY_FILE)).unwrap()).unwrap();
    let mxf = mxf_dir(&lab.drive("B", "V"));
    for f in &d.files {
        let p = mxf.join(f.path.split_once('/').unwrap().1);
        if p.exists() && !DB.iter().any(|db| f.path.ends_with(db)) {
            assert_eq!(hash_file(&p).unwrap(), f.hash, "{} has wrong content", p.display());
        }
    }
    // the database pair is either the old one (none here) or the delivered one – never mixed
    let dbs: Vec<bool> = DB.iter().map(|n| mxf.join("1").join(n).exists()).collect();
    assert!(dbs[0] == dbs[1], "only one database file: {dbs:?}");
}

/// Import again and check that the receiving folder is now exactly the sender's.
fn finishes(lab: &Lab) {
    let (problems, tasks) = import_checked(lab, &lab.targets(&["V"]));
    assert!(problems.is_empty() && tasks.is_empty(), "{problems:?} {tasks:?}");
    assert_eq!(snap(&mxf_dir(&lab.drive("A", "V")).join("1")), snap(&mxf_dir(&lab.drive("B", "V")).join("1")));
}

#[test]
#[ignore]
fn full_drive_is_refused_before_anything_is_written() {
    let lab = Lab::new("full");
    big(&lab, 300, MB); // 300 MB, the receiver image has 256 MB
    collect_checked(&lab, &Options::default());
    let before = snap(&lab.b);
    let Err(e) = collect::import(&lab.dest(), &lab.targets(&["V"]), "en", &mut |_| {}) else { panic!("imported into a full drive") };
    assert_eq!(e.kind(), std::io::ErrorKind::StorageFull, "{e}");
    assert_eq!(snap(&lab.b), before);
}

/// The free-space check passes, then another program fills the drive while files are copied (ENOSPC mid-copy).
#[test]
#[ignore]
fn drive_fills_up_during_the_import() {
    let lab = Lab::new("enospc");
    big(&lab, 120, MB);
    collect_checked(&lab, &Options::default());
    // outside the lab folder, on the same drive
    let filler = PathBuf::from(env::var("MFC_IO_B").unwrap()).join("filler.bin");
    let mut filled = false;
    let r = collect::import(&lab.dest(), &lab.targets(&["V"]), "en", &mut |line| {
        if !filled && line.starts_with("[40/") {
            filled = true;
            use std::io::Write;
            let mut f = fs::File::create(&filler).unwrap();
            let chunk = vec![0u8; MB];
            while f.write_all(&chunk).is_ok() {}
            let _ = f.sync_all();
        }
    });
    assert!(filled);
    match &r {
        Ok(r) => assert!(!r.problems.is_empty(), "drive full but no problem reported"),
        Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::StorageFull, "{e}"),
    }
    no_wrong_file(&lab);
    no_temp_files(&lab.b);
    fs::remove_file(&filler).unwrap();
    finishes(&lab);
}

#[test]
#[ignore]
fn read_only_drive_changes_nothing() {
    let lab = Lab::new("readonly");
    big(&lab, 5, MB);
    collect_checked(&lab, &Options::default());
    let before = snap(&lab.b);
    remount_b(true);
    let r = collect::import(&lab.dest(), &lab.targets(&["V"]), "en", &mut |_| {});
    remount_b(false);
    let r = r.unwrap();
    assert!(!r.problems.is_empty());
    // nothing arrived, so V/1 still fits its own database: no rebuild task
    assert!(r.tasks.is_empty(), "{:?}", r.tasks);
    assert_eq!(snap(&lab.b), before);
    finishes(&lab);
}

#[test]
#[ignore]
fn drive_pulled_out_during_the_import() {
    let lab = Lab::new("eject");
    big(&lab, 120, MB);
    collect_checked(&lab, &Options::default());
    let mnt = env::var("MFC_IO_B").unwrap();
    let mut pulled = false;
    let r = collect::import(&lab.dest(), &lab.targets(&["V"]), "en", &mut |line| {
        if !pulled && line.starts_with("[40/") {
            pulled = true;
            hdiutil(&["detach", "-force", &mnt]);
        }
    });
    assert!(pulled);
    // the drive is gone: problems or an error, but no panic
    if let Ok(r) = &r {
        assert!(!r.problems.is_empty());
    }
    remount_b(false);
    no_wrong_file(&lab);
    finishes(&lab);
}

/// Runs collect or import in a child process and kills it (SIGKILL) when its progress reaches `at`.
fn kill_child(lab: &Lab, what: &str, at: &str) {
    let mut child = Command::new(env::current_exe().unwrap())
        .args(["--ignored", "--exact", "child", "--nocapture", "--test-threads=1"])
        .env("MFC_CHILD", what)
        .env("MFC_CHILD_ROOT", &lab.root)
        .env("MFC_CHILD_B", &lab.b)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let out = BufReader::new(child.stdout.take().unwrap());
    let mut killed = false;
    for line in out.lines().map_while(Result::ok) {
        if line.starts_with(at) {
            child.kill().unwrap();
            killed = true;
            break;
        }
    }
    child.wait().unwrap();
    assert!(killed, "{what} ended before {at}");
}

#[test]
#[ignore]
fn child() {
    let Ok(what) = env::var("MFC_CHILD") else { return };
    let lab = Lab::open(env::var("MFC_CHILD_ROOT").unwrap().into(), env::var("MFC_CHILD_B").unwrap().into());
    let mut progress = |l: &str| println!("{l}");
    match what.as_str() {
        "collect" => {
            let (a, b) = (lab.scan("A"), lab.scan("B"));
            let p = plan::plan(&a, &b, &Options::default());
            collect::collect(&a, "B", &p, &lab.dest(), "en", &mut progress).unwrap();
        }
        _ => {
            collect::import(&lab.dest(), &lab.targets(&["V"]), "en", &mut progress).unwrap();
        }
    }
    println!("done");
}

#[test]
#[ignore]
fn app_killed_during_collect() {
    let lab = Lab::new("killcollect");
    big(&lab, 120, MB);
    let before = snap(&lab.root.join("A"));
    kill_child(&lab, "collect", "[60/");
    assert_eq!(snap(&lab.root.join("A")), before);
    assert!(!lab.dest().join(DELIVERY_FILE).exists(), "the delivery file is written last");
    // the next run continues: what was copied stays, the half-written file is written again
    collect_checked(&lab, &Options::default());
    finishes(&lab);
}

#[test]
#[ignore]
fn app_killed_during_import() {
    let lab = Lab::new("killimport");
    big(&lab, 120, MB);
    collect_checked(&lab, &Options::default());
    kill_child(&lab, "import", "[60/");
    no_wrong_file(&lab);
    finishes(&lab);
}

#[test]
#[ignore]
fn app_killed_while_the_database_is_placed() {
    let lab = Lab::new("killdb");
    big(&lab, 20, MB);
    // a big database, so the kill lands while it is copied
    let src = mxf_dir(&lab.drive("A", "V")).join("1");
    fs::write(src.join(DB[1]), vec![7u8; 60 * MB]).unwrap();
    collect_checked(&lab, &Options::default());
    kill_child(&lab, "import", "[22/");
    no_wrong_file(&lab);
    let left: Vec<_> = fs::read_dir(mxf_dir(&lab.drive("B", "V")).join("1")).unwrap().flatten().map(|e| e.file_name()).collect();
    assert!(!left.iter().any(|n| DB.iter().any(|d| n == *d)), "database placed before both files were there: {left:?}");
    finishes(&lab);
}

