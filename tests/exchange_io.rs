//! Exchange I/O tests on real folders: every run records each file (path, size, checksum) before and after
//! and checks the rules – nothing on the sending side changes, on the receiving side files are only added,
//! a replaced database is in Exchange-Backup, and no temporary file is left behind.
//! `MFC_IO_ROOT` puts the test folders on another file system (scripts/exchange-disks.sh: disk images).
mod common;
use common::*;
use mediafiles_check::exchange::collect::{self, DELIVERY_FILE};
use mediafiles_check::exchange::mxf_dir;
use mediafiles_check::exchange::plan::{self, Options};
use std::fs;

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
    // a second import brings nothing new: no task for a folder that got nothing
    let (problems, tasks) = import_checked(&lab, &lab.targets(&["V"]));
    assert!(problems.is_empty() && tasks.is_empty(), "{problems:?} {tasks:?}");
}

#[test]
fn own_folder_for_a_partly_present_folder_is_no_problem() {
    let lab = Lab::new("ownpart");
    lab.folder("A", "V", "1", &["a.mxf", "b.mxf", "+db"]);
    lab.folder("B", "V", "1", &["a.mxf", "+db"]);
    collect_checked(&lab, &Options::default());
    let mut t = lab.targets(&["V"]);
    t.own_folder.insert("V/1".into());
    let (problems, tasks) = import_checked(&lab, &t);
    let own = collect::own_folder_name("A", "1");
    // only b.mxf arrives in the own folder: the database does not fit it, which is expected, not a failure
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(tasks, [format!("[B] Rebuild database: V/{own}")]);
    assert!(lab.read("B", "V", &format!("{own}/msmFMID.pmr")).is_none());
    // imported again (as after a crash): the files are there already, the task must still come
    let (problems, again) = import_checked(&lab, &t);
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(again, tasks);
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
    // sent under their own names; the receiver puts both into its drive W when importing
    collect_checked(&lab, &Options::default());
    let mut t = lab.targets(&[]);
    for v in ["V1", "V2"] {
        t.media.insert(v.into(), lab.drive("B", "W"));
    }
    let (problems, tasks) = import_checked(&lab, &t);
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
    let first = snap(&lab.b);
    let (problems, _) = import_checked(&lab, &lab.targets(&["V"]));
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(snap(&lab.b), first);
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
    // nothing new landed in V/1: its own database still fits, so no rebuild task
    assert!(tasks.is_empty(), "{tasks:?}");
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
    let before = snap(&lab.b);
    for bad in [json[..json.len() / 2].to_string(), json.replace("mediafiles-exchange-delivery", "other"), json.replace("\"V/1/b.mxf\"", "\"V/../../b.mxf\""), json.replace("\"V/1/b.mxf\"", "\"V\\\\1/b.mxf\"")] {
        fs::write(&file, bad).unwrap();
        assert!(collect::verify(&lab.dest(), &mut |_| {}).is_err());
        assert!(collect::import(&lab.dest(), &lab.targets(&["V"]), "en", &mut |_| {}).is_err());
    }
    assert_eq!(snap(&lab.b), before);
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
    let (errors, tasks) = collect::collect(&a, "B", &p, &lab.dest(), "en", &mut |_| {}).unwrap();
    assert_eq!(errors.len(), 2, "{errors:?}");
    // the sender's result page shows that the database was left out
    assert_eq!(tasks, ["[B] Rebuild database: V/1"]);
    // what is in the delivery is consistent; the database is left out (folder differs from the scan)
    assert!(collect::verify(&lab.dest(), &mut |_| {}).unwrap().is_empty());
    no_temp_files(&lab.root);
    let (problems, tasks) = import_checked(&lab, &lab.targets(&["V"]));
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(tasks, ["[B] Rebuild database: V/1"]);
    assert_eq!(snap(&mxf_dir(&lab.drive("B", "V")).join("1")).into_keys().collect::<Vec<_>>(), ["c.mxf"]);
}

// ── delivery in parts (several places) ──

