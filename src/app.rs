//! App: check all drives or a chosen folder, show the differences, save them as a text report;
//! rename folders so Media Composer rebuilds their databases and rename them back afterwards.
#![cfg_attr(windows, windows_subsystem = "windows")]

use mediafiles_check::rename::{self, Added, Journal, Renamed, Status};
use mediafiles_check::projects::{self, ClipKind, MissingGroup, ScanResult};
use mediafiles_check::duplicates;
use mediafiles_check::exchange::flow::{self as exflow, Choice, Compare, Incoming, Mappings};
use mediafiles_check::exchange::{self as fy, Manifest, collect as fcollect, scan as fscan};
use mediafiles_check::rebalance::{self, Plan, Rebalanced};
use mediafiles_check::report::{self, Outcome, Report};
use mediafiles_check::{FolderReport, load_json, save_json, Gate, UPDATE_APP, check, find_volumes, log, pick, selftest, update};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

slint::include_modules!();

/// A second window that changes its height does not always repaint the new area (Slint 1.18 on macOS: the frame
/// is drawn before the resize is done and nothing asks again). Ask a few times while the resize settles.
/// Every window grows with its content: redraw after each size change, or the new area stays unpainted.
macro_rules! repaint_on_resize {
    ($w:expr) => {{
        let weak = $w.as_weak();
        $w.global::<Shared>().on_resized(move || weak.upgrade().iter().for_each(repaint_soon));
    }};
}


/// (code, name in its own language, manual). New language: add it here, translations/<code>/LC_MESSAGES/mediafiles-check.po
/// and docs/help/<code>.md.
const LANGUAGES: [(&str, &str, &str); 4] = [
    ("en", "English", include_str!("../docs/help/en.md")),
    ("de", "Deutsch", include_str!("../docs/help/de.md")),
    ("fr", "Français", include_str!("../docs/help/fr.md")),
    ("es", "Español", include_str!("../docs/help/es.md")),
];
/// File names shown per finding; the saved report has all of them.
const NAMES_SHOWN: usize = 10;

#[derive(Clone)]
struct Settings {
    language: String,
    dark: bool,
    /// settings tab opened by Settings… (0 General, 1 Updates, 2 Support; About is never remembered)
    tab: i32,
    /// "Get test versions": also offer GitHub prereleases (always while a test version runs)
    betas: bool,
    /// Rebalancing: at most this many MXF files per folder
    limit: usize,
    /// not saved; shared with the threads that check and install
    update: Arc<Mutex<Update>>,
}

/// Update state as the windows show it (Shared.update-*).
#[derive(Default)]
struct Update {
    available: Option<update::Release>,
    /// 0 nothing, 1 checking, 2 up to date, 3 installing, 4 failed
    state: i32,
    /// with state 4: 0 check, 1 download, 2 damaged, 3 replace, 4 restart
    error: i32,
    details: String,
}

impl Update {
    fn failed(&mut self, e: update::Error) {
        self.state = 4;
        (self.error, self.details) = match e {
            update::Error::Check => (0, String::new()),
            update::Error::Download => (1, String::new()),
            update::Error::Damaged => (2, String::new()),
            update::Error::Replace(d) => (3, d),
            update::Error::Restart => (4, String::new()),
        };
    }
}

fn app_dir() -> Option<PathBuf> {
    let (var, sub) = if cfg!(windows) { ("APPDATA", "MediaFiles Check") } else { ("HOME", "Library/Application Support/MediaFiles Check") };
    std::env::var_os(var).map(|b| PathBuf::from(b).join(sub))
}

fn settings_path() -> Option<PathBuf> {
    app_dir().map(|d| d.join("settings.txt"))
}

/// Which files rebalancing moved where – needed to undo it.
/// × and "Delete all" ask in a real macOS dialog first (user 2026-11-16); true = Delete clicked.
fn sure_delete(lang: &str, question: String) -> bool {
    let delete = pick(lang, ["Delete", "Löschen", "Supprimer", "Borrar"]);
    let cancel = pick(lang, ["Cancel", "Abbrechen", "Annuler", "Cancelar"]);
    #[cfg(target_os = "macos")]
    return mac_menu::ask_delete(&question, delete, cancel);
    #[cfg(not(target_os = "macos"))]
    return rfd::MessageDialog::new()
        .set_description(question)
        .set_buttons(rfd::MessageButtons::OkCancelCustom(delete.into(), cancel.into()))
        .show()
        == rfd::MessageDialogResult::Custom(delete.into());
}

fn rebalance_journal_path() -> PathBuf {
    app_dir().unwrap_or_else(|| PathBuf::from(".")).join("rebalanced.json")
}

/// Original names and media files of renamed folders – needed to rename them back.
fn journal_path() -> PathBuf {
    app_dir().unwrap_or_else(|| PathBuf::from(".")).join("renamed.json")
}

use mediafiles_check::media_composer_running;

/// settings.txt: language on the first line, "dark" or "light" on the second.
fn load_settings() -> Settings {
    let text = settings_path().and_then(|p| fs::read_to_string(p).ok()).unwrap_or_default();
    let mut lines = text.lines();
    let language = lines.next().filter(|l| LANGUAGES.iter().any(|x| x.0 == *l)).unwrap_or("en").to_string();
    let dark = lines.next() != Some("light");
    let tab = lines.next().and_then(|l| l.parse().ok()).filter(|t| matches!(t, 0..=2 | 4)).unwrap_or(0);
    let betas = lines.next() == Some("betas");
    let limit = lines.next().and_then(|l| l.parse().ok()).filter(|l| rebalance::LIMITS.contains(l)).unwrap_or(rebalance::DEFAULT_LIMIT);
    Settings { language, dark, tab, betas, limit, update: Arc::default() }
}

fn save_settings(s: &Settings) {
    // comfort only; if saving fails the defaults apply next time
    if let Some(p) = settings_path() {
        let _ = fs::create_dir_all(p.parent().unwrap());
        let _ = fs::write(p, format!("{}\n{}\n{}\n{}\n{}\n", s.language, if s.dark { "dark" } else { "light" }, s.tab, if s.betas { "betas" } else { "final" }, s.limit));
    }
}

fn apply_menus(shared: &Shared, s: &Settings) {
    let languages: Vec<MenuChoice> = LANGUAGES.iter().map(|l| MenuChoice { title: l.1.into(), checked: l.0 == s.language }).collect();
    shared.set_languages(ModelRc::new(VecModel::from(languages)));
    shared.set_appearance(ModelRc::new(VecModel::from(vec![s.dark, !s.dark])));
    let names: Vec<slint::SharedString> = LANGUAGES.iter().map(|l| l.1.into()).collect();
    shared.set_language_names(ModelRc::new(VecModel::from(names)));
    shared.set_language_index(LANGUAGES.iter().position(|l| l.0 == s.language).unwrap_or(0) as i32);
    shared.set_dark(s.dark);
    shared.set_betas(s.betas);
    shared.set_limit(s.limit as i32);
    let u = s.update.lock().unwrap();
    shared.set_version(env!("CARGO_PKG_VERSION").into());
    shared.set_update_version(u.available.as_ref().map(|r| r.tag.trim_start_matches('v').to_string()).unwrap_or_default().into());
    shared.set_update_state(u.state);
    shared.set_update_error(u.error);
    shared.set_update_details(u.details.clone().into());
}

/// Asks GitHub for a newer version; `manual` (Settings button) also reports "up to date" and errors.
fn check_for_update(app: &AppWindow, s: &Settings, manual: bool) {
    {
        let mut u = s.update.lock().unwrap();
        if u.state == 1 || u.state == 3 {
            return;
        }
        if manual {
            u.state = 1;
        }
    }
    app.global::<Shared>().invoke_refresh_update();
    let (weak, update, betas) = (app.as_weak(), s.update.clone(), s.betas);
    std::thread::spawn(move || {
        let result = update::check(&UPDATE_APP, env!("CARGO_PKG_VERSION"), betas);
        match &result {
            Ok(Some(r)) => log::write(&format!("update available: {}", r.tag)),
            Ok(None) => {}
            Err(_) => log::write("update check failed"),
        }
        {
            let mut u = update.lock().unwrap();
            if u.state == 3 {
                return;
            }
            match result {
                Ok(r) => {
                    let found = r.is_some();
                    u.available = r;
                    if manual {
                        u.state = if found { 0 } else { 2 };
                    }
                }
                Err(e) if manual => u.failed(e),
                Err(_) => {}
            }
        }
        let _ = slint::invoke_from_event_loop(move || weak.upgrade().iter().for_each(|a| a.global::<Shared>().invoke_refresh_update()));
    });
}

/// Downloads, verifies and swaps in the new version, then quits; the new version starts by itself.
fn install_update(app: &AppWindow, s: &Settings) {
    let release = {
        let mut u = s.update.lock().unwrap();
        let Some(r) = u.available.clone().filter(|_| u.state != 3) else { return };
        u.state = 3;
        r
    };
    log::write(&format!("installing update {}", release.tag));
    app.global::<Shared>().invoke_refresh_update();
    let (weak, update) = (app.as_weak(), s.update.clone());
    std::thread::spawn(move || {
        let result = update::download(&release, &UPDATE_APP).and_then(|new| update::swap_and_restart(&new));
        if let Err(e) = result {
            log::write(&format!("update failed: {e:?}"));
            update.lock().unwrap().failed(e);
            let _ = slint::invoke_from_event_loop(move || weak.upgrade().iter().for_each(|a| a.global::<Shared>().invoke_refresh_update()));
            return;
        }
        log::write("update installed, restarting");
        let _ = slint::invoke_from_event_loop(|| {
            let _ = slint::quit_event_loop();
        });
    });
}

fn volume_name(p: &std::path::Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// Connected drives with "Avid MediaFiles/MXF" and whether they are ticked.
type Drives = Rc<RefCell<Vec<(PathBuf, bool)>>>;

fn show_drives(app: &AppWindow, drives: &[(PathBuf, bool)]) {
    let rows: Vec<DriveRow> = drives.iter().map(|(p, sel)| DriveRow { name: volume_name(p).into(), selected: *sel }).collect();
    app.set_selected_count(drives.iter().filter(|d| d.1).count() as i32);
    app.set_drives(ModelRc::new(VecModel::from(rows)));
    inventory_ui::show_drives(app, drives);
}

/// Picks up drives that were connected or ejected; ticks stay, new drives come ticked.
fn drives_off_path() -> Option<PathBuf> {
    app_dir().map(|d| d.join("drives-off.txt"))
}

/// Drives switched off for checking and scanning (`key` of the name), remembered across launches.
fn drives_off() -> Vec<String> {
    drives_off_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .map(|text| text.lines().filter(|l| !l.is_empty()).map(String::from).collect())
        .unwrap_or_default()
}

fn save_drives_off(drives: &[(PathBuf, bool)]) {
    let mut off = drives_off();
    for (p, on) in drives {
        let k = fy::key(&volume_name(p));
        off.retain(|x| *x != k);
        if !on {
            off.push(k);
        }
    }
    if let Some(p) = drives_off_path() {
        let _ = fs::create_dir_all(p.parent().unwrap());
        let _ = fy::write_atomic(&p, off.join("\n").as_bytes());
    }
}

fn sync_drives(app: &AppWindow, drives: &Drives, found: Vec<PathBuf>) {
    let mut d = drives.borrow_mut();
    if found.iter().eq(d.iter().map(|x| &x.0)) {
        return;
    }
    let off = drives_off();
    *d = found.into_iter().map(|p| {
        let selected = d.iter().find(|x| x.0 == p).map_or_else(|| !off.contains(&fy::key(&volume_name(&p))), |x| x.1);
        (p, selected)
    }).collect();
    show_drives(app, &d);
    show_volumes(app, &d);
}

fn toggle_drive(app: &AppWindow, drives: &Drives, i: i32) {
    let mut d = drives.borrow_mut();
    if let Some(x) = d.get_mut(i as usize) {
        x.1 = !x.1;
    }
    save_drives_off(&d);
    show_drives(app, &d);
    show_volumes(app, &d);
}

// ── Exchange ──
// Characters that fit the content column (monospaced font: characters = width)
const NAME_CHARS: usize = 30;
const LINE_CHARS: usize = 64;
const TITLE_CHARS: usize = 40;

/// Exchange's folder in the app data (checksum cache, last manifest, drive mappings).
fn exchange_dir() -> PathBuf {
    fscan::app_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn manifest_path() -> PathBuf {
    exchange_dir().join("last-manifest.json")
}

fn group_digits(n: usize, lang: &str) -> String {
    let sep = match lang {
        "en" => ',',
        "fr" => '\u{202f}',
        _ => '.',
    };
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(sep);
        }
        out.push(c);
    }
    out
}

/// Scan list: the connected drives (same switches as the Scan tab) plus the drives of the last scan that are missing.
fn show_volumes(app: &AppWindow, drives: &[(PathBuf, bool)]) {
    let lang = app.get_language().to_string();
    let manifest = Manifest::load(&manifest_path()).ok();
    let scanned = |name: &str| {
        manifest.as_ref().and_then(|m| m.volumes.iter().find(|v| fy::key(&v.name) == fy::key(name))).map(|v| v.folders.iter().map(|f| f.files.len()).sum::<usize>())
    };
    let files = |n: Option<usize>| n.map(|n| group_digits(n, &lang)).unwrap_or_default().into();
    let online: Vec<(String, bool)> = drives.iter().map(|(p, on)| (volume_name(p), *on)).collect();
    let offline = manifest.iter().flat_map(|m| &m.volumes).map(|v| &v.name).filter(|n| !online.iter().any(|o| fy::key(&o.0) == fy::key(n)));
    let rows: Vec<VolumeRow> = online
        .iter()
        .map(|(n, on)| VolumeRow { name: shorten(n, NAME_CHARS).into(), files: files(scanned(n)), online: true, selected: *on })
        .chain(offline.map(|n| VolumeRow { name: shorten(n, NAME_CHARS).into(), files: files(scanned(n)), online: false, selected: false }))
        .collect();
    let x = app.global::<Exchange>();
    x.set_volumes(ModelRc::new(VecModel::from(rows)));
    x.set_has_manifest(manifest.is_some());
}

fn map_rows(app: &AppWindow, drives: &[Choice], with_skip: bool) -> ModelRc<MapRow> {
    let rows: Vec<MapRow> = drives
        .iter()
        .map(|d| {
            let mut options = vec![app.get_txt_choose_drive()];
            options.extend(d.options.iter().map(|o| SharedString::from(shorten(o, NAME_CHARS))));
            if with_skip {
                options.push(app.get_txt_skip());
            }
            MapRow { name: shorten(&d.name, NAME_CHARS).into(), options: ModelRc::new(VecModel::from(options)), selected: d.selected as i32 }
        })
        .collect();
    ModelRc::new(VecModel::from(rows))
}

fn strings(v: &[String], max: usize) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(v.iter().map(|s| SharedString::from(shorten(s, max))).collect::<Vec<_>>()))
}

fn show_compare(app: &AppWindow, c: &Compare) {
    let lang = app.get_language().to_string();
    let plan = c.plan();
    let (files, bytes) = exflow::media(&plan);
    let x = app.global::<Exchange>();
    x.set_remote_name(shorten(&c.remote.system, TITLE_CHARS).into());
    let fmt = if lang == "en" { "%Y-%m-%d %H:%M" } else { "%d.%m.%Y %H:%M" };
    x.set_remote_date(fy::local_time(c.remote.created, fmt).into());
    x.set_maps(map_rows(app, &c.drives, true));
    x.set_copy_files(files as i32);
    x.set_copy_size(exflow::format_size(bytes, &lang).into());
    let tasks: Vec<String> = plan.tasks.iter().map(|t| t.describe(&lang)).collect();
    x.set_tasks(strings(&tasks, 400));
    x.set_can_collect(c.ready());
    x.set_has_compare(true);
    x.set_view(1);
}

thread_local! {
    /// Exchange › Import: the window for merge / own folder per delivery folder (created on first use)
    static FOLDERS_WIN: RefCell<Option<FoldersWindow>> = const { RefCell::new(None) };
}

fn show_incoming(app: &AppWindow, i: &Incoming) {
    let (files, bytes) = i.media();
    let x = app.global::<Exchange>();
    x.set_delivery_from(shorten(&i.delivery.from, TITLE_CHARS).into());
    x.set_delivery_files(files as i32);
    x.set_delivery_size(exflow::format_size(bytes, &app.get_language()).into());
    let drives: Vec<DeliveryDrive> = map_rows(app, &i.drives, false)
        .iter()
        .zip(&i.renders)
        .map(|(m, r)| DeliveryDrive { name: m.name, options: m.options, selected: m.selected, split: r.is_some(), render: r.unwrap_or(0) as i32 })
        .collect();
    x.set_delivery_maps(ModelRc::new(VecModel::from(drives)));
    let folders: Vec<ChoiceRow> = i
        .folders
        .iter()
        .map(|(id, own)| {
            let folder = id.split_once('/').map_or(id.as_str(), |x| x.1);
            ChoiceRow { name: shorten(id, NAME_CHARS + 10).into(), own: shorten(&fcollect::own_folder_name(&i.delivery.from, folder), NAME_CHARS).into(), choice: *own as i32 }
        })
        .collect();
    let own = i.folders.iter().filter(|f| f.1).count();
    x.set_folders_merge((i.folders.len() - own) as i32);
    x.set_folders_own(own as i32);
    FOLDERS_WIN.with(|w| {
        if let Some(w) = w.borrow().as_ref() {
            w.set_heading(app.get_txt_folders_heading().replace("{}", &i.delivery.from).into());
            w.set_rows(ModelRc::new(VecModel::from(folders)));
        }
    });
    x.set_can_import(i.ready());
    x.set_has_delivery(true);
    x.set_view(3);
}

/// Time left of a scan, from the rate of the last 20 s. `tick` returns None between updates (at most ten a second,
/// the last one always), else the seconds left: -1 nothing to show, 0 still estimating. The first count is the
/// start (e.g. after a cache), so work that was already done does not make the rate look fast.
#[derive(Default)]
struct Eta {
    samples: VecDeque<(std::time::Instant, u64)>,
    last: Option<std::time::Instant>,
}

impl Eta {
    fn tick(&mut self, done: u64, total: u64) -> Option<i32> {
        let now = std::time::Instant::now();
        if done < total && self.last.is_some_and(|l| now.duration_since(l) < Duration::from_millis(100)) {
            return None;
        }
        self.last = Some(now);
        Some(self.at(now, done, total))
    }

    fn at(&mut self, now: std::time::Instant, done: u64, total: u64) -> i32 {
        if done >= total {
            return -1;
        }
        self.samples.push_back((now, done));
        while self.samples.len() > 2 && now.duration_since(self.samples[1].0) >= Duration::from_secs(20) {
            self.samples.pop_front();
        }
        let (t0, d0) = self.samples[0];
        let span = now.duration_since(t0).as_secs_f64();
        if span < 5.0 || done <= d0 {
            return 0;
        }
        ((total - done) as f64 * span / (done - d0) as f64).ceil().clamp(1.0, i32::MAX as f64) as i32
    }
}

/// Where a scan can be paused and cancelled; each place has the gate of its latest scan.
#[derive(Clone, Copy)]
enum ScanAt {
    /// Check's Working page (check, duplicates, full folders)
    Work,
}

thread_local! {
    static GATES: RefCell<[Arc<Gate>; 1]> = RefCell::default();
}

/// A fresh gate for a new scan at `at`; the old scan (if still running) keeps its own.
fn new_gate(at: ScanAt) -> Arc<Gate> {
    let g = Arc::new(Gate::default());
    GATES.with(|x| x.borrow_mut()[at as usize] = g.clone());
    g
}

fn gate(at: ScanAt) -> Arc<Gate> {
    GATES.with(|x| x.borrow()[at as usize].clone())
}

/// Only one run on the Working page at a time (user test 2026-11-16: two rebalancing runs at once moved files
/// away from each other and lost a History entry). Held by the run's thread, released when it ends.
static WORK_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct WorkGuard(slint::Weak<AppWindow>);

impl Drop for WorkGuard {
    fn drop(&mut self) {
        WORK_BUSY.store(false, std::sync::atomic::Ordering::SeqCst);
        let w = self.0.clone();
        let _ = slint::invoke_from_event_loop(move || w.upgrade().into_iter().for_each(|a| a.set_work_busy(false)));
    }
}

/// Progress of a run on the Working page from its thread: the file being read and how far into its folder
/// (at most ten updates a second, the last one always).
/// Progress of a run: (done, total, place) with the time left, like a scan.
fn work_progress(weak: slint::Weak<AppWindow>) -> impl FnMut(usize, usize, &str) {
    let mut eta = Eta::default();
    move |i, n, place| {
        let Some(left) = eta.tick((i + 1).min(n) as u64, n.max(1) as u64) else { return };
        let (w, text, value) = (weak.clone(), shorten(place, LINE_CHARS), ((i + 1) as f32 / n.max(1) as f32).min(1.0));
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(a) = w.upgrade() {
                a.set_work_text(text.into());
                a.set_work_value(value);
                a.set_work_eta(left);
            }
        });
    }
}

/// A run is going: its thread saves the journals itself, so nothing else may write them now (shows the run instead).
fn run_going(app: &AppWindow) -> bool {
    let busy = WORK_BUSY.load(std::sync::atomic::Ordering::SeqCst);
    if busy {
        log::write("refused: a run is going");
        app.invoke_select_tab(app.get_work_tab());
    }
    busy
}

fn begin_work(app: &AppWindow) -> Option<WorkGuard> {
    if WORK_BUSY.swap(true, std::sync::atomic::Ordering::SeqCst) {
        log::write("refused: another run is still going");
        app.invoke_select_tab(app.get_work_tab());
        return None;
    }
    app.set_work_busy(true);
    app.set_work_eta(-1);
    Some(WorkGuard(app.as_weak()))
}

/// Collect and import are runs too: nothing else may move files or rewrite databases meanwhile, and they don't start
/// while another run is going. Their progress is the Exchange page, not the Working page.
fn begin_exchange(app: &AppWindow) -> Option<WorkGuard> {
    if run_going(app) {
        return None;
    }
    WORK_BUSY.store(true, std::sync::atomic::Ordering::SeqCst);
    Some(WorkGuard(app.as_weak()))
}

/// Shows Pause/Cancel on Check's Working page for a scan and returns its gate.
fn work_gate(app: &AppWindow) -> Arc<Gate> {
    app.set_work_paused(false);
    app.set_work_can_pause(true);
    new_gate(ScanAt::Work)
}

/// The places macOS did not let us read at the last check (for "Ask for access again").
static DENIED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn show_denied(weak: &slint::Weak<AppWindow>, denied: Vec<PathBuf>, asked: bool) {
    let names = denied.iter().map(|p| volume_name(p)).collect::<Vec<_>>().join(", ");
    *DENIED.lock().unwrap() = denied;
    let w = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(a) = w.upgrade() {
            let x = a.global::<Access>();
            x.set_asked(asked && !names.is_empty());
            x.set_denied(names.into());
            x.set_asking(false);
        }
    });
}

/// In a scan's worker thread, before it reads: if macOS does not let us read one of `places`, the note with
/// "Ask for access again" shows and the scan is cancelled (it would find nothing there).
fn check_access(weak: &slint::Weak<AppWindow>, places: &[PathBuf], gate: &Gate) {
    let denied = mediafiles_check::denied(places);
    if !denied.is_empty() {
        log::write(&format!("  no access (macOS privacy): {}", denied.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")));
        gate.cancel();
    }
    show_denied(weak, denied, false);
}

/// "Ask for access again": macOS asks only once per app, so its answer is reset (tccutil, file access only) and
/// the places are read again – that brings up the question. Still blocked: System Settings opens.
fn ask_access(weak: slint::Weak<AppWindow>) {
    std::thread::spawn(move || {
        for service in ["SystemPolicyRemovableVolumes", "SystemPolicyNetworkVolumes", "SystemPolicyDocumentsFolder", "SystemPolicyDesktopFolder", "SystemPolicyDownloadsFolder"] {
            let r = std::process::Command::new("/usr/bin/tccutil").args(["reset", service, UPDATE_APP.bundle_id]).output();
            if let Err(e) = r {
                log::write(&format!("tccutil {service}: {e}"));
            }
        }
        let places = DENIED.lock().unwrap().clone();
        let still = mediafiles_check::denied(&places);
        log::write(&format!("access asked again: {} of {} still blocked", still.len(), places.len()));
        if !still.is_empty() {
            let _ = std::process::Command::new("open").arg("x-apple.systempreferences:com.apple.preference.security?Privacy_FilesAndFolders").spawn();
        }
        show_denied(&weak, still, true);
    });
}

/// Progress from a worker thread to the page.
fn progress_to(weak: &slint::Weak<AppWindow>) -> impl FnMut(&str) + Send + 'static {
    let weak = weak.clone();
    move |line: &str| {
        let (text, value) = (line.to_string(), exflow::progress_value(line));
        let w = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(a) = w.upgrade() {
                let x = a.global::<Exchange>();
                x.set_work_text(shorten(&text, LINE_CHARS).into());
                x.set_work_value(value);
            }
        });
    }
}

fn show_exchange_result(app: &AppWindow, kind: i32, path: Option<&Path>, problems: &[String], tasks: &[String]) {
    let x = app.global::<Exchange>();
    x.set_busy(false);
    x.set_result_kind(kind);
    x.set_result_path(path.map(|p| p.display().to_string()).unwrap_or_default().into());
    x.set_result_problems(strings(problems, 400));
    x.set_result_tasks(strings(tasks, 400));
    x.set_view(if kind <= 1 { 2 } else { 3 });
    app.set_page(Page::Exchange);
}

fn start_work(app: &AppWindow, kind: i32) {
    let x = app.global::<Exchange>();
    x.set_work_kind(kind);
    x.set_work_value(0.0);
    x.set_work_text("".into());
    x.set_result_kind(-1);
    x.set_busy(true);
    x.set_view(if kind == 0 { 2 } else { 3 });
}

/// Wires the "Exchange" tab (compare, collect, import).
fn wire_exchange(app: &AppWindow, drives: &Drives, settings: &Rc<RefCell<Settings>>, renames: &Rc<RefCell<Renames>>) {
    let x = app.global::<Exchange>();
    x.set_system_name(shorten(&fscan::computer_name(), TITLE_CHARS).into());
    show_volumes(app, &drives.borrow());
    let compare: Rc<RefCell<Option<Compare>>> = Rc::default();
    let incoming: Rc<RefCell<Option<Incoming>>> = Rc::default();
    let err = |app: &AppWindow, e: String| app.global::<Exchange>().set_error(shorten(&e, LINE_CHARS * 2).into());

    x.on_save_manifest({
        let weak = app.as_weak();
        move || {
            let Ok(m) = Manifest::load(&manifest_path()) else { return };
            let dialog = rfd::FileDialog::new().set_file_name(fy::manifest_file_name(&m)).add_filter("MediaFiles Manifest", &[fy::MANIFEST_EXT]);
            if let Some(target) = dialog.save_file()
                && let Err(e) = fs::copy(manifest_path(), &target)
            {
                err(&weak.unwrap(), format!("{}: {e}", target.display()));
            }
        }
    });

    x.on_open_manifest({
        let (weak, compare) = (app.as_weak(), compare.clone());
        move || {
            let app = weak.unwrap();
            let x = app.global::<Exchange>();
            let Some(file) = rfd::FileDialog::new().add_filter("MediaFiles Manifest", &[fy::MANIFEST_EXT]).pick_file() else { return };
            x.set_view(1);
            // a request saved under Projects › Request: files wanted by their Avid IDs
            if fy::request::is_request(&file) {
                match Manifest::load(&manifest_path()).and_then(|l| fy::request::Request::load(&file).map(|r| (l, r))) {
                    Ok((local, req)) => {
                        let c = Compare::from_request(local, &req);
                        x.set_error("".into());
                        x.set_is_request(true);
                        show_compare(&app, &c);
                        *compare.borrow_mut() = Some(c);
                    }
                    Err(e) => err(&app, e.to_string()),
                }
                return;
            }
            match Manifest::load(&manifest_path()).and_then(|l| Manifest::load(&file).map(|r| (l, r))) {
                Ok((local, remote)) if fy::key(&local.system) == fy::key(&remote.system) => {
                    err(&app, format!("{}: {}", remote.system, app.get_txt_same_system()));
                }
                Ok((local, remote)) => {
                    let c = Compare::new(local, remote, &Mappings::load(&exchange_dir()));
                    x.set_error("".into());
                    x.set_is_request(false);
                    show_compare(&app, &c);
                    *compare.borrow_mut() = Some(c);
                }
                Err(e) => err(&app, e.to_string()),
            }
        }
    });

    x.on_map_changed({
        let (weak, compare) = (app.as_weak(), compare.clone());
        move |row, value| {
            let mut c = compare.borrow_mut();
            let Some(c) = c.as_mut() else { return };
            c.drives[row as usize].selected = value as usize;
            show_compare(&weak.unwrap(), c);
        }
    });

    let tasks_win: Win<LinesWindow> = Rc::default();
    x.on_show_tasks({
        let (weak, settings) = (app.as_weak(), settings.clone());
        move || {
            let app = weak.unwrap();
            let x = app.global::<Exchange>();
            if tasks_win.borrow().is_none() {
                *tasks_win.borrow_mut() = LinesWindow::new().ok();
                if let Some(t) = tasks_win.borrow().as_ref() {
                    repaint_on_resize!(t);
                    forward_menu(&t.global::<Shared>(), &weak);
                    let tw = t.as_weak();
                    t.on_close(move || { let _ = tw.unwrap().hide(); });
                }
            }
            if let Some(t) = tasks_win.borrow().as_ref() {
                t.set_heading(app.get_txt_tasks_for().replace("{}", &x.get_remote_name()).into());
                t.set_lines(x.get_tasks());
                apply_menus(&t.global::<Shared>(), &settings.borrow());
                let _ = t.show();
            }
        }
    });

    x.on_collect({
        let (weak, compare) = (app.as_weak(), compare.clone());
        move || {
            let app = weak.unwrap();
            let c = compare.borrow();
            let Some(c) = c.as_ref() else { return };
            // the databases are copied as they are on the drive: Media Composer must not write them meanwhile
            if media_composer_running() {
                err(&app, app.get_txt_quit_mc().to_string());
                return;
            }
            let Some(parent) = rfd::FileDialog::new().set_title(app.get_txt_choose_target()).pick_folder() else { return };
            let Some(work) = begin_exchange(&app) else { return };
            let mut m = Mappings::load(&exchange_dir());
            c.remember(&mut m);
            m.save(&exchange_dir());
            let (local, to, plan, dest, lang) = (c.local.clone(), c.remote.system.clone(), c.plan(), c.delivery_dir(&parent), app.get_language().to_string());
            log::write(&format!("collect for {to} into {}", dest.display()));
            start_work(&app, 0);
            let (weak, mut progress) = (weak.clone(), progress_to(&weak));
            std::thread::spawn(move || {
                let _work = work;
                let result = fcollect::collect(&local, &to, &plan, &dest, &lang, &mut progress);
                log::write(&format!("  collect: {:?}", result.as_ref().map(|e| e.0.len())));
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(app) = weak.upgrade() else { return };
                    // the delivery's tasks, with those collect added (a database left out because its folder changed)
                    let planned: Vec<String> = plan.tasks.iter().map(|t| t.describe(&lang)).collect();
                    let tasks = result.as_ref().map_or(&planned, |r| &r.1);
                    add_tasks(tasks, &app.get_txt_task_for().replace("{}", &to), &fscan::computer_name());
                    app.global::<Tasks>().invoke_reload();
                    match &result {
                        Ok((errors, _)) => show_exchange_result(&app, if errors.is_empty() { 0 } else { 1 }, Some(&dest), errors, tasks),
                        Err(e) => show_exchange_result(&app, 1, Some(&dest), &[e.to_string()], tasks),
                    }
                });
            });
        }
    });

    x.on_choose_delivery({
        let (weak, incoming) = (app.as_weak(), incoming.clone());
        move || {
            let app = weak.unwrap();
            let Some(folder) = rfd::FileDialog::new().set_title(app.get_txt_choose_delivery()).pick_folder() else { return };
            match Incoming::open(folder, &Mappings::load(&exchange_dir())) {
                // collected for another system: its folders and databases would land on the wrong drives (user 2026-11-25: refuse)
                Ok(i) if fy::key(&i.delivery.to) != fy::key(&fscan::computer_name()) => {
                    err(&app, app.get_txt_not_for_us().replacen("{}", &i.delivery.to, 1).replacen("{}", &fscan::computer_name(), 1));
                }
                Ok(i) => {
                    app.global::<Exchange>().set_error("".into());
                    show_incoming(&app, &i);
                    *incoming.borrow_mut() = Some(i);
                }
                Err(e) => err(&app, e.to_string()),
            }
        }
    });

    x.on_delivery_map_changed({
        let (weak, incoming) = (app.as_weak(), incoming.clone());
        move |row, value| {
            let mut i = incoming.borrow_mut();
            let Some(i) = i.as_mut() else { return };
            i.drives[row as usize].selected = value as usize;
            show_incoming(&weak.unwrap(), i);
        }
    });

    x.on_delivery_split_changed({
        let (weak, incoming) = (app.as_weak(), incoming.clone());
        move |row, on| {
            let mut i = incoming.borrow_mut();
            let Some(i) = i.as_mut() else { return };
            i.renders[row as usize] = on.then_some(0);
            show_incoming(&weak.unwrap(), i);
        }
    });

    x.on_delivery_render_changed({
        let (weak, incoming) = (app.as_weak(), incoming.clone());
        move |row, value| {
            let mut i = incoming.borrow_mut();
            let Some(i) = i.as_mut() else { return };
            i.renders[row as usize] = Some(value as usize);
            show_incoming(&weak.unwrap(), i);
        }
    });

    x.on_show_folders({
        let (weak, incoming, settings) = (app.as_weak(), incoming.clone(), settings.clone());
        move || {
            let app = weak.unwrap();
            FOLDERS_WIN.with(|w| {
                if w.borrow().is_none() {
                    *w.borrow_mut() = FoldersWindow::new().ok();
                    if let Some(f) = w.borrow().as_ref() {
                        repaint_on_resize!(f);
                        forward_menu(&f.global::<Shared>(), &weak);
                        let fw = f.as_weak();
                        f.on_close(move || { let _ = fw.unwrap().hide(); });
                        let main = weak.clone();
                        f.on_changed(move |row, value| main.unwrap().global::<Exchange>().invoke_delivery_folder_changed(row, value));
                    }
                }
                if let Some(f) = w.borrow().as_ref() {
                    apply_menus(&f.global::<Shared>(), &settings.borrow());
                }
            });
            if let Some(i) = incoming.borrow().as_ref() {
                show_incoming(&app, i);
            }
            FOLDERS_WIN.with(|w| w.borrow().iter().for_each(|f| { let _ = f.show(); }));
        }
    });

    x.on_delivery_folder_changed({
        let (weak, incoming) = (app.as_weak(), incoming.clone());
        move |row, value| {
            let mut i = incoming.borrow_mut();
            let Some(i) = i.as_mut() else { return };
            for (n, f) in i.folders.iter_mut().enumerate() {
                if row < 0 || n == row as usize {
                    f.1 = value == 1;
                }
            }
            show_incoming(&weak.unwrap(), i);
        }
    });

    x.on_start_import({
        let (weak, incoming, renames) = (app.as_weak(), incoming.clone(), renames.clone());
        move || {
            let app = weak.unwrap();
            if media_composer_running() {
                err(&app, app.get_txt_quit_mc().to_string());
                return;
            }
            // while folders are renamed for Media Composer nothing else rewrites databases
            if renames_open(&renames.borrow()) {
                err(&app, app.get_txt_renames_open().to_string());
                return;
            }
            let i = incoming.borrow();
            let Some(i) = i.as_ref() else { return };
            let mut m = Mappings::load(&exchange_dir());
            i.remember(&mut m);
            m.save(&exchange_dir());
            let (dest, targets, from, me, lang) = (i.dir.clone(), i.targets(), i.delivery.from.clone(), i.delivery.to.clone(), app.get_language().to_string());
            let Some(work) = begin_exchange(&app) else { return };
            log::write(&format!("import {}", dest.display()));
            start_work(&app, 1);
            let (weak, mut progress) = (weak.clone(), progress_to(&weak));
            std::thread::spawn(move || {
                let _work = work;
                // everything is verified first; only a complete delivery is imported
                let problems = match fcollect::verify(&dest, &mut progress) {
                    Ok(p) => p,
                    Err(e) => vec![e.to_string()],
                };
                if !problems.is_empty() {
                    log::write(&format!("  verify failed: {} problems", problems.len()));
                    let _ = slint::invoke_from_event_loop(move || weak.upgrade().into_iter().for_each(|a| show_exchange_result(&a, 4, Some(&dest), &problems, &[])));
                    return;
                }
                let w = weak.clone();
                let _ = slint::invoke_from_event_loop(move || w.upgrade().into_iter().for_each(|a| { a.global::<Exchange>().set_work_kind(2); a.global::<Exchange>().set_work_value(0.0); }));
                let result = fcollect::import(&dest, &targets, &lang, &mut progress);
                log::write(&format!("  import: {:?}", result.as_ref().map(|r| (r.problems.len(), r.tasks.len()))));
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(app) = weak.upgrade() else { return };
                    if let Ok(r) = &result {
                        add_tasks(&r.tasks, &app.get_txt_task_from().replace("{}", &from), &me);
                        app.global::<Tasks>().invoke_reload();
                    }
                    match result {
                        Ok(r) => show_exchange_result(&app, if r.problems.is_empty() { 2 } else { 3 }, None, &r.problems, &r.tasks),
                        Err(e) => show_exchange_result(&app, 3, None, &[e.to_string()], &[]),
                    }
                });
            });
        }
    });

    x.on_show_result({
        let weak = app.as_weak();
        move || {
            let path = weak.unwrap().global::<Exchange>().get_result_path().to_string();
            let _ = std::process::Command::new("open").arg(path).spawn();
        }
    });

    x.on_close_result({
        let (weak, compare, incoming, drives) = (app.as_weak(), compare.clone(), incoming.clone(), drives.clone());
        move || {
            let app = weak.unwrap();
            let x = app.global::<Exchange>();
            // a finished import ends the delivery; a finished collection the comparison
            if x.get_result_kind() >= 2 {
                *incoming.borrow_mut() = None;
                x.set_has_delivery(false);
                x.set_view(3);
            } else {
                *compare.borrow_mut() = None;
                x.set_has_compare(false);
                x.set_view(1);
            }
            x.set_result_kind(-1);
            show_volumes(&app, &drives.borrow());
        }
    });
}

// ── Tasks ──
/// A task saved from a delivery (collected here or imported here); tasks.json in the app folder.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct TaskEntry {
    text: String,
    source: String,
    /// for this system (its name in brackets at the start of the text)
    mine: bool,
    /// "Rebuild database" for a folder of this system: the folder, to check it
    #[serde(default)]
    folder: Option<PathBuf>,
    done: bool,
}

fn tasks_path() -> Option<PathBuf> {
    app_dir().map(|d| d.join("tasks.json"))
}