#[test]
fn a_delivery_in_two_parts_is_imported_only_as_a_whole() {
    let lab = Lab::new("parts");
    lab.folder("A", "V", "1", &["a.mxf", "b.mxf", "+db"]);
    lab.folder("A", "V", "2", &["c.mxf", "+db"]);
    fs::create_dir_all(mxf_dir(&lab.drive("B", "V"))).unwrap();
    let (a, b) = (lab.scan("A"), lab.scan("B"));
    let p = plan::plan(&a, &b, &Options::default());
    // folder 1 with its database into part 1, folder 2 into part 2
    let dests = [lab.root.join("Place1/x_part1of2"), lab.root.join("Place2/x_part2of2")];
    let part_of: Vec<usize> = p.copies.iter().map(|c| usize::from(c.to_folder == "2")).collect();
    let before = snap(&lab.root.join("A"));
    let (errors, _) = collect::collect_split(&a, "B", &p, &dests, &part_of, "en", &mut |_| {}).unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(snap(&lab.root.join("A")), before);
    assert!(dests[0].join("V/1/a.mxf").exists() && dests[1].join("V/2/c.mxf").exists() && !dests[0].join("V/2").exists());
    assert!(fs::read_to_string(dests[1].join(collect::tasks_file("en"))).unwrap().starts_with("PART 2 OF 2"));
    // a format of its own: an older MediaFiles Check refuses a part instead of importing it alone
    assert!(fs::read_to_string(dests[0].join(DELIVERY_FILE)).unwrap().contains(collect::DELIVERY_PARTS_FORMAT));
    // one part alone: nothing is imported
    let e = collect::import_parts(&dests[..1], &lab.targets(&["V"]), "en", &mut |_| {}).err().unwrap();
    assert!(e.to_string().contains("part 2 of 2 is missing"), "{e}");
    assert!(collect::verify_parts(&dests[1..], &mut |_| {}).is_err());
    assert!(snap(&lab.b).is_empty());
    // both, in any order
    let both = [dests[1].clone(), dests[0].clone()];
    assert!(collect::verify_parts(&both, &mut |_| {}).unwrap().is_empty());
    let r = collect::import_parts(&both, &lab.targets(&["V"]), "en", &mut |_| {}).unwrap();
    assert!(r.problems.is_empty() && r.tasks.is_empty(), "{:?} {:?}", r.problems, r.tasks);
    for f in ["1/a.mxf", "1/b.mxf", "1/msmFMID.pmr", "2/c.mxf", "2/msmMMOB.mdb"] {
        assert!(lab.read("B", "V", f).is_some(), "{f}");
    }
    no_temp_files(&lab.root);
    no_temp_files(&lab.b);
}

#[test]
fn parts_of_two_deliveries_are_refused() {
    let lab = Lab::new("partsmix");
    lab.folder("A", "V", "1", &["a.mxf", "+db"]);
    fs::create_dir_all(mxf_dir(&lab.drive("B", "V"))).unwrap();
    let (a, b) = (lab.scan("A"), lab.scan("B"));
    let p = plan::plan(&a, &b, &Options::default());
    let one = vec![0; p.copies.len()];
    let first = [lab.root.join("P/d1_part1of2"), lab.root.join("Q/d1_part2of2")];
    collect::collect_split(&a, "B", &p, &first, &one, "en", &mut |_| {}).unwrap();
    let second = [lab.root.join("P/d2_part1of2"), lab.root.join("Q/d2_part2of2")];
    collect::collect_split(&a, "B", &p, &second, &one, "en", &mut |_| {}).unwrap();
    let e = collect::import_parts(&[first[0].clone(), second[1].clone()], &lab.targets(&["V"]), "en", &mut |_| {}).err().unwrap();
    assert!(e.to_string().contains("different deliveries"), "{e}");
    assert!(collect::import_parts(&[first[0].clone(), first[0].clone()], &lab.targets(&["V"]), "en", &mut |_| {}).is_err());
}