fn load_tasks() -> Vec<TaskEntry> {
    let all: Vec<TaskEntry> = tasks_path().and_then(|p| fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    // only this workstation's tasks (user 2026-11-16); older lists also held the other side's, they go with the delivery (Tasks.txt)
    all.into_iter().filter(|t| t.mine).collect()
}

fn save_tasks(t: &[TaskEntry]) {
    if let Some(p) = tasks_path() {
        let _ = fs::create_dir_all(p.parent().unwrap());
        if let Err(e) = fy::write_atomic(&p, &serde_json::to_vec_pretty(t).unwrap_or_default()) {
            log::write(&format!("tasks.json not saved: {e}"));
        }
    }
}

/// Tasks of a delivery ("[system] text"), each once: the same open text is not added again. `me` = this system's
/// name as the delivery knows it (an imported delivery names its recipient).
fn add_tasks(texts: &[String], source: &str, me: &str) {
    let me = format!("[{me}]");
    let mut all = load_tasks();
    for t in texts {
        if all.iter().any(|x| !x.done && x.text == *t) {
            continue;
        }
        let mine = t.starts_with(&me);
        if !mine {
            continue;
        }
        // "… : <drive>/<folder>" – a folder of this system that exists here
        // after "[system] <task>: " – drive and folder names may contain ": " themselves
        let folder = t.strip_prefix(&me).and_then(|r| r.split_once(": ")).and_then(|(_, p)| p.split_once('/')).map(|(v, f)| fy::mxf_dir(&Path::new("/Volumes").join(v)).join(f)).filter(|p| mine && p.is_dir());
        all.push(TaskEntry { text: t.clone(), source: source.to_string(), mine, folder, done: false });
    }
    save_tasks(&all);
}

/// What a task's button does: check the folder of a "Rebuild database" task of this system.
type TaskAct = Option<PathBuf>;

/// Exchange › Tasks: the saved tasks of deliveries, open ones first (`finish` counts them in this order).
fn show_tasks(app: &AppWindow, acts: &RefCell<Vec<TaskAct>>) {
    let all = load_tasks();
    let mut rows = vec![];
    let mut a = vec![];
    for t in all.iter().filter(|t| !t.done) {
        rows.push(TaskRow { text: t.text.clone().into(), source: t.source.clone().into(), action: if t.folder.is_some() { app.get_txt_task_check() } else { "".into() }, stored: true, mine: t.mine });
        a.push(t.folder.clone());
    }
    let done: Vec<TaskRow> = all.iter().filter(|t| t.done).map(|t| TaskRow { text: t.text.clone().into(), source: t.source.clone().into(), action: "".into(), stored: true, mine: t.mine }).collect();
    let g = app.global::<Tasks>();
    g.set_open(ModelRc::new(VecModel::from(rows)));
    g.set_done(ModelRc::new(VecModel::from(done)));
    *acts.borrow_mut() = a;
}

/// Wires Exchange › Tasks. Index i of `finish` counts the saved open tasks, which come first in the list.
fn wire_tasks(app: &AppWindow, last: &Last) -> Rc<RefCell<Vec<TaskAct>>> {
    let acts: Rc<RefCell<Vec<TaskAct>>> = Rc::default();
    let g = app.global::<Tasks>();
    let set_done = |i: i32, done: bool| {
        let mut all = load_tasks();
        if let Some(t) = all.iter_mut().filter(|t| t.done != done).nth(i as usize) {
            t.done = done;
        }
        save_tasks(&all);
    };
    g.on_finish({
        let (weak, acts) = (app.as_weak(), acts.clone());
        move |i| {
            set_done(i, true);
            show_tasks(&weak.unwrap(), &acts);
        }
    });
    g.on_reopen({
        let (weak, acts) = (app.as_weak(), acts.clone());
        move |i| {
            set_done(i, false);
            show_tasks(&weak.unwrap(), &acts);
        }
    });
    g.on_clear_done({
        let (weak, acts) = (app.as_weak(), acts.clone());
        move || {
            let mut all = load_tasks();
            all.retain(|t| !t.done);
            save_tasks(&all);
            show_tasks(&weak.unwrap(), &acts);
        }
    });
    g.on_reload({
        let (weak, acts) = (app.as_weak(), acts.clone());
        move || show_tasks(&weak.unwrap(), &acts)
    });
    g.on_action({
        let (weak, acts, last) = (app.as_weak(), acts.clone(), last.clone());
        move |i| {
            let app = weak.unwrap();
            if let Some(Some(folder)) = acts.borrow().get(i as usize) {
                start_check(&app, vec![folder.clone()], last.clone());
            }
        }
    });
    show_tasks(app, &acts);
    acts
}

/// A picture of the manual (`![…](images/<lang>/x.png)`, path relative to docs/help): in the app bundle under
/// Resources/help, when run from the repo in docs/help.
fn help_picture(path: &str) -> slint::Image {
    let bundled = std::env::current_exe().ok().and_then(|e| Some(e.parent()?.parent()?.join("Resources/help").join(path)));
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/help").join(path);
    bundled.filter(|p| p.is_file()).or(Some(repo)).and_then(|p| slint::Image::load_from_path(&p).ok()).unwrap_or_default()
}

/// Splits the manual into blocks: "# " = subtitle, "## " = heading, every paragraph its own block
/// (StyledText knows neither headings nor paragraph spacing), a line "![…](path)" a picture. Backticks go, so
/// everything stays in the app font.
fn help_blocks(markdown: &str) -> (String, Vec<HelpBlock>) {
    let mut subtitle = String::new();
    let mut blocks = vec![];
    let mut paragraph = String::new();
    let flush = |paragraph: &mut String, blocks: &mut Vec<HelpBlock>| {
        // "<folder>" would otherwise be an HTML tag
        let text = paragraph.trim().replace('`', "").replace('<', "‹").replace('>', "›");
        if !text.is_empty() {
            let body = slint::StyledText::from_markdown(&text).unwrap_or_else(|_| slint::StyledText::from_plain_text(&text));
            blocks.push(HelpBlock { heading: "".into(), body, picture: Default::default() });
        }
        paragraph.clear();
    };
    for line in markdown.lines() {
        if let Some(t) = line.strip_prefix("# ") {
            subtitle = t.to_uppercase();
        } else if let Some(h) = line.strip_prefix("## ") {
            flush(&mut paragraph, &mut blocks);
            blocks.push(HelpBlock { heading: h.into(), body: Default::default(), picture: Default::default() });
        } else if let Some(path) = line.trim().strip_prefix("![").and_then(|l| l.split_once("](")).and_then(|(_, p)| p.strip_suffix(')')) {
            flush(&mut paragraph, &mut blocks);
            blocks.push(HelpBlock { heading: "".into(), body: Default::default(), picture: help_picture(path) });
        } else if line.trim().is_empty() {
            flush(&mut paragraph, &mut blocks);
        } else {
            paragraph.push_str(line);
            paragraph.push('\n');
        }
    }
    flush(&mut paragraph, &mut blocks);
    (subtitle, blocks)
}

fn update_help(help: &HelpWindow, s: &Settings) {
    let markdown = LANGUAGES.iter().find(|l| l.0 == s.language).unwrap_or(&LANGUAGES[0]).2;
    let (subtitle, blocks) = help_blocks(markdown);
    help.set_subtitle(subtitle.into());
    help.set_blocks(ModelRc::new(VecModel::from(blocks)));
    apply_menus(&help.global::<Shared>(), s);
}

/// Menu of a second window (manual, About, settings) forwards to the main window.
fn forward_menu(shared: &Shared, weak: &slint::Weak<AppWindow>) {
    let w = weak.clone();
    shared.on_select_language(move |i| w.upgrade().into_iter().for_each(|a| a.global::<Shared>().invoke_select_language(i)));
    let w = weak.clone();
    shared.on_select_dark(move |d| w.upgrade().into_iter().for_each(|a| a.global::<Shared>().invoke_select_dark(d)));
    let w = weak.clone();
    shared.on_select_limit(move |l| w.upgrade().into_iter().for_each(|a| a.global::<Shared>().invoke_select_limit(l)));
    let w = weak.clone();
    shared.on_select_betas(move |b| w.upgrade().into_iter().for_each(|a| a.global::<Shared>().invoke_select_betas(b)));
    let w = weak.clone();
    shared.on_show_help(move || w.upgrade().into_iter().for_each(|a| a.global::<Shared>().invoke_show_help()));
    let w = weak.clone();
    shared.on_show_about(move || w.upgrade().into_iter().for_each(|a| a.global::<Shared>().invoke_show_about()));
    let w = weak.clone();
    shared.on_show_log(move || w.upgrade().into_iter().for_each(|a| a.global::<Shared>().invoke_show_log()));
    let w = weak.clone();
    shared.on_check_update(move || w.upgrade().into_iter().for_each(|a| a.global::<Shared>().invoke_check_update()));
    let w = weak.clone();
    shared.on_install_update(move || w.upgrade().into_iter().for_each(|a| a.global::<Shared>().invoke_install_update()));
    let w = weak.clone();
    shared.on_show_release(move || w.upgrade().into_iter().for_each(|a| a.global::<Shared>().invoke_show_release()));
}

/// Short message over the settings after sending (`Note`: success goes after 3 s, a failure stays until its ×).
fn toast(w: &slint::Weak<SettingsWindow>, outcome: &Outcome, lang: &str) {
    let (text, kind) = match outcome {
        Outcome::Sent => (pick(lang, ["Sent – thank you!", "Gesendet – danke!", "Envoyé – merci !", "Enviado, ¡gracias!"]).to_string(), 0),
        Outcome::Failed(path) if !path.is_empty() => {
            let file = Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            (pick(lang, [
                "The report could not be sent. It is saved in Downloads as “{}” – please try again later or send us the file.",
                "Der Bericht konnte nicht gesendet werden. Er liegt in „Downloads“ als „{}“ – bitte später noch einmal versuchen oder uns die Datei schicken.",
                "Le rapport n’a pas pu être envoyé. Il est enregistré dans Téléchargements sous « {} » – réessayez plus tard ou envoyez-nous le fichier.",
                "No se pudo enviar el informe. Está guardado en Descargas como «{}»: inténtalo más tarde o envíanos el archivo.",
            ]).replace("{}", &file), 1)
        }
        Outcome::Failed(_) => (pick(lang, ["The report could not be sent or saved.", "Der Bericht konnte weder gesendet noch gesichert werden.", "Le rapport n’a pu être ni envoyé ni enregistré.", "No se pudo enviar ni guardar el informe."]).to_string(), 2),
    };
    let Some(win) = w.upgrade() else { return };
    win.set_toast_kind(kind);
    win.set_toast_text(text.clone().into());
    win.set_toast_visible(true);
    // only "sent" goes away by itself; a failure stays until its × (user 2026-11-25)
    if kind != 0 {
        return;
    }
    let w = w.clone();
    slint::Timer::single_shot(Duration::from_secs(3), move || {
        // slides out; the text stays until the next toast so the box keeps its size while fading
        w.upgrade().into_iter().filter(|w| w.get_toast_text() == text.as_str()).for_each(|w| w.set_toast_visible(false));
    });
}

/// Settings → Support: self-test and problem report (same flow as Mountpoint). After a successful send the page
/// returns to the settings and shows the toast; on failure it stays, so nothing typed is lost.
fn wire_support(w: &SettingsWindow, settings: &Rc<RefCell<Settings>>) {
    let last: Rc<RefCell<Option<Report>>> = Rc::default();
    w.on_open_coffee(|| {
        let _ = std::process::Command::new("open").arg("https://buymeacoffee.com/moritzpoth").spawn();
    });
    w.on_back({
        let weak = w.as_weak();
        move || {
            let Some(w) = weak.upgrade() else { return };
            if w.get_test_phase() == 2 {
                w.set_test_phase(0);
                w.set_test_rows(ModelRc::default());
            }
            w.set_page(0);
        }
    });
    w.on_start_selftest({
        let (weak, settings, last) = (w.as_weak(), settings.clone(), last.clone());
        move || {
            let Some(w) = weak.upgrade() else { return };
            let lang = settings.borrow().language.clone();
            w.set_test_phase(1);
            w.set_test_note("".into());
            w.set_test_rows(ModelRc::default());
            let (weak, last) = (weak.clone(), last.clone());
            let (tx, rx) = std::sync::mpsc::channel::<Report>();
            let l = lang.clone();
            let progress_weak = weak.clone();
            std::thread::spawn(move || {
                let r = selftest::run(&l, &mut |done, next| {
                    let rows: Vec<TestRow> = done.iter().map(|c| TestRow {
                        title: selftest::title(&c.id, &l).into(),
                        detail: c.detail.clone().into(),
                        status: match c.status { "pass" => 0, "fail" => 1, _ => 2 },
                    }).collect();
                    let next = if next.is_empty() { String::new() } else { selftest::title(selftest::CHECKS.iter().find(|c| c.1 == next).map_or(next, |c| c.0), &l) + "…" };
                    let pw = progress_weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = pw.upgrade() {
                            w.set_test_rows(ModelRc::new(VecModel::from(rows)));
                            w.set_test_current(next.into());
                        }
                    });
                });
                let _ = tx.send(r);
            });
            // the report itself is not Send through slint's closure in every version: poll the channel
            let timer = Rc::new(slint::Timer::default());
            let t = timer.clone();
            timer.start(slint::TimerMode::Repeated, Duration::from_millis(200), move || {
                let Ok(r) = rx.try_recv() else { return };
                t.stop();
                if let Some(w) = weak.upgrade() {
                    let n = |s: &str| r.results.iter().filter(|c| c.status == s).count();
                    w.set_test_summary(pick(&lang, ["{p} passed, {f} failed, {s} skipped", "{p} bestanden, {f} fehlgeschlagen, {s} übersprungen", "{p} réussis, {f} échoués, {s} ignorés", "{p} superadas, {f} fallidas, {s} omitidas"])
                        .replace("{p}", &n("pass").to_string()).replace("{f}", &n("fail").to_string()).replace("{s}", &n("skip").to_string()).into());
                    w.set_test_failed(n("fail") > 0);
                    w.set_test_phase(2);
                }
                *last.borrow_mut() = Some(r);
            });
        }
    });
    w.on_save_selftest({
        let (weak, settings, last) = (w.as_weak(), settings.clone(), last.clone());
        move || {
            let Some(r) = last.borrow().clone() else { return };
            let lang = settings.borrow().language.clone();
            let note = match report::save(&r) {
                Some(p) => {
                    let _ = std::process::Command::new("open").arg("-R").arg(&p).spawn();
                    pick(&lang, ["Saved as “{}” in Downloads.", "Gesichert als „{}“ in „Downloads“.", "Enregistré sous « {} » dans Téléchargements.", "Guardado como «{}» en Descargas."])
                        .replace("{}", &p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
                }
                None => pick(&lang, ["The report could not be saved.", "Der Bericht konnte nicht gesichert werden.", "Le rapport n’a pas pu être enregistré.", "No se pudo guardar el informe."]).into(),
            };
            weak.upgrade().into_iter().for_each(|w| w.set_test_note(note.clone().into()));
        }
    });
    let send = move |weak: slint::Weak<SettingsWindow>, lang: String, report: Report, on_sent: Box<dyn Fn(&SettingsWindow)>| {
        let Some(w) = weak.upgrade() else { return };
        w.set_sending(true);
        let (tx, rx) = std::sync::mpsc::channel::<Outcome>();
        std::thread::spawn(move || {
            let _ = tx.send(report::send(&report));
        });
        let timer = Rc::new(slint::Timer::default());
        let t = timer.clone();
        timer.start(slint::TimerMode::Repeated, Duration::from_millis(200), move || {
            let Ok(outcome) = rx.try_recv() else { return };
            t.stop();
            if let Some(w) = weak.upgrade() {
                w.set_sending(false);
                if outcome == Outcome::Sent {
                    on_sent(&w);
                    w.set_page(0);
                }
                toast(&weak, &outcome, &lang);
            }
        });
    };
    w.on_send_selftest({
        let (weak, settings, last) = (w.as_weak(), settings.clone(), last.clone());
        move || {
            let Some(r) = last.borrow().clone() else { return };
            send(weak.clone(), settings.borrow().language.clone(), r, Box::new(|w: &SettingsWindow| {
                w.set_test_phase(0);
                w.set_test_rows(ModelRc::default());
            }));
        }
    });
    w.on_send_problem({
        let (weak, settings) = (w.as_weak(), settings.clone());
        move || {
            let Some(w) = weak.upgrade() else { return };
            let lang = settings.borrow().language.clone();
            let r = report::problem(&w.get_report_text(), &w.get_report_contact(), w.get_report_attach(), &lang);
            send(weak.clone(), lang, r, Box::new(|w: &SettingsWindow| {
                w.set_report_text("".into());
                w.set_report_contact("".into());
            }));
        }
    });
}

type Win<T> = Rc<RefCell<Option<T>>>;

/// The windows besides the main one, created when first opened.
#[derive(Clone, Default)]
struct Others {
    help: Win<HelpWindow>,
    settings: Win<SettingsWindow>,
    media: Win<MediaWindow>,
    table: Win<MediaTableWindow>,
}

/// Language or appearance changed: every open window follows.
fn apply_all(app: &AppWindow, others: &Others, s: &Settings) {
    apply_menus(&app.global::<Shared>(), s);
    others.settings.borrow().iter().for_each(|w| apply_menus(&w.global::<Shared>(), s));
    others.help.borrow().iter().for_each(|h| update_help(h, s));
    others.media.borrow().iter().for_each(|w| apply_menus(&w.global::<Shared>(), s));
    others.table.borrow().iter().for_each(|w| apply_menus(&w.global::<Shared>(), s));
    #[cfg(target_os = "macos")]
    {
        // Slint rebuilds the menu after this event; add the item back once that is done
        let title = app.get_txt_settings_menu().to_string();
        slint::Timer::single_shot(Duration::from_millis(150), move || mac_menu::set_settings_title(&title));
    }
}

/// Rows of the check result; the file names of a finding only while it is open (`open` = finding numbers,
/// counted over all folders; user 2026-11-16: closed at first, they take a lot of room).
fn rows(reports: &[FolderReport], lang: &str, more: &str, open: &HashSet<i32>) -> Vec<ReportRow> {
    let mut rows = vec![];
    let mut n = -1;
    let row = |text: String, level, group, names: usize, is_open| ReportRow { text: text.into(), level, group, names: names as i32, open: is_open };
    for r in reports.iter().filter(|r| !r.findings.is_empty()) {
        rows.push(row(r.place.clone(), 0, -1, 0, false));
        for f in &r.findings {
            n += 1;
            let names = f.names();
            let is_open = open.contains(&n);
            rows.push(row(f.describe(r.files, lang), 1, n, names.len(), is_open));
            if !is_open {
                continue;
            }
            rows.extend(names.iter().take(NAMES_SHOWN).map(|x| row(x.clone(), 2, n, 0, false)));
            if names.len() > NAMES_SHOWN {
                rows.push(row(more.replace("{}", &(names.len() - NAMES_SHOWN).to_string()), 2, n, 0, false));
            }
        }
    }
    rows
}

/// Result of the last check: the folders that can be renamed for Media Composer (folder, new name).
#[derive(Default)]
struct Checked {
    candidates: Vec<(PathBuf, String)>,
    /// for opening and closing the file names of a finding
    reports: Vec<FolderReport>,
    open: HashSet<i32>,
}

type Last = Arc<Mutex<Checked>>;

fn show_candidates(app: &AppWindow, c: &[(PathBuf, String)]) {
    let rows: Vec<RenameRow> = c.iter().map(|(f, to)| RenameRow { from: volume_name(f).into(), to: to.clone().into() }).collect();
    app.set_rename_count(c.len() as i32);
    app.set_rename_rows(ModelRc::new(VecModel::from(rows)));
}

fn start_check(app: &AppWindow, roots: Vec<PathBuf>, last: Last) {
    app.set_error("".into());
    app.set_work_value(0.0);
    app.set_work_text("".into());
    app.set_work_eta(-1);
    let Some(work) = begin_work(app) else { return };
    app.set_work_what(0);
    app.set_page(Page::Working);
    let (weak, gate) = (app.as_weak(), work_gate(app));
    log::write(&format!("check {}", roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(", ")));
    std::thread::spawn(move || {
        // one run at a time; released when this thread ends
        let _work = work;
        check_access(&weak, &roots, &gate);
        let (progress, mut eta) = (weak.clone(), Eta::default());
        let reports = check(&roots, &mut |i, n, place| {
            let Some(left) = eta.tick(i as u64, n as u64) else { return };
            let (place, w) = (place.to_string(), progress.clone());
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(a) = w.upgrade() {
                    a.set_work_text(place.into());
                    a.set_work_value(i as f32 / n.max(1) as f32);
                    a.set_work_eta(left);
                }
            });
        }, &gate);
        if gate.is_cancelled() {
            log::write("  check cancelled");
            let _ = slint::invoke_from_event_loop(move || weak.upgrade().into_iter().for_each(|a| a.invoke_back()));
            return;
        }
        let bad: Vec<&FolderReport> = reports.iter().filter(|r| !r.findings.is_empty()).collect();
        log::write(&format!("checked {} folders, {} with differences", reports.len(), bad.len()));
        for r in bad {
            let findings: Vec<String> = r.findings.iter().map(|f| f.describe(r.files, "en")).collect();
            log::write(&format!("  {}: {}", r.path.display(), findings.join("; ")));
        }
        let computer = rename::computer_name();
        let folders: Vec<PathBuf> = reports.iter().filter(|r| !r.findings.is_empty() && rename::needs_rename(&r.path, &computer)).map(|r| r.path.clone()).collect();
        let candidates: Vec<(PathBuf, String)> = folders.iter().cloned().zip(rename::plan_names(&folders, &computer)).collect();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(app) = weak.upgrade() else { return };
            let lang = app.get_language().to_string();
            show_candidates(&app, &candidates);
            app.set_checked_folders(reports.len() as i32);
            app.set_bad_folders(reports.iter().filter(|r| !r.findings.is_empty()).count() as i32);
            app.set_work_can_pause(false);
            app.set_rows(ModelRc::new(VecModel::from(rows(&reports, &lang, &app.get_txt_more(), &HashSet::new()))));
            *last.lock().unwrap() = Checked { candidates, reports, open: HashSet::new() };
            app.set_page(Page::Result);
        });
    });
}

// after repaint_on_resize!: macros are visible from their definition on
mod inventory_ui;

fn repaint_soon<T: ComponentHandle + 'static>(c: &T) {
    for ms in [50, 200, 600, 1500] {
        let weak = c.as_weak();
        slint::Timer::single_shot(Duration::from_millis(ms), move || weak.upgrade().into_iter().for_each(|c| c.window().request_redraw()));
    }
}

/// Long names shortened in the middle, so start and end stay visible (monospaced font: characters = width).
fn shorten(s: &str, max: usize) -> String {
    let c: Vec<char> = s.chars().collect();
    if c.len() <= max {
        return s.to_string();
    }
    let head = (max - 1) / 2;
    c[..head].iter().chain(['…'].iter()).chain(c[c.len() - (max - 1 - head)..].iter()).collect()
}

/// Avid projects (tab "Projects"): chosen folder, projects with their switch, last scan.
#[derive(Default)]
struct ProjectsState {
    projects: Vec<(PathBuf, bool)>,
    result: Arc<Mutex<Option<ScanResult>>>,
    /// opened media file groups in the result list and in the AMA list
    open: HashSet<usize>,
    ama_open: HashSet<usize>,
    /// the media window (Projects › Show media…) and what it shows: 0 missing media, 1 AMA clips
    media: Win<MediaWindow>,
    media_kind: i32,
    /// filter of the missing media window (see `MediaWindow.filter`)
    filter: i32,
    /// folders switched off in the media window's folder tree: their bins are left out of every view
    hidden: HashSet<FolderKey>,
    /// folders opened in that tree (all closed after a scan)
    folders_open: HashSet<FolderKey>,
}

/// A folder of a scanned project: (project, folders from the project down; empty = the project itself).
type FolderKey = (String, Vec<String>);

/// The folders a bin lies in ("A/B/Bin.avb" → ["A", "B"]).
fn bin_folder(bin: &str) -> Vec<String> {
    let mut c: Vec<String> = bin.split('/').map(str::to_string).collect();
    c.pop();
    c
}

/// Shown unless the bin's project or one of its folders is switched off.
fn bin_visible(hidden: &HashSet<FolderKey>, (project, bin): &(String, String)) -> bool {
    let dir = bin_folder(bin);
    (0..=dir.len()).all(|n| !hidden.contains(&(project.clone(), dir[..n].to_vec())))
}

/// Tick or untick a folder; ticking it also ticks its project and every folder above it (user 2026-11-24),
/// so its bins really show up.
fn toggle_folder(hidden: &mut HashSet<FolderKey>, (project, dir): FolderKey) {
    if hidden.remove(&(project.clone(), dir.clone())) {
        for n in 0..dir.len() {
            hidden.remove(&(project.clone(), dir[..n].to_vec()));
        }
    } else {
        hidden.insert((project, dir));
    }
}

/// Folder tree of the bins with missing files (projects first, then their folders in path order), each with the
/// number of different files below it; the rows of closed folders are left out, `index` counts over all keys,
/// which come in the same order.
fn folder_rows(w: &AppWindow, by_bin: &BTreeMap<(String, String), Vec<MissingGroup>>, hidden: &HashSet<FolderKey>, open: &HashSet<FolderKey>) -> (Vec<BinFolderRow>, Vec<FolderKey>) {
    let mut below: BTreeMap<FolderKey, HashSet<mediafiles_check::avb::MobId>> = BTreeMap::new();
    for ((project, bin), groups) in by_bin {
        let dir = bin_folder(bin);
        for n in 0..=dir.len() {
            below.entry((project.clone(), dir[..n].to_vec())).or_default().extend(groups.iter().flat_map(|g| g.ids.iter().copied()));
        }
    }
    let files = |n: usize| if n == 1 { w.get_txt_one_file() } else { w.get_txt_files_n().replace("{}", &n.to_string()).into() };
    let keys: Vec<FolderKey> = below.keys().cloned().collect();
    let rows = shown_folders(&keys, open)
        .into_iter()
        .map(|(i, branch)| {
            let (project, dir) = &keys[i];
            BinFolderRow {
                level: dir.len() as i32,
                name: dir.last().unwrap_or(project).clone().into(),
                note: files(below[&keys[i]].len()),
                checked: !hidden.contains(&keys[i]),
                // switched off further up: its own tick stays, the bins are hidden anyway
                dim: (0..dir.len()).any(|n| hidden.contains(&(project.clone(), dir[..n].to_vec()))),
                branch,
                open: open.contains(&keys[i]),
                index: i as i32,
            }
        })
        .collect();
    (rows, keys)
}

/// Of the sorted folder keys: (index, has subfolders) of those whose parents are all open.
fn shown_folders(keys: &[FolderKey], open: &HashSet<FolderKey>) -> Vec<(usize, bool)> {
    let is_child = |parent: &FolderKey, k: &FolderKey| k.0 == parent.0 && k.1.len() > parent.1.len() && k.1.starts_with(&parent.1);
    keys.iter()
        .enumerate()
        .filter(|(_, (project, dir))| (0..dir.len()).all(|n| open.contains(&(project.clone(), dir[..n].to_vec()))))
        .map(|(i, k)| (i, keys.get(i + 1).is_some_and(|next| is_child(k, next))))
        .collect()
}

/// Text into the Mac clipboard (pbcopy ships with macOS).
fn copy_to_clipboard(text: &str) -> bool {
    use std::io::Write;
    let Ok(mut child) = std::process::Command::new("/usr/bin/pbcopy").stdin(std::process::Stdio::piped()).spawn() else { return false };
    let wrote = child.stdin.take().is_some_and(|mut i| i.write_all(text.as_bytes()).is_ok());
    child.wait().is_ok_and(|s| s.success()) && wrote
}

fn projects_folder_file() -> Option<PathBuf> {
    app_dir().map(|d| d.join("projects-folder.txt"))
}

/// Projects below the folder, all switched off (the user picks what to scan).
fn load_projects(w: &AppWindow, st: &mut ProjectsState, folder: &Path) {
    st.projects = projects::find_projects(folder).into_iter().map(|p| (p, false)).collect();
    w.global::<Projects>().set_folder(shorten(&folder.display().to_string(), 60).into());
    show_projects(w, st);
}

fn show_projects(w: &AppWindow, st: &ProjectsState) {
    let rows: Vec<DriveRow> = st.projects.iter().map(|(p, on)| DriveRow { name: volume_name(p).into(), selected: *on }).collect();
    let p = w.global::<Projects>();
    p.set_selected_count(st.projects.iter().filter(|p| p.1).count() as i32);
    p.set_projects(ModelRc::new(VecModel::from(rows)));
}

/// What the request asks for: every missing file (without the ones to relink) used in a bin whose folders are all
/// switched on in the media window's tree.
fn requested(r: &ScanResult, hidden: &HashSet<FolderKey>) -> Vec<projects::Missing> {
    if hidden.is_empty() {
        return r.missing.clone();
    }
    let shown: HashSet<_> = r.by_bin.iter().filter(|(k, _)| bin_visible(hidden, k)).flat_map(|(_, gs)| gs).filter(|g| !g.relink).flat_map(|g| g.ids.iter().copied()).collect();
    r.missing.iter().filter(|m| shown.contains(&m.id)).cloned().collect()
}

/// The lists of missing media files and of AMA clips (an open file shows where it is used in the bin). The media
/// window leaves out the bins of folders switched off in its folder tree – in every view and in its counts.
fn show_rows(w: &AppWindow, r: &ScanResult, st: &ProjectsState) {
    let p = w.global::<Projects>();
    p.set_has_rows(!r.by_bin.is_empty());
    p.set_request_count(requested(r, &st.hidden).len() as i32);
    p.set_folders_off(!st.hidden.is_empty());
    p.set_has_ama_rows(!r.ama_by_bin.is_empty());
    if let Some(m) = st.media.borrow().as_ref() {
        let ama = st.media_kind == 1;
        let by_bin = if ama { &r.ama_by_bin } else { &r.by_bin };
        let shown = |k: &(String, String)| bin_visible(&st.hidden, k);
        m.set_kind(st.media_kind);
        m.set_folders(ModelRc::new(VecModel::from(folder_rows(w, by_bin, &st.hidden, &st.folders_open).0)));
        m.set_folders_off(!st.hidden.is_empty());
        // different files in the shown bins: (to relink, the rest)
        let (mut relink, mut rest) = (HashSet::new(), HashSet::new());
        for g in by_bin.iter().filter(|(k, _)| shown(k)).flat_map(|(_, gs)| gs) {
            (if g.relink { &mut relink } else { &mut rest }).extend(g.ids.iter().copied());
        }
        let (missing, relink) = if st.hidden.is_empty() { (r.missing.len() as i32, r.relink as i32) } else { (rest.len() as i32, relink.len() as i32) };
        m.set_count(match (ama, st.hidden.is_empty()) {
            (true, true) => p.get_ama(),
            (true, false) => rest.len() as i32,
            (false, _) => missing,
        });
        m.set_filter_counts(ModelRc::new(VecModel::from(vec![missing + relink, relink, missing, missing])));
        m.set_filter(st.filter);
        let rows = match (ama, st.filter) {
            (true, _) => media_rows(w, r, by_bin, &st.ama_open, 0, &shown),
            (false, 3) => {
                let mut list: Vec<&projects::Missing> = r.missing.iter().filter(|m| st.hidden.is_empty() || rest.contains(&m.id)).collect();
                list.sort_by(|a, b| a.name.cmp(&b.name));
                list.iter().map(|m| file_row(r, m.name.clone(), &m.id, SharedString::new(), false, false, -1)).collect()
            }
            (false, f) => media_rows(w, r, by_bin, &st.open, f, &shown),
        };
        // longest entry per column for the double-click on a column edge (uses are indented by 16 px ≈ 3 chars)
        let fit = |f: fn(&MediaFileRow) -> usize| rows.iter().filter(|r| r.level != 0).map(f).max().unwrap_or(0) as i32;
        let fit = vec![fit(|r| r.clip.chars().count() + if r.level == 2 { 3 } else { 0 }), fit(|r| r.mxf.chars().count()), fit(|r| r.original.chars().count())];
        m.set_fit_chars(ModelRc::new(VecModel::from(fit)));
        m.set_rows(ModelRc::new(VecModel::from(rows)));
    }
}

/// A media file in the media window: clip name, its MXF file name (if a database still lists it) and its source.
fn file_row(r: &ScanResult, clip: String, id: &mediafiles_check::avb::MobId, note: SharedString, open: bool, first: bool, index: i32) -> MediaFileRow {
    let name = |m: &HashMap<_, String>| m.get(id).cloned().unwrap_or_default().into();
    MediaFileRow { level: 1, clip: clip.into(), mxf: name(&r.mxf_names), original: name(&r.originals), note, open, first, index }
}

/// The media window's list: per bin its missing files, one row each (the group's first row opens its uses).
/// Capped at whole bins; group indexes count over all bins, so they stay stable. `filter`: 0 all groups, 1 only
/// those to relink, 2 only the really missing ones; `shown_bin` leaves out whole bins.
fn media_rows(w: &AppWindow, r: &ScanResult, by_bin: &BTreeMap<(String, String), Vec<MissingGroup>>, open: &HashSet<usize>, filter: i32, shown_bin: &dyn Fn(&(String, String)) -> bool) -> Vec<MediaFileRow> {
    const MAX_ROWS: usize = 3000;
    let files = |n: usize| if n == 1 { w.get_txt_one_file() } else { w.get_txt_files_n().replace("{}", &n.to_string()).into() };
    let other = |level, clip: SharedString, note: SharedString| MediaFileRow { level, clip, note, index: -1, ..Default::default() };
    let shown = |g: &MissingGroup| match filter {
        1 => g.relink,
        2 => !g.relink,
        _ => true,
    };
    let mut rows = vec![];
    let mut index = 0;
    for ((project, bin), groups) in by_bin {
        if rows.len() >= MAX_ROWS {
            rows.push(other(2, w.get_txt_more_rows(), "".into()));
            break;
        }
        if !shown_bin(&(project.clone(), bin.clone())) || !groups.iter().any(shown) {
            index += groups.len();
            continue;
        }
        rows.push(other(0, format!("{project} / {bin}").into(), files(groups.iter().filter(|g| shown(g)).map(|g| g.files).sum())));
        for g in groups {
            if shown(g) {
                let is_open = open.contains(&index);
                let note: SharedString = if g.relink { w.get_txt_relink() } else { "".into() };
                for (j, id) in g.ids.iter().enumerate() {
                    rows.push(file_row(r, g.name.clone(), id, note.clone(), is_open, j == 0, index as i32));
                }
                if is_open {
                    for (clip, kind) in &g.uses {
                        let kind = match kind {
                            ClipKind::Master => "".into(),
                            ClipKind::Subclip => w.get_txt_subclip(),
                            ClipKind::Sequence => w.get_txt_sequence(),
                        };
                        rows.push(other(2, clip.into(), kind));
                    }
                }
            }
            index += 1;
        }
    }
    rows
}

/// The missing media of the current scan (Scan tab) on the Projects tab; None: the scan read no projects.
/// `unknown`: MXF files in no database with an unreadable header (clips using them count as missing).
fn show_project_result(w: &AppWindow, st: &mut ProjectsState, r: Option<ScanResult>, unknown: usize) {
    st.open.clear();
    st.hidden.clear();
    st.folders_open.clear();
    st.ama_open.clear();
    // its list belongs to the previous result
    if let Some(m) = st.media.borrow().as_ref() {
        let _ = m.hide();
    }
    let p = w.global::<Projects>();
    p.set_error("".into());
    let Some(r) = r else {
        *st.result.lock().unwrap() = None;
        p.set_step(0);
        return;
    };
    p.set_missing(r.missing.len() as i32);
    p.set_unmanaged(r.missing.iter().filter(|m| m.unmanaged).count() as i32);
    p.set_bins(r.bins as i32);
    p.set_clips(r.clips as i32);
    p.set_failed(r.failed.len() as i32);
    p.set_unknown(unknown as i32);
    p.set_renders(r.renders as i32);
    p.set_relink(r.relink as i32);
    p.set_ama(r.ama as i32);
    p.set_ama_covered(r.ama_covered as i32);
    p.set_no_media(r.no_media as i32);
    show_rows(w, &r, st);
    *st.result.lock().unwrap() = Some(r);
    p.set_step(2);
}

/// Wires the "Projects" tab; the saved projects folder is loaded at once.
fn wire_projects(app: &AppWindow, st: &Rc<RefCell<ProjectsState>>, settings: &Rc<RefCell<Settings>>) {
    let p = app.global::<Projects>();
    let saved = projects_folder_file().and_then(|f| fs::read_to_string(f).ok()).map(|s| PathBuf::from(s.trim()));
    if let Some(folder) = saved.filter(|f| f.is_dir()) {
        load_projects(app, &mut st.borrow_mut(), &folder);
    }
    p.on_choose_folder({
        let (ww, st) = (app.as_weak(), st.clone());
        move || {
            let w = ww.unwrap();
            // opens in the folder whose projects are listed now
            let mut dialog = rfd::FileDialog::new().set_title(w.get_txt_choose());
            if let Some(current) = projects_folder_file().and_then(|f| fs::read_to_string(f).ok()).map(|p| PathBuf::from(p.trim())).filter(|p| p.is_dir()) {
                dialog = dialog.set_directory(current);
            }
            let Some(folder) = dialog.pick_folder() else { return };
            if let Some(f) = projects_folder_file() {
                let _ = fs::create_dir_all(f.parent().unwrap());
                let _ = fs::write(f, folder.display().to_string());
            }
            load_projects(&w, &mut st.borrow_mut(), &folder);
        }
    });
    p.on_toggle({
        let (ww, st) = (app.as_weak(), st.clone());
        move |i| {
            let mut st = st.borrow_mut();
            if let Some(p) = st.projects.get_mut(i as usize) {
                p.1 = !p.1;
            }
            show_projects(&ww.unwrap(), &st);
        }
    });
    let toggle = |ama: bool| {
        let (ww, st) = (app.as_weak(), st.clone());
        move |i: i32| {
            let w = ww.unwrap();
            let mut st = st.borrow_mut();
            let set = if ama { &mut st.ama_open } else { &mut st.open };
            if !set.remove(&(i as usize)) {
                set.insert(i as usize);
            }
            if let Some(r) = st.result.lock().unwrap().as_ref() {
                show_rows(&w, r, &st);
            }
        }
    };
    p.on_toggle_group(toggle(false));
    p.on_toggle_ama(toggle(true));
    p.on_show_media({
        let (ww, st, settings) = (app.as_weak(), st.clone(), settings.clone());
        move |kind| {
            let app = ww.unwrap();
            let win = st.borrow().media.clone();
            if win.borrow().is_none() {
                *win.borrow_mut() = MediaWindow::new().ok();
                if let Some(m) = win.borrow().as_ref() {
                    repaint_on_resize!(m);
                    forward_menu(&m.global::<Shared>(), &ww);
                    let (ww2, st2) = (ww.clone(), st.clone());
                    m.on_toggle_group(move |i| {
                        let a = ww2.unwrap();
                        let p = a.global::<Projects>();
                        if st2.borrow().media_kind == 1 { p.invoke_toggle_ama(i) } else { p.invoke_toggle_group(i) }
                    });
                    let (ww3, st3) = (ww.clone(), st.clone());
                    m.on_filter_changed(move |f| {
                        st3.borrow_mut().filter = f;
                        let s = st3.borrow();
                        if let Some(r) = s.result.lock().unwrap().as_ref() {
                            show_rows(&ww3.unwrap(), r, &s);
                        }
                    });
                    let (ww4, st4) = (ww.clone(), st.clone());
                    m.on_toggle_folder(move |i| {
                        let w = ww4.unwrap();
                        let mut s = st4.borrow_mut();
                        let result = s.result.clone();
                        let r = result.lock().unwrap();
                        let Some(r) = r.as_ref() else { return };
                        let by_bin = if s.media_kind == 1 { &r.ama_by_bin } else { &r.by_bin };
                        let Some(key) = folder_rows(&w, by_bin, &s.hidden, &s.folders_open).1.into_iter().nth(i as usize) else { return };
                        toggle_folder(&mut s.hidden, key);
                        show_rows(&w, r, &s);
                    });
                    let (ww5, st5) = (ww.clone(), st.clone());
                    m.on_select_folders(move |mode| {
                        let w = ww5.unwrap();
                        let mut s = st5.borrow_mut();
                        let result = s.result.clone();
                        let r = result.lock().unwrap();
                        let Some(r) = r.as_ref() else { return };
                        let by_bin = if s.media_kind == 1 { &r.ama_by_bin } else { &r.by_bin };
                        let keys = folder_rows(&w, by_bin, &s.hidden, &s.folders_open).1;
                        // 0 all on, 1 all off, 2 every tick the other way
                        s.hidden = match mode {
                            0 => HashSet::new(),
                            1 => keys.into_iter().collect(),
                            _ => keys.into_iter().filter(|k| !s.hidden.contains(k)).collect(),
                        };
                        show_rows(&w, r, &s);
                    });
                    let (ww6, st6) = (ww.clone(), st.clone());
                    m.on_open_folder(move |i| {
                        let w = ww6.unwrap();
                        let mut s = st6.borrow_mut();
                        let result = s.result.clone();
                        let r = result.lock().unwrap();
                        let Some(r) = r.as_ref() else { return };
                        let by_bin = if s.media_kind == 1 { &r.ama_by_bin } else { &r.by_bin };
                        let Some(key) = folder_rows(&w, by_bin, &s.hidden, &s.folders_open).1.into_iter().nth(i as usize) else { return };
                        if !s.folders_open.remove(&key) {
                            s.folders_open.insert(key);
                        }
                        show_rows(&w, r, &s);
                    });
                    let mw = m.as_weak();
                    m.on_copy(move |text| {
                        if copy_to_clipboard(&text) {
                            mw.unwrap().set_copied(text);
                        }
                    });
                    let mw = m.as_weak();
                    m.on_close(move || { let _ = mw.unwrap().hide(); });
                }
            }
            st.borrow_mut().media_kind = kind;
            {
                let s = st.borrow();
                if let Some(r) = s.result.lock().unwrap().as_ref() {
                    show_rows(&app, r, &s);
                }
            }
            if let Some(m) = win.borrow().as_ref() {
                apply_menus(&m.global::<Shared>(), &settings.borrow());
                let _ = m.show();
            }
        }
    });
    p.on_save_request({
        let (ww, st) = (app.as_weak(), st.clone());
        move || {
            let w = ww.unwrap();
            let system = rename::computer_name();
            let name = format!("{}_{}_Request.mfexchange", chrono::Local::now().format("%y%m%d-%H%M"), system.replace(['/', '\\', ':'], "_"));
            let Some(target) = rfd::FileDialog::new().set_file_name(name).add_filter("MediaFiles Request", &[fy::MANIFEST_EXT]).save_file() else { return };
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let data = {
                let st = st.borrow();
                let r = st.result.lock().unwrap();
                r.as_ref().map(|r| projects::request_json(&system, now, &requested(r, &st.hidden))).unwrap_or_default()
            };
            let result = fs::write(&target, data);
            log::write(&format!("request saved {}: {result:?}", target.display()));
            if let Err(e) = result {
                w.global::<Projects>().set_error(format!("{}: {e}", target.display()).into());
            }
        }
    });
}

fn notice(kind: i32, a: &str, b: &str, n: usize) -> NoticeRow {
    NoticeRow { kind, a: a.into(), b: b.into(), n: n as i32 }
}

/// Renaming back (user 2026-11-16): one review of every folder, one decision for all, single folders can be
/// left out. Each item: the entry, the new files since renaming, files missing or changed, database rewritten.
struct BackFlow {
    items: Vec<(Renamed, Vec<String>, usize, bool)>,
    on: Vec<bool>,
}

/// Rebalancing state: the groups found (and whether ticked), the Media Composer folders still to ask about,
/// the new names given, the groups just done (for "Start").
#[derive(Default)]
struct Rebal {
    plans: Vec<(Plan, bool)>,
    ask: VecDeque<usize>,
    names: HashMap<usize, String>,
    done: Vec<(PathBuf, String)>,
}

/// Media Composer updates folders with these names itself ("1" local, "<computer>.<n>" on network drives).
fn is_mc_folder(mxf: &Path, name: &str, computer: &str) -> bool {
    rename::is_avid_name(name, rename::is_network(mxf), computer)
}

/// Folders renamed for Media Composer right now: (MXF folder, name) for the original AND the current name – a group
/// is blocked under either (the current one, e.g. "2", is a normal subfolder for the search otherwise).
fn renamed_away(st: &Renames) -> Vec<(PathBuf, String)> {
    st.journal
        .as_ref()
        .map(|j| j.entries.iter().flat_map(|e| [(e.mxf.clone(), e.original.clone()), (e.mxf.clone(), e.current.clone())]).collect())
        .unwrap_or_default()
}

/// "Start" for these history entries: set the databases of every changed folder aside and rename those Media
/// Composer does not update itself. Marked done in the journal afterwards; a folder that is renamed already (gone
/// under its name) is left alone, so it can run again after the app stopped half-way.
fn rewrite_runs(st: &mut Renames, which: impl Fn(&Rebalanced) -> bool) -> Vec<NoticeRow> {
    let path = rebalance_journal_path();
    let mut journal = match load_json::<rebalance::Journal>(&path) {
        Ok(j) => j,
        Err(e) => return vec![notice(3, "rebalanced.json", &e.to_string(), 0)],
    };
    let (mut rows, mut to_rename) = (vec![], vec![]);
    for r in journal.entries.iter_mut().filter(|r| !r.db_done && which(r)) {
        if !r.belongs() {
            rows.push(notice(3, &r.mxf.join(&r.original).display().to_string(), "the drive there is not the one that was rebalanced – connect that drive", 0));
            continue;
        }
        let errors = rows.iter().filter(|x| x.kind == 3).count();
        for name in r.changed.clone() {
            let folder = r.mxf.join(&name);
            if folder.is_dir() {
                rewrite_original(&folder, &st.computer, &mut rows, &mut to_rename);
            }
        }
        // an error keeps the run open, so "Start" stays offered
        r.db_done = rows.iter().filter(|x| x.kind == 3).count() == errors;
    }
    rows.extend(rename_for_mc(st, &to_rename));
    if let Err(e) = save_json(&journal, &path) {
        rows.push(notice(3, &path.display().to_string(), &e.to_string(), 0));
    }
    rows
}

fn drive_and_folder(folder: &Path) -> String {
    let drive = folder.parent().and_then(Path::parent).and_then(Path::parent).map(volume_name).unwrap_or_default();
    format!("{drive}: {}", volume_name(folder))
}

/// "a, b, c" or "a … z" for long lists
fn short_list(names: &[String]) -> String {
    match names {
        [first, .., last] if names.len() > 3 => format!("{first} … {last}"),
        _ => names.join(", "),
    }
}

fn show_plans(app: &AppWindow, plans: &[(Plan, bool)], lang: &str) {
    let rows: Vec<RebalRow> = plans
        .iter()
        .map(|(p, sel)| {
            let names: Vec<String> = p.folders.iter().map(|f| f.0.clone()).collect();
            let detail = if names.is_empty() {
                pick(lang, [
                    "{count} media files in {g} folders → all back into {main}",
                    "{count} Mediendateien in {g} Ordnern → alle zurück nach {main}",
                    "{count} fichiers média dans {g} dossiers → tous de retour dans {main}",
                    "{count} archivos de medios en {g} carpetas → todos de vuelta a {main}",
                ])
            } else {
                pick(lang, [
                    "{count} media files in {g} folders → {k} rebalanced folders ({names}), {stay} stay in {main} · {m} files move",
                    "{count} Mediendateien in {g} Ordnern → {k} rebalanced-Ordner ({names}), {stay} bleiben in {main} · {m} Dateien werden verschoben",
                    "{count} fichiers média dans {g} dossiers → {k} dossiers rééquilibrés ({names}), {stay} restent dans {main} · {m} fichiers déplacés",
                    "{count} archivos de medios en {g} carpetas → {k} carpetas reequilibradas ({names}), quedan {stay} en {main} · se mueven {m} archivos",
                ])
            }
            .replace("{count}", &p.count.to_string())
            .replace("{g}", &(1 + p.members.len()).to_string())
            .replace("{k}", &names.len().to_string())
            .replace("{names}", &short_list(&names))
            .replace("{stay}", &p.stays.to_string())
            .replace("{main}", &p.main)
            .replace("{m}", &p.moves.len().to_string());
            let note = if p.blocked {
                pick(lang, [
                    "A folder of it is renamed for Media Composer – rename it back first.",
                    "Ein Ordner davon ist für Media Composer umbenannt – erst zurückbenennen.",
                    "Un de ses dossiers est renommé pour Media Composer – renommez-le d’abord à l’inverse.",
                    "Una de sus carpetas está renombrada para Media Composer: primero devuélvele el nombre.",
                ])
            } else if p.mc_folder {
                pick(lang, [
                    "Media Composer's own folder – we recommend renaming it first (you will be asked).",
                    "Ordner von Media Composer selbst – wir empfehlen, ihn vorher umzubenennen (du wirst gefragt).",
                    "Dossier de Media Composer lui-même – nous recommandons de le renommer d’abord (la question vous sera posée).",
                    "Carpeta propia de Media Composer: recomendamos renombrarla antes (se te preguntará).",
                ])
            } else {
                ""
            };
            RebalRow { title: drive_and_folder(&p.mxf.join(&p.main)).into(), detail: detail.into(), note: note.into(), selected: *sel && !p.blocked, blocked: p.blocked }
        })
        .collect();
    app.set_rebalance_selected(plans.iter().filter(|p| p.1 && !p.0.blocked).count() as i32);
    app.set_rebalance_rows(ModelRc::new(VecModel::from(rows)));
}

/// Earlier rebalancing runs (History; each group can be undone).
fn show_rebalanced(app: &AppWindow, lang: &str) {
    let path = rebalance_journal_path();
    let mut journal = load_json::<rebalance::Journal>(&path).unwrap_or_default();
    // a drive mounted under another path: follow it (the main folder and its rebalanced folders identify it)
    let mut moved = false;
    for r in journal.entries.iter_mut() {
        if r.relocate() {
            log::write(&format!("rebalanced {}: drive now at {}", r.original, r.mxf.display()));
            moved = true;
        }
    }
    if moved && let Err(e) = save_json(&journal, &path) {
        log::write(&format!("rebalanced.json not saved: {e}"));
    }
    show_rebalanced_entries(app, lang, &journal.entries);
}

/// History rows from journal entries – also called by the running rebalancing after each group (its own copy,
/// so the UI never reads rebalanced.json while the run writes it).
fn show_rebalanced_entries(app: &AppWindow, lang: &str, entries: &[Rebalanced]) {
    app.set_rebalance_pending(entries.iter().filter(|r| !r.db_done).count() as i32);
    let rows: Vec<RunRow> = entries
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let mut text = pick(lang, [
                "{folder} → {k} rebalanced folders, {n} files moved",
                "{folder} → {k} rebalanced-Ordner, {n} Dateien verschoben",
                "{folder} → {k} dossiers rééquilibrés, {n} fichiers déplacés",
                "{folder} → {k} carpetas reequilibradas, {n} archivos movidos",
            ])
            .replace("{folder}", &drive_and_folder(&r.mxf.join(&r.original)))
            .replace("{k}", &r.folders.len().to_string())
            .replace("{n}", &r.files.to_string());
            if let Some(old) = &r.renamed_from {
                text += &pick(lang, [" (was {})", " (war {})", " (avant : {})", " (antes {})"]).replace("{}", old);
            }
            let detail = pick(lang, ["Rebalancing · {}", "Rebalancing · {}", "Rééquilibrage · {}", "Reequilibrado · {}"]).replace("{}", &r.date);
            RunRow { text: text.into(), detail: detail.into(), db_done: r.db_done, kind: 0, index: i as i32, date: r.date.clone().into() }
        })
        .collect();
    app.set_rebalanced(ModelRc::new(VecModel::from(rows)));
    show_history(app);
}

/// Maintain › History: rebalancing and duplicate runs together, newest first (user 2026-11-16).
fn show_history(app: &AppWindow) {
    let mut rows: Vec<RunRow> = app.get_rebalanced().iter().chain(app.get_dup_runs().iter()).collect();
    rows.sort_by(|a, b| b.date.cmp(&a.date));
    app.set_history(ModelRc::new(VecModel::from(rows)));
}

/// Removes a run from its journal without undoing it (× in the History; kind 0 rebalancing, 1 duplicates;
/// index -1 = all of both).
fn forget_runs(kind: i32, index: i32) -> Result<(), String> {
    let (rp, dp) = (rebalance_journal_path(), dup_journal_path());
    if kind == 0 || index < 0 {
        let mut j = load_json::<rebalance::Journal>(&rp).map_err(|e| e.to_string())?;
        if index < 0 { j.entries.clear() } else if (index as usize) < j.entries.len() { j.entries.remove(index as usize); }
        save_json(&j, &rp).map_err(|e| e.to_string())?;
    }
    if kind == 1 || index < 0 {
        let mut j: duplicates::Journal = load_json(&dp).map_err(|e| e.to_string())?;
        if index < 0 { j.entries.clear() } else if (index as usize) < j.entries.len() { j.entries.remove(index as usize); }
        save_json(&j, &dp).map_err(|e| e.to_string())?;
    }
    log::write(&format!("history: removed {}", if index < 0 { "all".to_string() } else { format!("{kind}/{index}") }));
    Ok(())
}

/// Next Media Composer folder to ask about (rename it first?), or run the rebalancing when all are answered.
fn ask_next(app: &AppWindow, rebal: &Rc<RefCell<Rebal>>, settings: &Rc<RefCell<Settings>>, renames: &Rc<RefCell<Renames>>) {
    let next = rebal.borrow_mut().ask.front().copied();
    match next {
        Some(i) => {
            let main = rebal.borrow().plans[i].0.main.clone();
            app.set_ask_folder(main.into());
            app.set_ask_name("".into());
            app.set_ask_error("".into());
            app.set_page(Page::Askname);
        }
        None => start_rebalance(app, rebal, settings.borrow().limit, renames),
    }
}

/// Lays out the ticked groups anew (fresh from the drive, with the new names) and moves the files.
fn start_rebalance(app: &AppWindow, rebal: &Rc<RefCell<Rebal>>, limit: usize, renames: &Rc<RefCell<Renames>>) {
    if media_composer_running() {
        app.set_mc_running(true);
        app.set_page(Page::Rebalance);
        return;
    }
    let chosen: Vec<(Plan, Option<String>)> = {
        let r = rebal.borrow();
        r.plans.iter().enumerate().filter(|(_, p)| p.1 && !p.0.blocked).map(|(i, p)| (p.0.clone(), r.names.get(&i).cloned())).collect()
    };
    let away = renamed_away(&renames.borrow());
    // folders may have been renamed for Media Composer since the search: those groups don't change now
    let chosen: Vec<(Plan, Option<String>)> = chosen
        .into_iter()
        .filter(|(p, _)| !rebalance::blocked_by(&p.main, &away.iter().filter(|a| a.0 == p.mxf).map(|a| a.1.clone()).collect::<Vec<_>>()))
        .collect();
    app.set_work_value(0.0);
    app.set_work_text("".into());
    let Some(work) = begin_work(app) else { return };
    app.set_work_what(3);
    app.set_page(Page::Working);
    let (tx, rx) = std::sync::mpsc::channel::<(Vec<(Rebalanced, rebalance::Moved)>, Vec<NoticeRow>)>();
    let mut progress = work_progress(app.as_weak());
    let total: usize = chosen.iter().map(|(p, _)| p.count).sum();
    let live = app.as_weak();
    std::thread::spawn(move || {
        // one run at a time; released when this thread ends
        let _work = work;
        let (mut done, mut errors) = (vec![], vec![]);
        let mut read = 0;
        let mut counted = |_: usize, _: usize, place: &str| {
            progress(read, total, place);
            read += 1;
        };
        let path = rebalance_journal_path();
        match load_json::<rebalance::Journal>(&path) {
            Err(e) => errors.push(notice(3, "rebalanced.json", &e.to_string(), 0)),
            Ok(mut journal) => {
                let mut reserved: HashMap<PathBuf, HashSet<String>> = HashMap::new();
                for (p, name) in &chosen {
                    // a long run: Media Composer may have been started meanwhile – the next groups wait for the next run
                    if media_composer_running() {
                        errors.push(notice(3, &p.main, "Media Composer was started – this group and the ones after it were left as they are", 0));
                        break;
                    }
                    let res = reserved.entry(p.mxf.clone()).or_insert_with(|| away.iter().filter(|a| a.0 == p.mxf).map(|a| mediafiles_check::key(&a.1)).collect());
                    let result = rebalance::plan(&p.mxf, &p.main, rebalance::per_folder(limit), name.as_deref(), p.mc_folder, res, Some(&mut counted)).and_then(|fresh| rebalance::execute(&fresh, &mut journal, &path));
                    log::write(&format!("rebalance {} (limit {limit}{}): {}", p.mxf.join(&p.main).display(), name.as_ref().map_or(String::new(), |n| format!(", renamed to {n}")), result.as_ref().map_or_else(|e| e.to_string(), |(e, m)| format!("{} moved, {} skipped, folders {:?}", m.moved, m.skipped.len(), e.folders))));
                    // History in the sidebar grows group by group, not all at the end (user 2026-11-16)
                    let (w, entries) = (live.clone(), journal.entries.clone());
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(a) = w.upgrade() {
                            show_rebalanced_entries(&a, &a.get_language(), &entries);
                        }
                    });
                    match result {
                        Ok(r) => done.push(r),
                        Err(e) => errors.push(notice(3, &drive_and_folder(&p.mxf.join(&p.main)), &e.to_string(), 0)),
                    }
                }
            }
        }
        let _ = tx.send((done, errors));
    });
    let (timer, weak, rebal) = (Rc::new(slint::Timer::default()), app.as_weak(), rebal.clone());
    let t = timer.clone();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(200), move || {
        let Ok((done, mut errors)) = rx.try_recv() else { return };
        t.stop();
        let Some(app) = weak.upgrade() else { return };
        let lang = app.get_language().to_string();
        show_rebalanced(&app, &lang);
        // one list row per group (user 2026-11-16: a long text was unreadable with many folders)
        let mut summary: Vec<RunRow> = vec![];
        for (r, m) in &done {
            let detail = pick(&lang, [
                "{n} media files moved · {k} rebalanced folders",
                "{n} Mediendateien verschoben · {k} rebalanced-Ordner",
                "{n} fichiers média déplacés · {k} dossiers rééquilibrés",
                "{n} archivos de medios movidos · {k} carpetas reequilibradas",
            ])
            .replace("{n}", &m.moved.to_string())
            .replace("{k}", &r.folders.len().to_string());
            summary.push(RunRow { text: drive_and_folder(&r.mxf.join(&r.original)).into(), detail: detail.into(), ..Default::default() });
            if !m.skipped.is_empty() {
                errors.push(notice(7, &short_list(&m.skipped), "", m.skipped.len()));
            }
            errors.extend(m.kept.iter().map(|k| notice(6, k, "", 0)));
        }
        rebal.borrow_mut().done = done.iter().map(|(r, _)| (r.mxf.clone(), r.original.clone())).collect();
        if !errors.is_empty() {
            // something went wrong or was left: show it; the groups that worked are in the History (undo / databases)
            app.set_notice(ModelRc::new(VecModel::from(errors)));
            app.set_page(Page::Notice);
            return;
        }
        app.set_rebalance_summary(ModelRc::new(VecModel::from(summary)));
        app.set_mc_running(media_composer_running());
        app.set_page(Page::Rebalanced);
    });
}

/// Which copies "Set aside" moved where – needed to undo it.
fn dup_journal_path() -> PathBuf {
    app_dir().unwrap_or_else(|| PathBuf::from(".")).join("duplicates.json")
}

/// Duplicates state: what the last search found, which groups are ticked, the run just done (for "Start").
#[derive(Default)]
struct Dups {
    found: duplicates::Found,
    selected: Vec<bool>,
    done: Option<duplicates::Run>,
}

/// at most this many groups on the page (the report lists all; the rest are set aside with the automatic choice)
const DUPS_SHOWN: usize = 300;

/// "1.4 MB" in English, "1,4 MB" in the other languages (like `exchange::flow::format_size`).
fn size_text(bytes: u64, lang: &str) -> String {
    let s = match bytes {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b => format!("{} KB", b.div_ceil(1024)),
    };
    if lang == "en" { s } else { s.replace('.', ",") }
}

/// Folders renamed for Media Composer right now (their names on the drive): copies there are not touched.
fn renamed_dirs(st: &Renames) -> Vec<PathBuf> {
    st.journal.as_ref().map(|j| j.entries.iter().map(|e| e.dir()).collect()).unwrap_or_default()
}

fn dup_blocked(g: &duplicates::Group, renamed: &[PathBuf]) -> bool {
    g.copies.iter().any(|c| c.path.parent().is_some_and(|p| renamed.iter().any(|r| r == p)))
}

fn show_dups(app: &AppWindow, d: &Dups, renamed: &[PathBuf], lang: &str) {
    let f = &d.found;
    let mut summary = if f.groups.is_empty() {
        pick(lang, [
            "No duplicates: each of the {f} media files is there only once.",
            "Keine Duplikate: Jede der {f} Mediendateien gibt es nur einmal.",
            "Aucun doublon : chacun des {f} fichiers média n’existe qu’une fois.",
            "Sin duplicados: cada uno de los {f} archivos de medios está una sola vez.",
        ])
        .replace("{f}", &f.files.to_string())
    } else {
        pick(lang, [
            "{g} media files are there more than once ({f} looked at). Setting the ticked copies aside frees {size}.",
            "{g} Mediendateien gibt es mehrfach ({f} angesehen). Die gewählten Kopien zur Seite zu legen macht {size} frei.",
            "{g} fichiers média existent en plusieurs exemplaires ({f} examinés). Mettre de côté les copies choisies libère {size}.",
            "{g} archivos de medios están más de una vez ({f} revisados). Apartar las copias elegidas libera {size}.",
        ])
        .replace("{g}", &f.groups.len().to_string())
        .replace("{f}", &f.files.to_string())
        .replace("{size}", &size_text(f.groups.iter().zip(&d.selected).filter(|(g, s)| **s && !dup_blocked(g, renamed)).map(|(g, _)| g.extra()).sum(), lang))
    };
    if f.groups.len() > DUPS_SHOWN {
        summary += &pick(lang, [" The first {n} are shown; the report lists all.", " Die ersten {n} werden gezeigt, der Bericht listet alle.", " Les {n} premiers sont affichés ; le rapport les liste tous.", " Se muestran los primeros {n}; el informe los lista todos."]).replace("{n}", &DUPS_SHOWN.to_string());
    }
    if !f.mirrors.is_empty() {
        summary += pick(lang, [" Mountpoint workspaces were left out (they show files of another drive).", " Mountpoint-Arbeitsbereiche wurden ausgelassen (sie zeigen Dateien eines anderen Laufwerks).", " Les espaces Mountpoint ont été ignorés (ils montrent les fichiers d’un autre disque).", " Se omitieron los espacios de Mountpoint (muestran archivos de otra unidad)."]);
    }
    if !f.unreadable.is_empty() {
        summary += &pick(lang, [" {n} files could not be read (see report).", " {n} Dateien waren nicht lesbar (siehe Bericht).", " {n} fichiers étaient illisibles (voir le rapport).", " {n} archivos no se pudieron leer (ver informe)."]).replace("{n}", &f.unreadable.len().to_string());
    }
    let rows: Vec<DupGroup> = f
        .groups
        .iter()
        .zip(&d.selected)
        .take(DUPS_SHOWN)
        .map(|(g, sel)| {
            let blocked = dup_blocked(g, renamed);
            let detail = pick(lang, ["{k} copies · {size} each", "{k} Kopien · je {size}", "{k} copies · {size} chacune", "{k} copias · {size} cada una"])
                .replace("{k}", &g.copies.len().to_string())
                .replace("{size}", &size_text(g.copies[g.keep].size, lang));
            let note = if blocked {
                pick(lang, ["A copy is in a folder renamed for Media Composer – rename it back first.", "Eine Kopie liegt in einem für Media Composer umbenannten Ordner – erst zurückbenennen.", "Une copie est dans un dossier renommé pour Media Composer – renommez-le d’abord à l’inverse.", "Una copia está en una carpeta renombrada para Media Composer: primero devuélvele el nombre."])
            } else if g.sizes_differ() {
                pick(lang, ["Same Media Composer ID, but different sizes – check the copies before setting them aside.", "Gleiche Media-Composer-ID, aber unterschiedliche Größen – die Kopien vor dem Zur-Seite-Legen prüfen.", "Même ID Media Composer, mais tailles différentes – vérifiez les copies avant de les mettre de côté.", "Mismo ID de Media Composer, pero tamaños distintos: revisa las copias antes de apartarlas."])
            } else if !g.by_id {
                pick(lang, ["No Media Composer ID – found by name, size and checksum.", "Ohne Media-Composer-ID – gefunden über Name, Größe und Prüfsumme.", "Sans ID Media Composer – trouvé par nom, taille et somme de contrôle.", "Sin ID de Media Composer: encontrado por nombre, tamaño y suma de control."])
            } else {
                ""
            };
            let title = g.name();
            let copies: Vec<DupCopy> = g
                .copies
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let name = c.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                    let place = c.path.parent().map(drive_and_folder).unwrap_or_default();
                    DupCopy { place: if name == title { place } else { format!("{place} · {name}") }.into(), keep: i == g.keep }
                })
                .collect();
            DupGroup { title: title.into(), detail: detail.into(), note: note.into(), selected: *sel && !blocked, blocked, copies: ModelRc::new(VecModel::from(copies)) }
        })
        .collect();
    app.set_dup_summary(summary.into());
    app.set_dup_selected(f.groups.iter().zip(&d.selected).filter(|(g, s)| **s && !dup_blocked(g, renamed)).count() as i32);
    app.set_dup_groups(ModelRc::new(VecModel::from(rows)));
}

/// Earlier "set aside" runs (History).
fn show_dup_runs(app: &AppWindow, lang: &str) {
    let runs: duplicates::Journal = load_json(&dup_journal_path()).unwrap_or_default();
    app.set_dup_pending(runs.entries.iter().filter(|r| !r.db_done).count() as i32);
    let rows: Vec<RunRow> = runs
        .entries
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let text = pick(lang, ["{n} copies set aside ({size})", "{n} Kopien zur Seite gelegt ({size})", "{n} copies mises de côté ({size})", "{n} copias apartadas ({size})"])
                .replace("{n}", &r.moved.len().to_string())
                .replace("{size}", &size_text(r.bytes(), lang));
            let detail = pick(lang, ["Duplicates · {}", "Duplikate · {}", "Doublons · {}", "Duplicados · {}"]).replace("{}", &r.date);
            RunRow { text: text.into(), detail: detail.into(), db_done: r.db_done, kind: 1, index: i as i32, date: r.date.clone().into() }
        })
        .collect();
    app.set_dup_runs(ModelRc::new(VecModel::from(rows)));
    show_history(app);
}

/// Sets the databases of these folders aside and renames those Media Composer does not update itself.
fn rewrite_folders(st: &mut Renames, folders: &[PathBuf]) -> Vec<NoticeRow> {
    let (mut rows, mut to_rename) = (vec![], vec![]);
    for f in folders.iter().filter(|f| f.is_dir()) {
        rewrite_original(f, &st.computer, &mut rows, &mut to_rename);
    }
    rows.extend(rename_for_mc(st, &to_rename));
    rows
}

/// Renames folders for Media Composer (same as the Rename page) and returns the errors as notice rows.
fn rename_for_mc(st: &mut Renames, folders: &[PathBuf]) -> Vec<NoticeRow> {
    let names = rename::plan_names(folders, &st.computer);
    let Ok(journal) = st.journal.as_mut() else { return vec![notice(3, "renamed.json", "unreadable – nothing renamed", 0)] };
    folders
        .iter()
        .zip(names)
        .filter_map(|(folder, to)| {
            let result = rename::rename_for_avid(folder, &to, journal, &journal_path());
            log::write(&format!("rename {} -> {to}: {}", folder.display(), result.as_ref().map_or_else(|e| e.to_string(), |_| "ok".into())));
            result.err().map(|e| notice(3, &volume_name(folder), &e.to_string(), 0))
        })
        .collect()
}

/// Sets the database of a full folder aside; a folder Media Composer does not update itself is renamed for it too.
fn rewrite_original(folder: &Path, computer: &str, rows: &mut Vec<NoticeRow>, to_rename: &mut Vec<PathBuf>) {
    match rebalance::set_aside_database(folder) {
        Ok(Some(b)) => {
            log::write(&format!("database of {} set aside: {}", folder.display(), b.display()));
            rows.push(notice(8, &volume_name(folder), &b.display().to_string(), 0));
        }
        Ok(None) => {}
        Err(e) => rows.push(notice(3, &volume_name(folder), &e.to_string(), 0)),
    }
    if rename::needs_rename(folder, computer) {
        to_rename.push(folder.to_path_buf());
    }
}

struct Renames {
    computer: String,
    /// Err: the journal could not be read – renaming stays off so it is never overwritten
    journal: Result<Journal, String>,
    back: Option<BackFlow>,
}

impl Renames {
    fn load() -> Renames {
        Renames { computer: rename::computer_name(), journal: load_json(&journal_path()).map_err(|e| e.to_string()), back: None }
    }
}

/// Entries that no longer match the drive (moved mount point, app stopped mid-way) – see `rename::repair`.
/// Not while renaming back: that flow holds copies of the entries.
fn repair_renamed(st: &mut Renames) {
    let Ok(journal) = st.journal.as_mut() else { return };
    if st.back.is_some() {
        return;
    }
    let before = journal.entries.len();
    let mut changed = false;
    journal.entries.retain_mut(|r| match rename::repair(r) {
        None => true,
        Some(how) => {
            log::write(&format!("renamed folder {} (was {}): {how:?}", r.dir().display(), r.original));
            changed = true;
            how == rename::Repair::Moved
        }
    });
    if (changed || journal.entries.len() != before) && let Err(e) = save_json(journal, &journal_path()) {
        log::write(&format!("journal not saved: {e}"));
    }
}

/// Folders renamed for Media Composer and not renamed back yet: nothing else may rename folders or rewrite
/// databases until then (user 2026-11-16) – an unreadable journal blocks too.
pub(crate) fn renames_open(st: &Renames) -> bool {
    st.journal.as_ref().map_or(true, |j| !j.entries.is_empty())
}

fn show_renamed(app: &AppWindow, st: &mut Renames) {
    repair_renamed(st);
    app.global::<Inventory>().set_renames_pending(renames_open(st));
    let entries = st.journal.as_ref().map(|j| j.entries.as_slice()).unwrap_or_default();
    let rows: Vec<RenamedRow> = entries
        .iter()
        .map(|r| {
            let status = match rename::status(r) {
                Status::Offline => 0,
                Status::Waiting => 1,
                Status::Ready => 2,
            };
            RenamedRow { current: r.current.clone().into(), original: r.original.clone().into(), status }
        })
        .collect();
    app.set_mc_running(!rows.is_empty() && media_composer_running());
    app.set_ready_count(rows.iter().filter(|r| r.status == 2).count() as i32);
    app.set_renamed(ModelRc::new(VecModel::from(rows)));
}

fn show_back(app: &AppWindow, flow: &BackFlow) {
    let rows: Vec<BackRow> = flow
        .items
        .iter()
        .zip(&flow.on)
        .map(|((r, added, changed, ready), on)| BackRow { current: r.current.clone().into(), original: r.original.clone().into(), ready: *ready, changed: *changed as i32, added: added.len() as i32, on: *on })
        .collect();
    app.set_back_count(flow.on.iter().filter(|o| **o).count() as i32);
    app.set_back_added(flow.items.iter().zip(&flow.on).any(|(i, on)| *on && !i.1.is_empty()));
    app.set_back_rows(ModelRc::new(VecModel::from(rows)));
}

/// Renames the ticked folders back (new files handled by `choice`); the others stay renamed.
fn finish_back(app: &AppWindow, st: &mut Renames, choice: Added) {
    let Renames { computer, journal, back } = st;
    let (Ok(journal), Some(flow)) = (journal.as_mut(), back.take()) else { return };
    let mut notes = vec![];
    for ((r, added, _, _), on) in flow.items.iter().zip(flow.on) {
        if !on {
            log::write(&format!("left renamed: {}", r.dir().display()));
            notes.push(notice(2, &r.current, "", 0));
            continue;
        }
        let result = rename::rename_back(r, added, choice, computer);
        log::write(&format!(
            "rename back {} -> {} ({} new files, {choice:?}): {}",
            r.dir().display(),
            r.original,
            added.len(),
            result.as_ref().map_or_else(|e| e.to_string(), |b| format!("{b:?}"))
        ));
        match result {
            Ok(b) => {
                notes.push(notice(0, &r.current, &r.original, 0));
                if let Some((to, n)) = b.moved_to.filter(|m| m.1 > 0) {
                    notes.push(notice(1, &to, "", n));
                }
                notes.extend(b.not_moved.iter().map(|f| notice(4, f, "", 0)));
                if let Some(e) = &b.error {
                    notes.push(notice(3, &r.original, e, 0));
                }
                journal.entries.retain(|e| e != r);
                if let Err(e) = save_json(journal, &journal_path()) {
                    notes.push(notice(3, &journal_path().display().to_string(), &e.to_string(), 0));
                }
            }
            Err(e) => notes.push(notice(3, &r.current, &e.to_string(), 0)),
        }
    }
    show_renamed(app, st);
    app.set_notice(ModelRc::new(VecModel::from(notes)));
    app.set_page(Page::Notice);
}

/// macOS app menu (Slint's winit backend builds it itself, without a Settings item):
/// "About" goes to our About window instead of the standard panel, and "Settings…" (⌘,) is added after it.
/// Slint rebuilds the menu (language change, other window in front), so the item is re-added in `setMainMenu:`.
#[cfg(target_os = "macos")]
mod mac_menu {
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
    use objc2::{class, msg_send, sel};
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicUsize, Ordering};

    type Handler = RefCell<Option<Box<dyn Fn()>>>;
    thread_local! {
        static ABOUT: Handler = RefCell::new(None);
        static SETTINGS: Handler = RefCell::new(None);
        static TITLE: RefCell<String> = RefCell::new("Settings…".into());
    }
    static ORIGINAL_SET_MAIN_MENU: AtomicUsize = AtomicUsize::new(0);
    type ObjFn = extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject);

    extern "C-unwind" fn about(_: *mut AnyObject, _: Sel, _: *mut AnyObject) {
        ABOUT.with(|h| h.borrow().iter().for_each(|f| f()));
    }

    extern "C-unwind" fn settings(_: *mut AnyObject, _: Sel, _: *mut AnyObject) {
        SETTINGS.with(|h| h.borrow().iter().for_each(|f| f()));
    }

    extern "C-unwind" fn set_main_menu(app: *mut AnyObject, sel: Sel, menu: *mut AnyObject) {
        let original = ORIGINAL_SET_MAIN_MENU.load(Ordering::Relaxed);
        if original != 0 {
            let original: ObjFn = unsafe { std::mem::transmute(original) };
            original(app, sel, menu);
        }
        ensure();
    }

    fn ns_string(s: &str) -> Retained<AnyObject> {
        let c = std::ffi::CString::new(s).unwrap_or_default();
        unsafe { msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()] }
    }

    /// App-modal warning alert in this process (rfd fell back to a CFUserNotification outside the app).
    /// First button = `delete` (destructive, Return), second = `cancel` (Esc); true = delete clicked.
    pub fn ask_delete(question: &str, delete: &str, cancel: &str) -> bool {
        unsafe {
            let alert: Retained<AnyObject> = msg_send![class!(NSAlert), new];
            let _: () = msg_send![&*alert, setAlertStyle: 0usize];
            let _: () = msg_send![&*alert, setMessageText: &*ns_string(question)];
            let ok: *mut AnyObject = msg_send![&*alert, addButtonWithTitle: &*ns_string(delete)];
            let _: () = msg_send![ok, setHasDestructiveAction: true];
            let no: *mut AnyObject = msg_send![&*alert, addButtonWithTitle: &*ns_string(cancel)];
            let _: () = msg_send![no, setKeyEquivalent: &*ns_string("\u{1b}")];
            let answer: isize = msg_send![&*alert, runModal];
            answer == 1000
        }
    }

    /// Adds "Settings…" to the app menu if it is missing and keeps its title in the current language.
    pub fn ensure() {
        unsafe {
            let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            let main: *mut AnyObject = msg_send![app, mainMenu];
            if main.is_null() {
                return;
            }
            let count: isize = msg_send![main, numberOfItems];
            if count == 0 {
                return;
            }
            let first: *mut AnyObject = msg_send![main, itemAtIndex: 0isize];
            let app_menu: *mut AnyObject = msg_send![first, submenu];
            if app_menu.is_null() {
                return;
            }
            let title = TITLE.with(|t| ns_string(&t.borrow()));
            let index: isize = msg_send![app_menu, indexOfItemWithTarget: std::ptr::null_mut::<AnyObject>(), andAction: sel!(mfcShowSettings:)];
            if index >= 0 {
                let item: *mut AnyObject = msg_send![app_menu, itemAtIndex: index];
                let _: () = msg_send![item, setTitle: &*title];
                return;
            }
            // only into the app menu Slint builds (About, separator, …); macOS sets its own short menu at launch,
            // and inserting past its end throws an Objective-C exception that aborts the app
            let items: isize = msg_send![app_menu, numberOfItems];
            if items < 3 {
                return;
            }
            let second: *mut AnyObject = msg_send![app_menu, itemAtIndex: 1isize];
            let separator_at_1: bool = msg_send![second, isSeparatorItem];
            if !separator_at_1 {
                return;
            }
            let item: Retained<AnyObject> = msg_send![class!(NSMenuItem), new];
            let _: () = msg_send![&*item, setTitle: &*title];
            let _: () = msg_send![&*item, setAction: sel!(mfcShowSettings:)];
            let _: () = msg_send![&*item, setKeyEquivalent: &*ns_string(",")];
            let separator: Retained<AnyObject> = msg_send![class!(NSMenuItem), separatorItem];
            // About, separator, here: Settings…, separator, Services …
            let _: () = msg_send![app_menu, insertItem: &*item, atIndex: 2isize];
            let _: () = msg_send![app_menu, insertItem: &*separator, atIndex: 3isize];
        }
    }

    pub fn set_settings_title(title: &str) {
        TITLE.with(|t| *t.borrow_mut() = title.to_string());
        ensure();
    }

    pub fn install(on_about: Box<dyn Fn()>, on_settings: Box<dyn Fn()>, settings_title: &str) {
        ABOUT.with(|h| *h.borrow_mut() = Some(on_about));
        SETTINGS.with(|h| *h.borrow_mut() = Some(on_settings));
        TITLE.with(|t| *t.borrow_mut() = settings_title.to_string());
        let Some(cls) = AnyClass::get(c"NSApplication") else { return };
        let imp = |f: ObjFn| unsafe { std::mem::transmute::<ObjFn, Imp>(f) };
        for sel in [sel!(orderFrontStandardAboutPanel:), sel!(orderFrontStandardAboutPanelWithOptions:)] {
            if let Some(m) = cls.instance_method(sel) {
                // same signature (self, _cmd, one object) as the methods it replaces
                unsafe { m.set_implementation(imp(about)) };
            }
        }
        // the menu item's action; with no target it reaches NSApplication through the responder chain
        unsafe { objc2::ffi::class_addMethod(cls as *const AnyClass as *mut AnyClass, sel!(mfcShowSettings:), imp(settings), c"v@:@".as_ptr()) };
        if let Some(m) = cls.instance_method(sel!(setMainMenu:)) {
            let original = unsafe { m.set_implementation(imp(set_main_menu)) };
            ORIGINAL_SET_MAIN_MENU.store(original as usize, Ordering::Relaxed);
        }
        ensure();
    }
}

/// AppKit details Slint does not offer: colours in sRGB like the Swift apps, and the settings window's title bar.
#[cfg(target_os = "macos")]
mod mac_window {
    use objc2::encode::{Encode, Encoding};
    use objc2::runtime::{AnyObject, Imp, Sel};
    use objc2::{class, msg_send, sel};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Size {
        width: f64,
        height: f64,
    }
    unsafe impl Encode for Size {
        const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Rect {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    }

    unsafe impl Encode for Rect {
        const ENCODING: Encoding = Encoding::Struct("CGRect", &[Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]), Size::ENCODING]);
    }

    /// Usable height of the screen with the key window (without menu bar and Dock), in points; 0 if unknown.
    pub fn screen_height() -> f64 {
        unsafe {
            let screen: *mut AnyObject = msg_send![class!(NSScreen), mainScreen];
            if screen.is_null() {
                return 0.0;
            }
            let frame: Rect = msg_send![screen, visibleFrame];
            frame.height
        }
    }

    type Init = extern "C-unwind" fn(*mut AnyObject, Sel, Rect, usize, usize, bool) -> *mut AnyObject;
    static ORIGINAL_INIT: AtomicUsize = AtomicUsize::new(0);

    extern "C-unwind" fn init_srgb(this: *mut AnyObject, sel: Sel, rect: Rect, style: usize, backing: usize, defer: bool) -> *mut AnyObject {
        let original: Init = unsafe { std::mem::transmute(ORIGINAL_INIT.load(Ordering::Relaxed)) };
        let win = original(this, sel, rect, style, backing, defer);
        if !win.is_null() {
            unsafe {
                let srgb: *mut AnyObject = msg_send![class!(NSColorSpace), sRGBColorSpace];
                let _: () = msg_send![win, setColorSpace: srgb];
            }
        }
        win
    }

    /// Slint (femtovg, OpenGL) draws without colour management, so #1f2225 came out as #21262a next to Mountpoint's
    /// exact #1f2225. A window tagged as sRGB before its first frame shows the Theme colours exactly like the SwiftUI
    /// apps; set later it only worked now and then – so every NSWindow gets sRGB right in its initializer.
    /// Call once, before the first window is created.
    pub fn srgb_windows() {
        let cls = class!(NSWindow);
        if let Some(m) = cls.instance_method(sel!(initWithContentRect:styleMask:backing:defer:)) {
            let imp: Imp = unsafe { std::mem::transmute(init_srgb as Init) };
            let original = unsafe { m.set_implementation(imp) };
            ORIGINAL_INIT.store(original as usize, Ordering::Relaxed);
        }
    }

    type KeyFn = extern "C-unwind" fn(*mut AnyObject, Sel);
    static ORIGINAL_BECOME_KEY: AtomicUsize = AtomicUsize::new(0);
    static ORIGINAL_RESIGN_KEY: AtomicUsize = AtomicUsize::new(0);
    thread_local! {
        static KEY_CHANGED: std::cell::RefCell<Option<Box<dyn Fn()>>> = std::cell::RefCell::new(None);
    }

    fn key_changed(original: &AtomicUsize, this: *mut AnyObject, sel: Sel) {
        let original: KeyFn = unsafe { std::mem::transmute(original.load(Ordering::Relaxed)) };
        original(this, sel);
        KEY_CHANGED.with(|f| f.borrow().iter().for_each(|f| f()));
    }
    extern "C-unwind" fn become_key(this: *mut AnyObject, sel: Sel) {
        key_changed(&ORIGINAL_BECOME_KEY, this, sel);
    }
    extern "C-unwind" fn resign_key(this: *mut AnyObject, sel: Sel) {
        key_changed(&ORIGINAL_RESIGN_KEY, this, sel);
    }

    /// Calls `f` whenever any window becomes or stops being the key window – the tab bar then changes colour at once
    /// together with the title bar (polling made it lag behind).
    pub fn on_key_change(f: Box<dyn Fn()>) {
        KEY_CHANGED.with(|k| *k.borrow_mut() = Some(f));
        let cls = class!(NSWindow);
        for (name, imp, original) in [
            (sel!(becomeKeyWindow), become_key as KeyFn, &ORIGINAL_BECOME_KEY),
            (sel!(resignKeyWindow), resign_key as KeyFn, &ORIGINAL_RESIGN_KEY),
        ] {
            if original.load(Ordering::Relaxed) != 0 {
                continue;
            }
            if let Some(m) = cls.instance_method(name) {
                let imp: Imp = unsafe { std::mem::transmute(imp) };
                original.store(unsafe { m.set_implementation(imp) } as usize, Ordering::Relaxed);
            }
        }
    }

    fn ns_window(w: &slint::Window) -> Option<*mut AnyObject> {
        let handle = w.window_handle();
        let RawWindowHandle::AppKit(h) = handle.window_handle().ok()?.as_raw() else { return None };
        let view = h.ns_view.as_ptr() as *mut AnyObject;
        let win: *mut AnyObject = unsafe { msg_send![view, window] };
        (!win.is_null()).then_some(win)
    }

    /// Content up to the top edge under a transparent title bar (like an NSToolbar window), so the tab bar and the
    /// title bar are one surface without the separator line. Keeps the content size (`height` = whole window).
    pub fn unify_titlebar(w: &slint::Window, width: f64, height: f64) {
        let Some(win) = ns_window(w) else { return };
        unsafe {
            let mask: usize = msg_send![win, styleMask];
            const FULL_SIZE_CONTENT_VIEW: usize = 1 << 15;
            const MINIATURIZABLE: usize = 1 << 2;
            if mask & FULL_SIZE_CONTENT_VIEW == 0 {
                // HIG settings: minimize and zoom dimmed (zoom already is: the size is fixed)
                let _: () = msg_send![win, setStyleMask: (mask | FULL_SIZE_CONTENT_VIEW) & !MINIATURIZABLE];
                let _: () = msg_send![win, setTitlebarAppearsTransparent: true];
                let _: () = msg_send![win, setContentSize: Size { width, height }];
            }
        }
    }

    /// Title bar, traffic lights and title text in the app's own light/dark (not the system's): every window.
    pub fn appearance_all(dark: bool) {
        let name: &std::ffi::CStr = if dark { c"NSAppearanceNameDarkAqua" } else { c"NSAppearanceNameAqua" };
        unsafe {
            let name: *mut AnyObject = msg_send![class!(NSString), stringWithUTF8String: name.as_ptr()];
            let wanted: *mut AnyObject = msg_send![class!(NSAppearance), appearanceNamed: name];
            let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            let windows: *mut AnyObject = msg_send![app, windows];
            let count: usize = msg_send![windows, count];
            for i in 0..count {
                let w: *mut AnyObject = msg_send![windows, objectAtIndex: i];
                let current: *mut AnyObject = msg_send![w, appearance];
                if current != wanted {
                    let _: () = msg_send![w, setAppearance: wanted];
                }
            }
        }
    }

    /// Whether the window is the key window (the title bar is lighter then; the tab bar follows).
    pub fn is_key(w: &slint::Window) -> bool {
        ns_window(w).is_some_and(|win| unsafe { msg_send![win, isKeyWindow] })
    }
}

/// Maintain MXFs: Duplicates, Rebalancing and their History.
fn wire_maintain(app: &AppWindow, drives: &Drives, settings: &Rc<RefCell<Settings>>, renames: &Rc<RefCell<Renames>>, others: &Others) {
    // ── Duplicates ─────────────────────────────────────────
    let dups: Rc<RefCell<Dups>> = Rc::default();
    show_dup_runs(app, &settings.borrow().language);

    app.on_find_dups({
        let (weak, drives, dups, renames) = (app.as_weak(), drives.clone(), dups.clone(), renames.clone());
        move || {
            let app = weak.unwrap();
            let roots: Vec<PathBuf> = drives.borrow().iter().filter(|d| d.1).map(|d| d.0.clone()).collect();
            app.set_work_value(0.0);
            app.set_work_text("".into());
            app.set_work_eta(-1);
            let Some(work) = begin_work(&app) else { return };
            app.set_work_what(1);
            app.set_page(Page::Working);
            log::write(&format!("duplicates: looking through {}", roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(", ")));
            let (tx, rx) = std::sync::mpsc::channel::<duplicates::Found>();
            let (progress, gate) = (app.as_weak(), work_gate(&app));
            let cancelled = gate.clone();
            std::thread::spawn(move || {
                // one run at a time; released when this thread ends
                let _work = work;
                check_access(&progress, &roots, &gate);
                let mut eta = Eta::default();
                let found = duplicates::find(&roots, &mut |i, n, place| {
                    let Some(left) = eta.tick(i as u64, n as u64) else { return };
                    let (place, w) = (place.to_string(), progress.clone());
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(a) = w.upgrade() {
                            a.set_work_text(place.into());
                            a.set_work_value(i as f32 / n.max(1) as f32);
                            a.set_work_eta(left);
                        }
                    });
                }, &gate);
                let _ = tx.send(found);
            });
            let (timer, weak, dups, renames) = (Rc::new(slint::Timer::default()), weak.clone(), dups.clone(), renames.clone());
            let t = timer.clone();
            timer.start(slint::TimerMode::Repeated, Duration::from_millis(200), move || {
                let Ok(found) = rx.try_recv() else { return };
                t.stop();
                let Some(app) = weak.upgrade() else { return };
                app.set_work_can_pause(false);
                if cancelled.is_cancelled() {
                    log::write("duplicates: cancelled");
                    app.invoke_back();
                    return;
                }
                log::write(&format!("duplicates: {} files, {} with copies, {} unreadable, {} workspaces left out", found.files, found.groups.len(), found.unreadable.len(), found.mirrors.len()));
                let mut d = dups.borrow_mut();
                // copies with different sizes are never ticked by themselves
                d.selected = found.groups.iter().map(|g| !g.sizes_differ()).collect();
                d.found = found;
                show_dups(&app, &d, &renamed_dirs(&renames.borrow()), &app.get_language());
                app.set_mc_running(media_composer_running());
                app.set_page(Page::Dups);
            });
        }
    });

    app.on_toggle_dup({
        let (weak, dups, renames) = (app.as_weak(), dups.clone(), renames.clone());
        move |i| {
            let app = weak.unwrap();
            let mut d = dups.borrow_mut();
            if let Some(s) = d.selected.get_mut(i as usize) {
                *s = !*s;
            }
            show_dups(&app, &d, &renamed_dirs(&renames.borrow()), &app.get_language());
        }
    });

    app.on_keep_copy({
        let (weak, dups, renames) = (app.as_weak(), dups.clone(), renames.clone());
        move |g, c| {
            let app = weak.unwrap();
            let mut d = dups.borrow_mut();
            if let Some(group) = d.found.groups.get_mut(g as usize).filter(|gr| (c as usize) < gr.copies.len()) {
                group.keep = c as usize;
            }
            show_dups(&app, &d, &renamed_dirs(&renames.borrow()), &app.get_language());
        }
    });

    app.on_save_dups({
        let (weak, dups) = (app.as_weak(), dups.clone());
        move || {
            let app = weak.unwrap();
            let name = format!("MediaFiles Check Duplicates {}.txt", chrono::Local::now().format("%Y-%m-%d"));
            let Some(target) = rfd::FileDialog::new().set_file_name(name).add_filter("Text", &["txt"]).save_file() else { return };
            if let Err(e) = fs::write(&target, duplicates::report_text(&dups.borrow().found, &app.get_language())) {
                app.set_notice(ModelRc::new(VecModel::from(vec![notice(3, &target.display().to_string(), &e.to_string(), 0)])));
                app.set_page(Page::Notice);
            }
        }
    });

    app.on_set_aside_dups({
        let (weak, dups, renames) = (app.as_weak(), dups.clone(), renames.clone());
        move || {
            let app = weak.unwrap();
            if media_composer_running() {
                app.set_mc_running(true);
                return;
            }
            let renamed = renamed_dirs(&renames.borrow());
            let copies: Vec<(duplicates::Copy, Vec<duplicates::Copy>)> = {
                let d = dups.borrow();
                d.found
                    .groups
                    .iter()
                    .zip(&d.selected)
                    .filter(|(g, s)| **s && !dup_blocked(g, &renamed))
                    .map(|(g, _)| (g.copies[g.keep].clone(), g.copies.iter().enumerate().filter(|(i, _)| *i != g.keep).map(|(_, c)| c.clone()).collect()))
                    .collect()
            };
            app.set_work_value(0.0);
            app.set_work_text("".into());
            let Some(work) = begin_work(&app) else { return };
            app.set_work_what(4);
            app.set_page(Page::Working);
            let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<(duplicates::Run, duplicates::Moved)>>();
            std::thread::spawn(move || {
                // one run at a time; released when this thread ends
                let _work = work;
                let _ = tx.send(duplicates::set_aside(&copies, &dup_journal_path()));
            });
            let (timer, weak, dups) = (Rc::new(slint::Timer::default()), weak.clone(), dups.clone());
            let t = timer.clone();
            timer.start(slint::TimerMode::Repeated, Duration::from_millis(200), move || {
                let Ok(result) = rx.try_recv() else { return };
                t.stop();
                let Some(app) = weak.upgrade() else { return };
                let lang = app.get_language().to_string();
                show_dup_runs(&app, &lang);
                match result {
                    Err(e) => {
                        log::write(&format!("duplicates: set aside failed: {e}"));
                        app.set_notice(ModelRc::new(VecModel::from(vec![notice(3, "duplicates", &e.to_string(), 0)])));
                        app.set_page(Page::Notice);
                    }
                    Ok((run, m)) => {
                        log::write(&format!("duplicates: {} copies set aside, {} skipped", m.moved, m.skipped.len()));
                        let mut lines = vec![pick(&lang, [
                            "{n} copies set aside ({size}) into “MediaFiles Check Duplicates” on their drives.",
                            "{n} Kopien zur Seite gelegt ({size}), in „MediaFiles Check Duplicates“ auf ihren Laufwerken.",
                            "{n} copies mises de côté ({size}) dans « MediaFiles Check Duplicates » sur leurs disques.",
                            "{n} copias apartadas ({size}) en «MediaFiles Check Duplicates» en sus unidades.",
                        ])
                        .replace("{n}", &m.moved.to_string())
                        .replace("{size}", &size_text(run.bytes(), &lang))];
                        if !m.skipped.is_empty() {
                            lines.push(pick(&lang, ["Left where they are (changed or gone meanwhile): {}", "Liegen geblieben (inzwischen geändert oder weg): {}", "Laissées en place (modifiées ou disparues entre-temps) : {}", "Se quedaron (cambiadas o desaparecidas entretanto): {}"]).replace("{}", &m.skipped.join(", ")));
                        }
                        lines.extend(run.changed.iter().map(|f| format!("· {}", drive_and_folder(f))));
                        dups.borrow_mut().done = Some(run);
                        app.set_dup_done_summary(lines.join("\n").into());
                        app.set_mc_running(media_composer_running());
                        app.set_page(Page::Dupsdone);
                    }
                }
            });
        }
    });

    // "Start" (-1 = the run just done) or "Rewrite databases" of one History entry
    app.on_forget_run({
        let weak = app.as_weak();
        move |kind, index| {
            let app = weak.unwrap();
            if run_going(&app) {
                return;
            }
            // × and "Delete all…" both ask in a macOS dialog first (user 2026-11-16)
            {
                let lang = app.get_language().to_string();
                let history = app.get_history();
                let n = history.row_count();
                let one = history.iter().find(|r| index >= 0 && r.kind == kind && r.index == index);
                let q = if let Some(r) = one {
                    pick(&lang, [
                        "Really remove “{}” from the History? Nothing on the drives changes, but the run can then no longer be undone.",
                        "Wirklich „{}“ aus dem Verlauf entfernen? Auf den Laufwerken ändert sich nichts, aber der Lauf lässt sich danach nicht mehr rückgängig machen.",
                        "Vraiment retirer « {} » de l’historique ? Rien ne change sur les disques, mais l’opération ne pourra plus être annulée.",
                        "¿Quitar de verdad «{}» del historial? En las unidades no cambia nada, pero la operación ya no se podrá deshacer.",
                    ]).replace("{}", &r.text)
                } else if n == 1 {
                    pick(&lang, [
                        "Really remove the entry from the History? Nothing on the drives changes, but the run can then no longer be undone.",
                        "Wirklich den Eintrag aus dem Verlauf entfernen? Auf den Laufwerken ändert sich nichts, aber der Lauf lässt sich danach nicht mehr rückgängig machen.",
                        "Vraiment retirer l’entrée de l’historique ? Rien ne change sur les disques, mais l’opération ne pourra plus être annulée.",
                        "¿Quitar de verdad la entrada del historial? En las unidades no cambia nada, pero la operación ya no se podrá deshacer.",
                    ]).to_string()
                } else {
                    pick(&lang, [
                        "Really remove all {n} entries from the History? Nothing on the drives changes, but these runs can then no longer be undone.",
                        "Wirklich alle {n} Einträge aus dem Verlauf entfernen? Auf den Laufwerken ändert sich nichts, aber diese Läufe lassen sich danach nicht mehr rückgängig machen.",
                        "Vraiment retirer les {n} entrées de l’historique ? Rien ne change sur les disques, mais ces opérations ne pourront plus être annulées.",
                        "¿Quitar de verdad las {n} entradas del historial? En las unidades no cambia nada, pero estas operaciones ya no se podrán deshacer.",
                    ]).replace("{n}", &n.to_string())
                };
                if !sure_delete(&lang, q) {
                    return;
                }
            }
            if let Err(e) = forget_runs(kind, index) {
                app.set_notice(ModelRc::new(VecModel::from(vec![notice(3, "History", &e, 0)])));
                app.set_page(Page::Notice);
            }
            let lang = app.get_language().to_string();
            show_rebalanced(&app, &lang);
            show_dup_runs(&app, &lang);
        }
    });

    // Rebalancing / Duplicates start page: runs left waiting (Cancel on the done page) – set aside all their databases
    app.on_rewrite_all({
        let (weak, renames) = (app.as_weak(), renames.clone());
        move || {
            let app = weak.unwrap();
            if run_going(&app) {
                return;
            }
            if media_composer_running() {
                app.set_mc_running(true);
                return;
            }
            if renames_open(&renames.borrow()) {
                log::write("refused: folders still renamed for Media Composer");
                return;
            }
            let mut st = renames.borrow_mut();
            let mut rows = rewrite_runs(&mut st, |_| true);
            let path = dup_journal_path();
            match load_json::<duplicates::Journal>(&path) {
                Err(e) => rows.push(notice(3, "duplicates.json", &e.to_string(), 0)),
                Ok(mut journal) => {
                    for run in journal.entries.iter_mut().filter(|r| !r.db_done) {
                        let new_rows = rewrite_folders(&mut st, &run.changed);
                        // an error (database not set aside, folder not renamed) keeps the run open, so "Start" stays offered
                        run.db_done = !new_rows.iter().any(|r| r.kind == 3);
                        rows.extend(new_rows);
                    }
                    if let Err(e) = save_json(&journal, &path) {
                        rows.push(notice(3, &path.display().to_string(), &e.to_string(), 0));
                    }
                }
            }
            show_renamed(&app, &mut st);
            let lang = app.get_language().to_string();
            show_rebalanced(&app, &lang);
            show_dup_runs(&app, &lang);
            app.set_notice(ModelRc::new(VecModel::from(rows)));
            app.set_notice_db(true);
            app.set_page(Page::Notice);
        }
    });

    app.on_dup_db({
        let (weak, dups, renames) = (app.as_weak(), dups.clone(), renames.clone());
        move |i| {
            let app = weak.unwrap();
            if media_composer_running() {
                app.set_mc_running(true);
                return;
            }
            if run_going(&app) {
                return;
            }
            if renames_open(&renames.borrow()) {
                log::write("refused: folders still renamed for Media Composer");
                return;
            }
            let path = dup_journal_path();
            let mut rows = vec![];
            match load_json::<duplicates::Journal>(&path) {
                Err(e) => rows.push(notice(3, "duplicates.json", &e.to_string(), 0)),
                Ok(mut journal) => {
                    let done = dups.borrow_mut().done.take();
                    let index = usize::try_from(i).ok().or_else(|| done.and_then(|d| journal.entries.iter().position(|r| *r == d)));
                    if let Some(run) = index.and_then(|i| journal.entries.get_mut(i)) {
                        rows = rewrite_folders(&mut renames.borrow_mut(), &run.changed);
                        // an error keeps the run open, so "Start" stays offered
                        run.db_done = !rows.iter().any(|r| r.kind == 3);
                        if let Err(e) = save_json(&journal, &path) {
                            rows.push(notice(3, &path.display().to_string(), &e.to_string(), 0));
                        }
                    }
                }
            }
            show_renamed(&app, &mut renames.borrow_mut());
            show_dup_runs(&app, &app.get_language());
            app.set_notice(ModelRc::new(VecModel::from(rows)));
            // the result page explains the next step and leads to Check DBs › New databases
            app.set_notice_db(true);
            app.set_page(Page::Notice);
        }
    });

    app.on_undo_dup({
        let (weak, renames) = (app.as_weak(), renames.clone());
        move |i| {
            let app = weak.unwrap();
            if media_composer_running() {
                app.set_mc_running(true);
                return;
            }
            if run_going(&app) {
                return;
            }
            // undo moves files back and rewrites databases: not while folders are renamed for Media Composer
            if renames_open(&renames.borrow()) {
                log::write("refused: folders still renamed for Media Composer");
                app.set_error(app.get_txt_renames_open());
                return;
            }
            let lang = app.get_language().to_string();
            let path = dup_journal_path();
            let mut rows = vec![];
            match load_json::<duplicates::Journal>(&path) {
                Err(e) => rows.push(notice(3, "duplicates.json", &e.to_string(), 0)),
                Ok(mut journal) => {
                    let Some(run) = journal.entries.get(i as usize).cloned() else { return };
                    match duplicates::undo(&run) {
                        Err(e) => rows.push(notice(3, "duplicates", &e.to_string(), 0)),
                        Ok(u) => {
                            log::write(&format!("duplicates: undo, {} back, {} skipped", u.moved, u.skipped.len()));
                            rows.push(notice(5, pick(&lang, ["their folders", "ihre Ordner", "leurs dossiers", "sus carpetas"]), "", u.moved));
                            if !u.skipped.is_empty() {
                                rows.push(notice(7, &short_list(&u.skipped), "", u.skipped.len()));
                            }
                            journal.entries.remove(i as usize);
                            if let Err(e) = save_json(&journal, &path) {
                                rows.push(notice(3, &path.display().to_string(), &e.to_string(), 0));
                            }
                            // databases rewritten after the run no longer fit: rewritten again; otherwise the old ones fit again
                            if run.db_done {
                                rows.extend(rewrite_folders(&mut renames.borrow_mut(), &run.changed));
                            }
                        }
                    }
                }
            }
            show_renamed(&app, &mut renames.borrow_mut());
            show_dup_runs(&app, &lang);
            app.set_notice(ModelRc::new(VecModel::from(rows)));
            app.set_page(Page::Notice);
        }
    });

    // ── Rebalancing ────────────────────────────────────────
    let rebal: Rc<RefCell<Rebal>> = Rc::default();
    show_rebalanced(app, &settings.borrow().language);

    app.global::<Shared>().on_select_limit({
        let (weak, settings, others) = (app.as_weak(), settings.clone(), others.clone());
        move |limit| {
            let Some(app) = weak.upgrade() else { return };
            let limit = limit as usize;
            if !rebalance::LIMITS.contains(&limit) {
                return;
            }
            settings.borrow_mut().limit = limit;
            save_settings(&settings.borrow());
            apply_all(&app, &others, &settings.borrow());
        }
    });

    app.on_find_full({
        let (weak, drives, settings, rebal, renames) = (app.as_weak(), drives.clone(), settings.clone(), rebal.clone(), renames.clone());
        move || {
            let app = weak.unwrap();
            let roots: Vec<PathBuf> = drives.borrow().iter().filter(|d| d.1).map(|d| d.0.clone()).collect();
            let limit = settings.borrow().limit;
            let (away, computer) = { let st = renames.borrow(); (renamed_away(&st), st.computer.clone()) };
            app.set_work_value(0.0);
            app.set_work_text("".into());
            let Some(work) = begin_work(&app) else { return };
            app.set_work_what(2);
            app.set_page(Page::Working);
            log::write(&format!("rebalance: looking for groups that do not match {limit} files"));
            let (tx, rx) = std::sync::mpsc::channel::<Vec<Plan>>();
            let (gate, access) = (work_gate(&app), app.as_weak());
            let cancelled = gate.clone();
            std::thread::spawn(move || {
                // one run at a time; released when this thread ends
                let _work = work;
                check_access(&access, &roots, &gate);
                let mut progress = work_progress(access.clone());
                let _ = tx.send(rebalance::full_folders(&roots, limit, &away, &|mxf, name| is_mc_folder(mxf, name, &computer), &gate, &mut progress));
            });
            let (timer, weak, rebal) = (Rc::new(slint::Timer::default()), weak.clone(), rebal.clone());
            let t = timer.clone();
            timer.start(slint::TimerMode::Repeated, Duration::from_millis(200), move || {
                let Ok(plans) = rx.try_recv() else { return };
                t.stop();
                let Some(app) = weak.upgrade() else { return };
                app.set_work_can_pause(false);
                if cancelled.is_cancelled() {
                    log::write("rebalance: search cancelled");
                    app.invoke_back();
                    return;
                }
                log::write(&format!("rebalance: {} groups to lay out", plans.len()));
                let mut r = rebal.borrow_mut();
                r.plans = plans.into_iter().map(|p| (p, true)).collect();
                show_plans(&app, &r.plans, &app.get_language());
                app.set_mc_running(media_composer_running());
                app.set_page(Page::Rebalance);
            });
        }
    });

    app.on_toggle_rebalance({
        let (weak, rebal) = (app.as_weak(), rebal.clone());
        move |i| {
            let app = weak.unwrap();
            let mut r = rebal.borrow_mut();
            if let Some(p) = r.plans.get_mut(i as usize) {
                p.1 = !p.1;
            }
            show_plans(&app, &r.plans, &app.get_language());
        }
    });

    // "Rebalance": first ask about every ticked Media Composer folder (rename it?), then run
    app.on_rebalance({
        let (weak, rebal, settings, renames) = (app.as_weak(), rebal.clone(), settings.clone(), renames.clone());
        move || {
            let app = weak.unwrap();
            if media_composer_running() {
                app.set_mc_running(true);
                return;
            }
            {
                let mut r = rebal.borrow_mut();
                r.names.clear();
                r.ask = r.plans.iter().enumerate().filter(|(_, p)| p.1 && !p.0.blocked && p.0.mc_folder).map(|(i, _)| i).collect();
            }
            ask_next(&app, &rebal, &settings, &renames);
        }
    });

    app.on_rename_main({
        let (weak, rebal, settings, renames) = (app.as_weak(), rebal.clone(), settings.clone(), renames.clone());
        move |name| {
            let app = weak.unwrap();
            let lang = app.get_language().to_string();
            let computer = renames.borrow().computer.clone();
            let problem = {
                let r = rebal.borrow();
                let Some(&i) = r.ask.front() else { return };
                let mxf = &r.plans[i].0.mxf;
                // names given to other groups in the same MXF folder are taken too
                let taken: HashSet<String> = r.names.iter().filter(|(j, _)| r.plans[**j].0.mxf == *mxf).map(|(_, n)| mediafiles_check::key(n)).collect();
                rebalance::check_name(mxf, &name, is_mc_folder(mxf, &name, &computer), &taken)
            };
            use rebalance::NameProblem as N;
            let text = match problem {
                None => {
                    let mut r = rebal.borrow_mut();
                    if let Some(i) = r.ask.pop_front() {
                        r.names.insert(i, name.to_string());
                    }
                    drop(r);
                    ask_next(&app, &rebal, &settings, &renames);
                    return;
                }
                Some(N::Empty) => pick(&lang, ["Enter a name.", "Bitte einen Namen eingeben.", "Saisissez un nom.", "Escribe un nombre."]),
                Some(N::BadCharacters) => pick(&lang, ["Not allowed: “/”, “:”, a leading dot or spaces at the ends.", "Nicht erlaubt: „/“, „:“, ein Punkt am Anfang oder Leerzeichen an den Enden.", "Interdit : « / », « : », un point au début ou des espaces aux extrémités.", "No se permite: «/», «:», un punto al principio ni espacios en los extremos."]),
                Some(N::Exists) => pick(&lang, ["A folder with this name exists already.", "Einen Ordner mit diesem Namen gibt es schon.", "Un dossier de ce nom existe déjà.", "Ya existe una carpeta con este nombre."]),
                Some(N::MediaComposer) => pick(&lang, ["Media Composer uses this name itself – choose another one.", "Diesen Namen verwendet Media Composer selbst – bitte einen anderen wählen.", "Media Composer utilise lui-même ce nom – choisissez-en un autre.", "Media Composer usa este nombre: elige otro."]),
                Some(N::Reserved) => pick(&lang, ["Names with “-rebalanced-” are reserved for rebalanced folders.", "Namen mit „-rebalanced-“ sind für rebalanced-Ordner reserviert.", "Les noms avec « -rebalanced- » sont réservés aux dossiers rééquilibrés.", "Los nombres con «-rebalanced-» están reservados para las carpetas reequilibradas."]),
            };
            app.set_ask_error(text.into());
        }
    });

    app.on_keep_main({
        let (weak, rebal, settings, renames) = (app.as_weak(), rebal.clone(), settings.clone(), renames.clone());
        move || {
            let app = weak.unwrap();
            rebal.borrow_mut().ask.pop_front();
            ask_next(&app, &rebal, &settings, &renames);
        }
    });

    // "Start" (-1 = the groups just done) or "Rewrite databases" of one History entry
    app.on_rebalance_db({
        let (weak, rebal, renames) = (app.as_weak(), rebal.clone(), renames.clone());
        move |i| {
            let app = weak.unwrap();
            if media_composer_running() {
                app.set_mc_running(true);
                return;
            }
            if run_going(&app) {
                return;
            }
            if renames_open(&renames.borrow()) {
                log::write("refused: folders still renamed for Media Composer");
                return;
            }
            let done = std::mem::take(&mut rebal.borrow_mut().done);
            let journal = load_json::<rebalance::Journal>(&rebalance_journal_path()).map(|j| j.entries).unwrap_or_default();
            let pick: Option<&Rebalanced> = usize::try_from(i).ok().and_then(|i| journal.get(i));
            let mut st = renames.borrow_mut();
            let rows = rewrite_runs(&mut st, |r| match pick {
                Some(p) => r.mxf == p.mxf && r.original == p.original,
                None => done.iter().any(|d| d.0 == r.mxf && d.1 == r.original),
            });
            show_renamed(&app, &mut st);
            show_rebalanced(&app, &app.get_language());
            app.set_notice(ModelRc::new(VecModel::from(rows)));
            // the result page explains the next step and leads to Check DBs › New databases
            app.set_notice_db(true);
            app.set_page(Page::Notice);
        }
    });

    app.on_undo_rebalance({
        let (weak, renames) = (app.as_weak(), renames.clone());
        move |i| {
            let app = weak.unwrap();
            if media_composer_running() {
                app.set_mc_running(true);
                return;
            }
            if run_going(&app) {
                return;
            }
            // undo moves files back and rewrites databases: not while folders are renamed for Media Composer
            if renames_open(&renames.borrow()) {
                log::write("refused: folders still renamed for Media Composer");
                app.set_error(app.get_txt_renames_open());
                return;
            }
            let path = rebalance_journal_path();
            let entry = match load_json::<rebalance::Journal>(&path) {
                Ok(j) => j.entries.get(i as usize).cloned(),
                Err(e) => {
                    app.set_notice(ModelRc::new(VecModel::from(vec![notice(3, "rebalanced.json", &e.to_string(), 0)])));
                    app.set_page(Page::Notice);
                    return;
                }
            };
            let Some(entry) = entry else { return };
            // moving thousands of files back takes a while: in a thread, never on the window's (user test 2026-11-16)
            let Some(work) = begin_work(&app) else { return };
            app.set_work_value(0.0);
            app.set_work_text(drive_and_folder(&entry.mxf.join(&entry.original)).into());
            app.set_work_eta(-1);
            app.set_work_can_pause(false);
            app.set_work_what(5);
            app.set_page(Page::Working);
            let (tx, rx) = std::sync::mpsc::channel();
            let run = entry.clone();
            std::thread::spawn(move || {
                let _work = work;
                let _ = tx.send(rebalance::undo(&run));
            });
            let (timer, weak, renames) = (Rc::new(slint::Timer::default()), weak.clone(), renames.clone());
            let t = timer.clone();
            timer.start(slint::TimerMode::Repeated, Duration::from_millis(200), move || {
                let Ok(result) = rx.try_recv() else { return };
                t.stop();
                let Some(app) = weak.upgrade() else { return };
                let main = entry.mxf.join(&entry.original);
                let mut rows = vec![];
                match result {
                    Ok(u) => {
                        log::write(&format!("undo rebalance of {}: {} back, {} skipped, kept {:?}, renamed back {:?}", main.display(), u.moved_back, u.skipped.len(), u.kept, u.renamed_back));
                        rows.push(notice(5, &entry.original, "", u.moved_back));
                        if !u.skipped.is_empty() {
                            rows.push(notice(7, &short_list(&u.skipped), "", u.skipped.len()));
                        }
                        rows.extend(u.kept.iter().map(|k| notice(6, k, "", 0)));
                        if let Some(old) = &u.renamed_back {
                            rows.push(notice(0, &entry.original, old, 0));
                        }
                        // the journal is read again: the entry is found by its folder, not by its place in the list
                        match load_json::<rebalance::Journal>(&path) {
                            Ok(mut journal) => {
                                if let Some(i) = journal.find(&entry.mxf, &entry.original) {
                                    journal.entries.remove(i);
                                }
                                if let Err(e) = save_json(&journal, &path) {
                                    rows.push(notice(3, &path.display().to_string(), &e.to_string(), 0));
                                }
                            }
                            Err(e) => rows.push(notice(3, &path.display().to_string(), &e.to_string(), 0)),
                        }
                        // the main folder's database was set aside: a folder Media Composer does not update gets renamed
                        let main = entry.mxf.join(u.renamed_back.as_deref().unwrap_or(&entry.original));
                        let mut st = renames.borrow_mut();
                        if rename::needs_rename(&main, &st.computer) {
                            rows.extend(rename_for_mc(&mut st, std::slice::from_ref(&main)));
                        }
                        show_renamed(&app, &mut st);
                    }
                    Err(e) => rows.push(notice(3, &drive_and_folder(&main), &e.to_string(), 0)),
                }
                show_rebalanced(&app, &app.get_language());
                app.set_notice(ModelRc::new(VecModel::from(rows)));
                app.set_page(Page::Notice);
            });
        }
    });
}

fn main() -> Result<(), slint::PlatformError> {
    #[cfg(target_os = "macos")]
    mac_window::srgb_windows();
    let settings = Rc::new(RefCell::new(load_settings()));
    let app = AppWindow::new()?;
    repaint_on_resize!(app);
    let _ = slint::select_bundled_translation(&settings.borrow().language);
    app.set_language(settings.borrow().language.clone().into());
    apply_menus(&app.global::<Shared>(), &settings.borrow());
    let drives: Drives = Rc::default();
    // a hanging mount must not keep the window from opening: the timer fills the list later
    if let Some(found) = mediafiles_check::find_volumes_responsive() {
        sync_drives(&app, &drives, found);
    }
    let others = Others::default();
    let last: Last = Arc::default();
    let renames = Rc::new(RefCell::new(Renames::load()));
    if let Err(e) = &renames.borrow().journal {
        log::write(&format!("journal unreadable: {e}"));
        app.set_error(e.clone().into());
    }
    show_renamed(&app, &mut renames.borrow_mut());
    log::write(&format!("start {} {}", env!("CARGO_PKG_VERSION"), std::env::consts::ARCH));
    std::panic::set_hook(Box::new(|info| log::write(&format!("crash: {info}"))));

    // Menu actions run after the click: they rename menu items, which crashed during macOS menu handling.
    app.global::<Shared>().on_select_language({
        let (weak, settings, others) = (app.as_weak(), settings.clone(), others.clone());
        move |i| {
            let (weak, settings, others) = (weak.clone(), settings.clone(), others.clone());
            slint::Timer::single_shot(Duration::ZERO, move || {
                let Some(app) = weak.upgrade() else { return };
                let code = LANGUAGES.get(i as usize).map_or("en", |l| l.0);
                let _ = slint::select_bundled_translation(code);
                app.set_language(code.into());
                settings.borrow_mut().language = code.into();
                save_settings(&settings.borrow());
                apply_all(&app, &others, &settings.borrow());
            });
        }
    });

    app.global::<Shared>().on_select_dark({
        let (weak, settings, others) = (app.as_weak(), settings.clone(), others.clone());
        move |dark| {
            let (weak, settings, others) = (weak.clone(), settings.clone(), others.clone());
            slint::Timer::single_shot(Duration::ZERO, move || {
                let Some(app) = weak.upgrade() else { return };
                settings.borrow_mut().dark = dark;
                save_settings(&settings.borrow());
                apply_all(&app, &others, &settings.borrow());
            });
        }
    });

    // "Get test versions": the old result no longer applies – hide "up to date" and check again quietly
    app.global::<Shared>().on_select_betas({
        let (weak, settings, others) = (app.as_weak(), settings.clone(), others.clone());
        move |betas| {
            let (weak, settings, others) = (weak.clone(), settings.clone(), others.clone());
            slint::Timer::single_shot(Duration::ZERO, move || {
                let Some(app) = weak.upgrade() else { return };
                settings.borrow_mut().betas = betas;
                save_settings(&settings.borrow());
                {
                    let s = settings.borrow();
                    let mut u = s.update.lock().unwrap();
                    if u.state == 2 || u.state == 4 {
                        u.state = 0;
                    }
                }
                apply_all(&app, &others, &settings.borrow());
                check_for_update(&app, &settings.borrow(), false);
            });
        }
    });

    // Settings… opens the tab chosen last (never About); About opens the same window on the About tab
    let open_settings = {
        let (weak, settings, win) = (app.as_weak(), settings.clone(), others.settings.clone());
        move |about: bool| {
            let (weak, settings, win) = (weak.clone(), settings.clone(), win.clone());
            slint::Timer::single_shot(Duration::ZERO, move || {
                let mut win = win.borrow_mut();
                if win.is_none() {
                    *win = SettingsWindow::new().ok();
                    win.iter().for_each(|w| {
                        repaint_on_resize!(w);
                        forward_menu(&w.global::<Shared>(), &weak);
                        wire_support(w, &settings);
                        w.set_version(env!("CARGO_PKG_VERSION").into());
                        // HIG settings: restore the most recently viewed pane (also after a restart)
                        w.set_last_tab(settings.borrow().tab);
                        w.on_tab_chosen({
                            let settings = settings.clone();
                            move |tab| {
                                settings.borrow_mut().tab = tab;
                                save_settings(&settings.borrow());
                            }
                        });
                        w.on_open_source(|| {
                            let _ = std::process::Command::new("open").arg("https://github.com/moritz-poth/MediaFiles-Check").spawn();
                        });
                        w.on_open_license(|| {
                            // bundled license text, otherwise the FSF page
                            let bundled = std::env::current_exe().ok().and_then(|e| Some(e.parent()?.parent()?.join("Resources/LICENSE.txt")));
                            let target = bundled.filter(|p| p.exists()).map_or("https://www.gnu.org/licenses/gpl-3.0.html".into(), |p| p.display().to_string());
                            let _ = std::process::Command::new("open").arg(target).spawn();
                        });
                    });
                }
                if let Some(w) = win.as_ref() {
                    apply_menus(&w.global::<Shared>(), &settings.borrow());
                    w.set_tab(if about { 3 } else { w.get_last_tab() });
                    let _ = w.show();
                }
            });
        }
    };
    app.global::<Shared>().on_show_settings({
        let open = open_settings.clone();
        move || open(false)
    });
    app.global::<Shared>().on_show_about(move || open_settings(true));

    app.global::<Shared>().on_refresh_update({
        let (weak, settings, others) = (app.as_weak(), settings.clone(), others.clone());
        move || weak.upgrade().iter().for_each(|a| apply_all(a, &others, &settings.borrow()))
    });

    app.global::<Shared>().on_check_update({
        let (weak, settings) = (app.as_weak(), settings.clone());
        move || weak.upgrade().iter().for_each(|a| check_for_update(a, &settings.borrow(), true))
    });
    app.global::<Shared>().on_install_update({
        let (weak, settings) = (app.as_weak(), settings.clone());
        move || weak.upgrade().iter().for_each(|a| install_update(a, &settings.borrow()))
    });
    app.global::<Shared>().on_show_release({
        let settings = settings.clone();
        move || {
            if let Some(r) = settings.borrow().update.lock().unwrap().available.as_ref() {
                let _ = std::process::Command::new("open").arg(&r.page).spawn();
            }
        }
    });
    // quietly shortly after the start, then every 6 hours
    slint::Timer::single_shot(Duration::from_secs(5), {
        let (weak, settings) = (app.as_weak(), settings.clone());
        move || weak.upgrade().iter().for_each(|a| check_for_update(a, &settings.borrow(), false))
    });
    let update_timer = slint::Timer::default();
    update_timer.start(slint::TimerMode::Repeated, Duration::from_secs(6 * 3600), {
        let (weak, settings) = (app.as_weak(), settings.clone());
        move || weak.upgrade().iter().for_each(|a| check_for_update(a, &settings.borrow(), false))
    });

    app.global::<Shared>().on_show_log(|| {
        log::write("log file shown");
        let _ = std::process::Command::new("open").arg("-R").arg(log::path()).spawn();
    });

    app.global::<Shared>().on_show_help({
        let (weak, settings, help) = (app.as_weak(), settings.clone(), others.help.clone());
        move || {
            let (weak, settings, help) = (weak.clone(), settings.clone(), help.clone());
            slint::Timer::single_shot(Duration::ZERO, move || {
                let mut help = help.borrow_mut();
                if help.is_none() {
                    *help = HelpWindow::new().ok();
                    help.iter().for_each(|h| {
                        repaint_on_resize!(h);
                        forward_menu(&h.global::<Shared>(), &weak)
                    });
                }
                if let Some(h) = help.as_ref() {
                    update_help(h, &settings.borrow());
                    let _ = h.show();
                }
            });
        }
    });

    app.on_check_drives({
        let (weak, last) = (app.as_weak(), last.clone());
        // the volumes of the current scan (Scan tab) that are still connected
        move || start_check(&weak.unwrap(), inventory_ui::scanned_volumes(), last.clone())
    });

    app.on_toggle_finding({
        let (weak, last) = (app.as_weak(), last.clone());
        move |n| {
            let app = weak.unwrap();
            let mut l = last.lock().unwrap();
            if !l.open.remove(&n) {
                l.open.insert(n);
            }
            app.set_rows(ModelRc::new(VecModel::from(rows(&l.reports, &app.get_language(), &app.get_txt_more(), &l.open))));
        }
    });

    app.on_check_folder({
        let (weak, last) = (app.as_weak(), last.clone());
        move || {
            let app = weak.unwrap();
            if let Some(folder) = rfd::FileDialog::new().set_title(app.get_txt_choose_folder()).pick_folder() {
                start_check(&app, vec![folder], last.clone());
            }
        }
    });

    app.global::<Access>().on_ask({
        let weak = app.as_weak();
        move || {
            weak.unwrap().global::<Access>().set_asking(true);
            ask_access(weak.clone());
        }
    });
    app.on_work_pause(|on| gate(ScanAt::Work).pause(on));
    app.on_work_cancel(|| gate(ScanAt::Work).cancel());

    app.on_back({
        let (weak, drives, renames) = (app.as_weak(), drives.clone(), renames.clone());
        move || {
            let app = weak.unwrap();
            if let Some(found) = mediafiles_check::find_volumes_responsive() {
                sync_drives(&app, &drives, found);
                show_renamed(&app, &mut renames.borrow_mut());
            }
            app.invoke_home();
        }
    });


    wire_maintain(&app, &drives, &settings, &renames, &others);

    app.on_show_rename({
        let weak = app.as_weak();
        move || {
            let app = weak.unwrap();
            app.set_mc_running(media_composer_running());
            app.set_page(Page::Rename);
        }
    });

    app.on_rename({
        let (weak, renames, last) = (app.as_weak(), renames.clone(), last.clone());
        move || {
            let app = weak.unwrap();
            if media_composer_running() {
                app.set_mc_running(true);
                return;
            }
            if run_going(&app) {
                return;
            }
            if renames_open(&renames.borrow()) {
                log::write("refused: folders still renamed for Media Composer");
                return;
            }
            let candidates = std::mem::take(&mut last.lock().unwrap().candidates);
            show_candidates(&app, &[]);
            let mut st = renames.borrow_mut();
            let Ok(journal) = st.journal.as_mut() else { return };
            let errors: Vec<NoticeRow> = candidates
                .iter()
                .filter_map(|(folder, to)| {
                    let result = rename::rename_for_avid(folder, to, journal, &journal_path());
                    log::write(&format!("rename {} -> {to}: {}", folder.display(), result.as_ref().map_or_else(|e| e.to_string(), |_| "ok".into())));
                    result.err().map(|e| notice(3, &volume_name(folder), &e.to_string(), 0))
                })
                .collect();
            show_renamed(&app, &mut st);
            if errors.is_empty() {
                app.set_page(Page::Maintain);
            } else {
                app.set_notice(ModelRc::new(VecModel::from(errors)));
                app.set_page(Page::Notice);
            }
        }
    });

    app.on_rename_back({
        let (weak, renames) = (app.as_weak(), renames.clone());
        move || {
            let app = weak.unwrap();
            if media_composer_running() {
                app.set_mc_running(true);
                return;
            }
            let mut st = renames.borrow_mut();
            let Ok(journal) = &st.journal else { return };
            let mut items = vec![];
            let mut errors = vec![];
            for r in journal.entries.iter().filter(|r| rename::status(r) != Status::Offline) {
                match rename::diff(r) {
                    Ok(d) => items.push((r.clone(), d.added, d.changed.len(), rename::status(r) == Status::Ready)),
                    Err(e) => errors.push(notice(3, &r.current, &e.to_string(), 0)),
                }
            }
            if items.is_empty() {
                app.set_notice(ModelRc::new(VecModel::from(errors)));
                app.set_page(Page::Notice);
                return;
            }
            // decided for all at once; single folders can be left out (unticked)
            let flow = BackFlow { on: vec![true; items.len()], items };
            show_back(&app, &flow);
            app.set_back_choice(0);
            // local drives use "1", network drives "<computer>.1"
            app.set_back_first(format!("1 / {}.1", st.computer).into());
            st.back = Some(flow);
            app.set_page(Page::Question);
        }
    });

    wire_exchange(&app, &drives, &settings, &renames);
    let welcome_seen = || app_dir().map(|d| d.join("welcome-seen"));
    app.set_show_welcome(welcome_seen().is_some_and(|p| !p.exists()));
    app.on_welcome_done({
        let weak = app.as_weak();
        move || {
            if let Some(p) = welcome_seen() {
                let _ = fs::create_dir_all(p.parent().unwrap());
                let _ = fs::write(p, env!("CARGO_PKG_VERSION"));
            }
            weak.unwrap().set_show_welcome(false);
        }
    });
    let task_acts = wire_tasks(&app, &last);
    let projects_state: Rc<RefCell<ProjectsState>> = Rc::default();
    projects_state.borrow_mut().media = others.media.clone();
    wire_projects(&app, &projects_state, &settings);
    let inv: inventory_ui::Inv = Rc::default();
    inv.borrow_mut().win = others.table.clone();
    inventory_ui::wire(&app, &inv, &drives, &projects_state, &settings, &renames);

    app.on_start_media_composer({
        let weak = app.as_weak();
        move || {
            let started = std::process::Command::new("open").args(["-b", "com.avid.mediacomposer"]).status();
            log::write(&format!("start Media Composer: {started:?}"));
            if !started.is_ok_and(|s| s.success()) {
                let app = weak.unwrap();
                app.set_error(app.get_txt_mc_not_started());
            }
        }
    });

    app.on_back_toggle({
        let (weak, renames) = (app.as_weak(), renames.clone());
        move |i| {
            let mut st = renames.borrow_mut();
            let Some(flow) = st.back.as_mut() else { return };
            // -1 all, -2 none
            for (n, on) in flow.on.iter_mut().enumerate() {
                match i {
                    -1 => *on = true,
                    -2 => *on = false,
                    i if n == i as usize => *on = !*on,
                    _ => {}
                }
            }
            show_back(&weak.unwrap(), flow);
        }
    });
    app.on_back_confirm({
        let (weak, renames) = (app.as_weak(), renames.clone());
        move |choice| {
            let app = weak.unwrap();
            if media_composer_running() {
                app.set_mc_running(true);
                return;
            }
            let choice = match choice {
                1 => Added::NewFolder,
                2 => Added::FirstFolder,
                _ => Added::Keep,
            };
            finish_back(&app, &mut renames.borrow_mut(), choice);
        }
    });
    app.on_back_cancel({
        let (weak, renames) = (app.as_weak(), renames.clone());
        move || {
            renames.borrow_mut().back = None;
            weak.unwrap().set_page(Page::Maintain);
        }
    });

    // polls /Volumes every 2 s on the start pages (also Scan: drives mounted later appear). The lookup runs in a
    // thread first: while a network or macFUSE mount hangs, this tick reads nothing on disk, so the window never freezes
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_secs(2), {
        let (weak, drives, renames, task_acts) = (app.as_weak(), drives.clone(), renames.clone(), task_acts.clone());
        move || {
            let Some(app) = weak.upgrade() else { return };
            #[cfg(target_os = "macos")]
            mac_menu::ensure();
            match app.get_page() {
                Page::Home | Page::Maintain | Page::Rebalstart | Page::Dupsstart | Page::Runs | Page::Exchange | Page::Projects | Page::Media | Page::Scan => {
                    if let Some(found) = mediafiles_check::find_volumes_responsive() {
                        sync_drives(&app, &drives, found);
                        inventory_ui::show_runs(&app);
                        show_renamed(&app, &mut renames.borrow_mut());
                        show_rebalanced(&app, &app.get_language());
                        show_dup_runs(&app, &app.get_language());
                        show_tasks(&app, &task_acts);
                    }
                }
                _ => {}
            }
            if app.get_page() != Page::Working {
                app.set_mc_running(media_composer_running());
                app.global::<Inventory>().set_mc_running(app.get_mc_running());
            }
        }
    });

    #[cfg(target_os = "macos")]
    {
        let (w1, w2) = (app.as_weak(), app.as_weak());
        mac_menu::install(
            Box::new(move || w1.upgrade().into_iter().for_each(|a| a.global::<Shared>().invoke_show_about())),
            Box::new(move || w2.upgrade().into_iter().for_each(|a| a.global::<Shared>().invoke_show_settings())),
            &app.get_txt_settings_menu(),
        );
    }

    // title bars follow the app's light/dark; the settings tab bar follows the title bar (lighter while active)
    // shortcut: polls every 200 ms for the title bar style and appearance – cheap, both only change something once
    #[cfg(target_os = "macos")]
    let chrome = slint::Timer::default();
    #[cfg(target_os = "macos")]
    chrome.start(slint::TimerMode::Repeated, Duration::from_millis(200), {
        let (win, settings, weak) = (others.settings.clone(), settings.clone(), app.as_weak());
        move || {
            mac_window::appearance_all(settings.borrow().dark);
            // the main window never grows past the screen (minus its title bar); longer pages scroll
            let h = mac_window::screen_height();
            if let Some(app) = weak.upgrade() {
                if h > 0.0 {
                    app.set_screen_height((h - 28.0) as f32);
                }
                // tab bar and title bar are one surface, as in the settings window
                let size = app.window().size().to_logical(app.window().scale_factor());
                mac_window::unify_titlebar(app.window(), size.width as f64, size.height as f64);
            }
            if let Some(w) = win.borrow().as_ref() {
                // the NSWindow exists only once the event loop has run after show(); only changes it once
                mac_window::unify_titlebar(w.window(), 800.0, 450.0);
            }
        }
    });
    #[cfg(target_os = "macos")]
    mac_window::on_key_change({
        let (win, weak) = (others.settings.clone(), app.as_weak());
        Box::new(move || {
            // called from inside AppKit (also while show() holds the window's RefCell): update on the next turn
            let (win, weak) = (win.clone(), weak.clone());
            slint::Timer::single_shot(Duration::ZERO, move || {
                if let Some(app) = weak.upgrade() {
                    let key = mac_window::is_key(app.window());
                    if app.get_active() != key {
                        app.set_active(key);
                    }
                }
                if let Some(w) = win.borrow().as_ref() {
                    let key = mac_window::is_key(w.window());
                    if w.get_active() != key {
                        w.set_active(key);
                    }
                }
            });
        })
    });
    app.run()
}

#[cfg(test)]
mod tests {
    #[test]
    fn size_text_uses_the_decimal_comma_outside_english() {
        assert_eq!(super::size_text(1_468_006, "en"), "1.4 MB");
        assert_eq!(super::size_text(1_468_006, "de"), "1,4 MB");
        assert_eq!(super::size_text(3 << 30, "fr"), "3,0 GB");
        assert_eq!(super::size_text(2000, "es"), "2 KB");
    }

    /// Column edges of the media table with real pointer events: dragging changes only that column,
    /// the list does not pan along, a double-click fits the column and dragging still works after it,
    /// the wheel scrolls rows vertically under a fixed header and moves header and rows together sideways.
    #[test]
    fn media_table_columns_resize_and_scroll() {
        use slint::platform::{PointerEventButton, WindowEvent};
        use slint::{ComponentHandle, LogicalPosition};
        use i_slint_backend_testing::{ElementHandle, mock_elapsed_time};
        i_slint_backend_testing::init_no_event_loop();
        let w = super::MediaTableWindow::new().unwrap();
        slint::select_bundled_translation("en").unwrap();
        w.window().set_size(slint::LogicalSize::new(1000.0, 600.0));
        let rows: Vec<super::MediaRow> = (0..300)
            .map(|i| super::MediaRow { clip: format!("clip-{i}").into(), file: format!("{}-file.mxf", "x".repeat(40)).into(), ..Default::default() })
            .collect();
        w.set_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
        w.set_fit_chars(slint::ModelRc::new(slint::VecModel::from(vec![30; 13])));
        w.show().unwrap();
        let one = |label: &str| ElementHandle::find_by_accessible_label(&w, label).next()
            .or_else(|| { w.window().dispatch_event(WindowEvent::PointerMoved { position: LogicalPosition::new(300.0, 300.0) }); ElementHandle::find_by_accessible_label(&w, label).next() }).unwrap_or_else(|| panic!("{label}"));
        let head = |label: &str| { let e = one(label); (e.absolute_position(), e.size()) };
        let ev = |e: WindowEvent| w.window().dispatch_event(e);
        let at = |x: f32, y: f32| LogicalPosition::new(x, y);
        let drag = |from: LogicalPosition, dx: f32| {
            ev(WindowEvent::PointerMoved { position: from });
            ev(WindowEvent::PointerPressed { position: from, button: PointerEventButton::Left });
            for k in 1..=10 {
                mock_elapsed_time(std::time::Duration::from_millis(16));
                ev(WindowEvent::PointerMoved { position: at(from.x + dx * k as f32 / 10.0, from.y) });
            }
            ev(WindowEvent::PointerReleased { position: at(from.x + dx, from.y), button: PointerEventButton::Left });
            mock_elapsed_time(std::time::Duration::from_millis(1000));
        };
        let edge = |label: &str| { let (p, s) = head(label); at(p.x + s.width - 3.0, p.y + s.height / 2.0) };

        let (clip0, clip_size) = head("Clip");
        let kind0 = head("Kind").0;
        let row0 = one("clip-0").absolute_position();
        drag(edge("Clip"), 60.0);
        assert!((head("Clip").1.width - (clip_size.width + 60.0)).abs() < 0.5, "width {}", head("Clip").1.width);
        assert!((head("Kind").0.x - (kind0.x + 60.0)).abs() < 0.5, "next column follows");
        assert_eq!(head("Clip").0, clip0, "column itself stays");
        assert_eq!(one("clip-0").absolute_position(), row0, "list did not pan");

        // past the minimum and back: the edge waits for the mouse like in Finder
        drag(edge("Clip"), -400.0);
        assert!((head("Clip").1.width - 36.0).abs() < 0.5);

        // double-click fits the column (30 chars), dragging afterwards still works
        let p = edge("Clip");
        for _ in 0..2 {
            ev(WindowEvent::PointerPressed { position: p, button: PointerEventButton::Left });
            mock_elapsed_time(std::time::Duration::from_millis(30));
            ev(WindowEvent::PointerReleased { position: p, button: PointerEventButton::Left });
            mock_elapsed_time(std::time::Duration::from_millis(30));
        }
        let fitted = head("Clip").1.width;
        assert!((fitted - (30.0 * 7.2 + 18.0)).abs() < 0.5, "fitted {fitted}");
        mock_elapsed_time(std::time::Duration::from_millis(1000));
        drag(edge("Clip"), 40.0);
        assert!((head("Clip").1.width - (fitted + 40.0)).abs() < 0.5, "drag after double-click {}", head("Clip").1.width);

        // vertical wheel: rows move up, header stays
        let header_y = head("Clip").0.y;
        let row_y = one("clip-12").absolute_position().y;
        ev(WindowEvent::PointerScrolled { position: at(300.0, 300.0), delta_x: 0.0, delta_y: -240.0 });
        assert!((one("clip-12").absolute_position().y - (row_y - 240.0)).abs() < 0.5, "rows scrolled");
        assert_eq!(head("Clip").0.y, header_y, "header stays");
        // sideways: header and rows move together
        let hx = head("Kind").0.x;
        let rx = one("clip-12").absolute_position().x;
        ev(WindowEvent::PointerScrolled { position: at(300.0, 300.0), delta_x: -200.0, delta_y: 0.0 });
        let moved = hx - head("Kind").0.x;
        assert!(moved > 100.0, "scrolled sideways {moved}");
        assert!((rx - one("clip-12").absolute_position().x - moved).abs() < 0.5, "rows follow the header");

        // missing media window: same column heads
        let m = super::MediaWindow::new().unwrap();
        m.window().set_size(slint::LogicalSize::new(1240.0, 720.0));
        let rows: Vec<super::MediaFileRow> = (0..200)
            .map(|i| super::MediaFileRow { level: 1, clip: format!("m-{i}").into(), mxf: "a.mxf".into(), first: true, index: i, ..Default::default() })
            .collect();
        m.set_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
        m.set_fit_chars(slint::ModelRc::new(slint::VecModel::from(vec![20, 5, 0])));
        m.show().unwrap();
        let ev = |e: WindowEvent| m.window().dispatch_event(e);
        ev(WindowEvent::PointerMoved { position: at(700.0, 400.0) });
        let one = |label: &str| ElementHandle::find_by_accessible_label(&m, label).next().unwrap_or_else(|| panic!("{label}"));
        let head = |label: &str| { let e = one(label); (e.absolute_position(), e.size()) };
        let (_, s0) = head("Clip in the bin");
        let mxf0 = head("MXF file").0;
        let row0 = one("m-0").absolute_position();
        let (p, s) = head("Clip in the bin");
        let from = at(p.x + s.width - 3.0, p.y + s.height / 2.0);
        ev(WindowEvent::PointerPressed { position: from, button: PointerEventButton::Left });
        for k in 1..=10 {
            mock_elapsed_time(std::time::Duration::from_millis(16));
            ev(WindowEvent::PointerMoved { position: at(from.x + 5.0 * k as f32, from.y) });
        }
        ev(WindowEvent::PointerReleased { position: at(from.x + 50.0, from.y), button: PointerEventButton::Left });
        mock_elapsed_time(std::time::Duration::from_millis(1000));
        assert!((head("Clip in the bin").1.width - (s0.width + 50.0)).abs() < 0.5);
        assert!((head("MXF file").0.x - (mxf0.x + 50.0)).abs() < 0.5);
        assert_eq!(one("m-0").absolute_position(), row0, "missing media list did not pan");
        let row_y = one("m-12").absolute_position().y;
        ev(WindowEvent::PointerScrolled { position: at(700.0, 400.0), delta_x: 0.0, delta_y: -120.0 });
        assert!((one("m-12").absolute_position().y - (row_y - 120.0)).abs() < 0.5, "missing media rows scroll");
    }

    #[test]
    fn ticking_a_folder_ticks_its_parents() {
        use super::{FolderKey, HashSet, toggle_folder};
        let k = |d: &[&str]| ("P".to_string(), d.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let mut hidden: HashSet<FolderKey> = [k(&[]), k(&["A"]), k(&["A", "B"]), k(&["A", "C"]), k(&["D"])].into();
        toggle_folder(&mut hidden, k(&["A", "B"]));
        assert_eq!(hidden, [k(&["A", "C"]), k(&["D"])].into(), "project and A on, siblings stay off");
        toggle_folder(&mut hidden, k(&["A", "B"]));
        assert!(hidden.contains(&k(&["A", "B"])) && !hidden.contains(&k(&["A"])), "unticking touches only itself");
    }

    #[test]
    fn bins_of_switched_off_folders_are_hidden() {
        use super::{FolderKey, HashSet, bin_folder, bin_visible};
        let hidden: HashSet<FolderKey> = [("P".to_string(), vec!["Old".to_string()])].into();
        let k = |b: &str| ("P".to_string(), b.to_string());
        assert_eq!(bin_folder("Old/Cuts/Bin.avb"), ["Old", "Cuts"]);
        assert!(!bin_visible(&hidden, &k("Old/Cuts/Bin.avb")) && !bin_visible(&hidden, &k("Old/Bin.avb")));
        assert!(bin_visible(&hidden, &k("Bin.avb")) && bin_visible(&hidden, &k("Older/Bin.avb")));
        assert!(!bin_visible(&[("P".to_string(), vec![])].into(), &k("Bin.avb")));
    }

    #[test]
    fn closed_folders_hide_their_subfolders() {
        use super::{FolderKey, HashSet, shown_folders};
        let k = |p: &str, d: &[&str]| -> FolderKey { (p.to_string(), d.iter().map(|s| s.to_string()).collect()) };
        let keys = [k("P", &[]), k("P", &["A"]), k("P", &["A", "B"]), k("P", &["C"]), k("Q", &[])];
        assert_eq!(shown_folders(&keys, &HashSet::new()), [(0, true), (4, false)]);
        assert_eq!(shown_folders(&keys, &[k("P", &[])].into()), [(0, true), (1, true), (3, false), (4, false)]);
        // a child stays hidden while a parent further up is closed
        assert_eq!(shown_folders(&keys, &[k("P", &["A"])].into()), [(0, true), (4, false)]);
        assert_eq!(shown_folders(&keys, &[k("P", &[]), k("P", &["A"])].into()).len(), 5);
    }

    #[test]
    fn eta_from_recent_rate() {
        use super::{Duration, Eta};
        let (mut e, t) = (Eta::default(), std::time::Instant::now());
        let at = |secs| t + Duration::from_secs(secs);
        assert_eq!(e.at(at(0), 400, 1000), 0); // 400 already done (cache)
        assert_eq!(e.at(at(4), 440, 1000), 0); // too early
        assert_eq!(e.at(at(10), 500, 1000), 50); // 10 a second, 500 left
        // after a slower stretch only the last 20 s count: 1 a second
        for i in 11..=40 {
            e.at(at(i), 500 + (i - 10), 1000);
        }
        assert_eq!(e.at(at(41), 531, 1000), 469);
        assert_eq!(e.at(at(42), 1000, 1000), -1);
    }

    #[test]
    fn long_names_are_shortened_in_the_middle() {
        assert_eq!(super::shorten("Avid8.1", 10), "Avid8.1");
        assert_eq!(super::shorten("/Volumes/Archive/Projects/Avid Projects", 21), "/Volumes/A…d Projects");
    }

    #[test]
    fn help_is_split_at_headings_in_every_language() {
        for l in super::LANGUAGES {
            let (subtitle, blocks) = super::help_blocks(l.2);
            assert!(!subtitle.is_empty(), "{}", l.0);
            assert!(blocks.iter().filter(|b| !b.heading.is_empty()).count() >= 5, "{}", l.0);
            assert!(!blocks[0].heading.is_empty(), "{}: starts with a heading", l.0);
            // every picture the manual names exists
            for line in l.2.lines().filter(|l| l.starts_with("![")) {
                let path = line.split_once("](").unwrap().1.trim_end_matches(')');
                assert!(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/help").join(path).is_file(), "{}: {path}", l.0);
            }
        }
        let (_, blocks) = super::help_blocks("## A\n\ntext\n\n![x](nowhere.png)\n\nmore");
        assert_eq!(blocks.len(), 4);
        assert_eq!(blocks[2].picture.size().width, 0); // missing file: empty picture, not a crash
    }
}
