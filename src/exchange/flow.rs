//! Abläufe der Oberfläche: Vergleichen → Sammeln, Lieferung → Prüfen & Einspielen.
//! Der Zustand liegt in `Session`; jede Änderung baut die Modelle der Oberfläche neu auf.
use crate::exchange::collect::{self, Delivery};
use crate::exchange::plan::{self, Options, Plan};
use crate::exchange::{DB_FILES, Manifest, key, local_time, now_secs, safe_name, scan, write_atomic};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Gemerkte Zuordnungen, damit man sie beim nächsten Mal nicht neu wählen muss.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Mappings {
    /// Gegenseite -> (eigenes Volume -> Volume drüben, "" = nicht senden)
    pub send: BTreeMap<String, BTreeMap<String, String>>,
    /// Absender -> (Volume der Lieferung -> eigenes Volume)
    pub receive: BTreeMap<String, BTreeMap<String, String>>,
}

impl Mappings {
    pub fn load(dir: &Path) -> Self {
        fs::read(dir.join("mappings.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    pub fn save(&self, dir: &Path) {
        // Komfort: scheitert das Speichern, wird beim nächsten Mal neu gefragt.
        let _ = fs::create_dir_all(dir);
        let _ = write_atomic(&dir.join("mappings.json"), &serde_json::to_vec_pretty(self).unwrap_or_default());
    }
}

/// Eine Zuordnung: Name, Auswahlmöglichkeiten, gewählter Index (0 = noch offen).
pub struct Choice {
    pub name: String,
    pub options: Vec<String>,
    pub selected: usize,
}

#[derive(Default)]
pub struct Compare {
    pub local: Manifest,
    pub remote: Manifest,
    /// alle eigenen Volumes; options = Volumes drüben, letzter Index = nicht senden; gleichnamige sind vorausgewählt
    pub drives: Vec<Choice>,
    /// Anforderung aus MediaFiles Check: fester Plan, nichts zuzuordnen
    pub request: Option<Plan>,
}

impl Compare {
    pub fn new(local: Manifest, remote: Manifest, saved: &Mappings) -> Self {
        let remote_names: Vec<String> = remote.volumes.iter().map(|v| v.name.clone()).collect();
        let saved = saved.send.get(&remote.system);
        let drives = local
            .volumes
            .iter()
            .map(|lv| {
                // the saved choice first, else the drive of the same name over there (user 2026-11-16: preselected, not fixed)
                let same = || remote_names.iter().position(|n| key(n) == key(&lv.name)).map_or(0, |i| i + 1);
                let selected = match saved.and_then(|m| m.get(&lv.name)) {
                    Some(r) if r.is_empty() => remote_names.len() + 1,
                    Some(r) => remote_names.iter().position(|n| key(n) == key(r)).map_or_else(same, |i| i + 1),
                    None => same(),
                };
                Choice { name: lv.name.clone(), options: remote_names.clone(), selected }
            })
            .collect();
        Compare { local, remote, drives, request: None }
    }

    /// Anforderung: die Gegenseite ist der Anfordernde, der Plan steht fest.
    pub fn from_request(local: Manifest, req: &crate::exchange::request::Request) -> Self {
        let plan = crate::exchange::request::plan(&local, req);
        let remote = Manifest { system: req.system.clone(), created: req.created, ..Default::default() };
        Compare { local, remote, drives: vec![], request: Some(plan) }
    }

    fn options(&self) -> Options {
        let mut o = Options::default();
        for d in &self.drives {
            if (1..=d.options.len()).contains(&d.selected) {
                o.volume_map.insert(d.name.clone(), d.options[d.selected - 1].clone());
            } else if d.selected > d.options.len() {
                o.skip.insert(d.name.clone());
            }
        }
        o
    }

    pub fn plan(&self) -> Plan {
        if let Some(p) = &self.request {
            return p.clone();
        }
        plan::plan(&self.local, &self.remote, &self.options())
    }

    pub fn ready(&self) -> bool {
        self.drives.iter().all(|d| d.selected != 0)
    }

    pub fn remember(&self, m: &mut Mappings) {
        let entry = m.send.entry(self.remote.system.clone()).or_default();
        for d in &self.drives {
            match d.selected {
                0 => {}
                i if i <= d.options.len() => {
                    entry.insert(d.name.clone(), d.options[i - 1].clone());
                }
                _ => {
                    entry.insert(d.name.clone(), String::new());
                }
            }
        }
    }

    /// Lieferordner im gewählten Ordner, z. B. "261102-1830_Mac Studio_to_MacBook"
    pub fn delivery_dir(&self, parent: &Path) -> PathBuf {
        let name = format!("{}_{}_to_{}", local_time(now_secs(), "%y%m%d-%H%M"), safe_name(&self.local.system), safe_name(&self.remote.system));
        parent.join(name)
    }
}

/// Mediendateien (ohne Datenbanken) eines Plans: Anzahl und Bytes
pub fn media(plan: &Plan) -> (usize, u64) {
    plan.copies.iter().filter(|c| c.hash.is_some()).fold((0, 0), |(n, b), c| (n + 1, b + c.size))
}

pub struct Incoming {
    pub dir: PathBuf,
    pub delivery: Delivery,
    /// Volumes der Lieferung; options = eingehängte Laufwerke
    pub drives: Vec<Choice>,
    /// per drive: None = as on the other side, Some(i) = separated, render files to option i (0 = not chosen yet)
    pub renders: Vec<Option<usize>>,
    /// folders of the delivery ("Volume/Folder"), true = into a folder of its own ("<sender>_<Folder>")
    pub folders: Vec<(String, bool)>,
}

impl Incoming {
    pub fn open(dir: PathBuf, saved: &Mappings) -> std::io::Result<Self> {
        let delivery = collect::load_delivery(&dir)?;
        let found: Vec<String> = scan::find_volumes().iter().map(|p| scan::volume_name(p)).collect();
        let saved = saved.receive.get(&delivery.from);
        let mut names: Vec<String> = vec![];
        let mut folders: Vec<(String, bool)> = vec![];
        for f in &delivery.files {
            let mut parts = f.path.split('/');
            let vol = parts.next().unwrap_or_default().to_string();
            let id = format!("{vol}/{}", parts.next().unwrap_or_default());
            if !names.contains(&vol) {
                names.push(vol);
            }
            if !folders.iter().any(|x| x.0 == id) {
                folders.push((id, false));
            }
        }
        let drives: Vec<Choice> = names
            .into_iter()
            .map(|name| {
                let pick = |want: &str| found.iter().position(|f| key(f) == key(want)).map(|i| i + 1);
                let selected = pick(&name).or_else(|| saved.and_then(|m| m.get(&name)).and_then(|s| pick(s))).unwrap_or(0);
                Choice { name, options: found.clone(), selected }
            })
            .collect();
        let renders = vec![None; drives.len()];
        Ok(Incoming { dir, delivery, drives, renders, folders })
    }

    pub fn ready(&self) -> bool {
        self.drives.iter().all(|d| d.selected != 0) && self.renders.iter().all(|r| *r != Some(0))
    }

    /// Where everything goes: drives per volume of the delivery, render drives, folders of their own.
    pub fn targets(&self) -> collect::Targets {
        let at = |d: &Choice, i: usize| PathBuf::from(format!("/Volumes/{}", d.options[i - 1]));
        collect::Targets {
            media: self.drives.iter().filter(|d| d.selected != 0).map(|d| (d.name.clone(), at(d, d.selected))).collect(),
            render: self.drives.iter().zip(&self.renders).filter_map(|(d, r)| r.filter(|i| *i != 0).map(|i| (d.name.clone(), at(d, i)))).collect(),
            own_folder: self.folders.iter().filter(|f| f.1).map(|f| f.0.clone()).collect(),
        }
    }

    pub fn remember(&self, m: &mut Mappings) {
        let entry = m.receive.entry(self.delivery.from.clone()).or_default();
        for d in self.drives.iter().filter(|d| d.selected != 0) {
            entry.insert(d.name.clone(), d.options[d.selected - 1].clone());
        }
    }

    pub fn media(&self) -> (usize, u64) {
        let is_db = |p: &str| DB_FILES.iter().any(|d| p.ends_with(d));
        self.delivery.files.iter().filter(|f| !is_db(&f.path)).fold((0, 0), |(n, b), f| (n + 1, b + f.size))
    }
}

/// "[3/120] Volume/Ordner/Datei" -> 3/120
pub fn progress_value(line: &str) -> f32 {
    line.strip_prefix('[')
        .and_then(|r| r.split_once(']'))
        .and_then(|(n, _)| n.split_once('/'))
        .and_then(|(a, b)| Some(a.parse::<f32>().ok()? / b.parse::<f32>().ok()?.max(1.0)))
        .unwrap_or(0.0)
}

pub fn format_size(bytes: u64, lang: &str) -> String {
    let (v, unit) = if bytes >= 1_000_000_000 { (bytes as f64 / 1e9, "GB") } else { (bytes as f64 / 1e6, "MB") };
    let s = format!("{v:.1} {unit}");
    if lang == "en" { s } else { s.replace('.', ",") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::{FileEntry, Folder, Volume};

    fn vol(name: &str, folders: &[(&str, &[&str])]) -> Volume {
        Volume {
            name: name.into(),
            path: String::new(),
            folders: folders
                .iter()
                .map(|(f, files)| Folder { name: (*f).into(), has_db: true, files: files.iter().map(|n| FileEntry { name: (*n).into(), size: 1, hash: (*n).into() }).collect() })
                .collect(),
        }
    }

    #[test]
    fn drives_are_preselected_by_name_assigned_otherwise_and_remembered() {
        let local = Manifest { system: "A".into(), volumes: vec![vol("Data", &[("1", &["a"])]), vol("Audio", &[("2", &["b"])])], ..Default::default() };
        let remote = Manifest { system: "B".into(), volumes: vec![vol("Data", &[]), vol("Audio_B", &[])], ..Default::default() };
        let mut c = Compare::new(local.clone(), remote.clone(), &Mappings::default());
        assert_eq!(c.drives.iter().map(|d| (d.name.as_str(), d.selected)).collect::<Vec<_>>(), [("Data", 1), ("Audio", 0)]);
        assert!(!c.ready());
        assert_eq!(media(&c.plan()).0, 1, "unassigned drive is not sent");
        c.drives[1].selected = 2; // Audio_B
        assert!(c.ready());
        assert_eq!(media(&c.plan()).0, 2);
        // a drive of the same name can go elsewhere
        c.drives[0].selected = 2; // Data -> Audio_B
        assert_eq!(c.plan().copies.iter().filter(|x| x.from_volume == "Data").map(|x| x.to_volume.as_str()).next(), Some("Audio_B"));
        let mut m = Mappings::default();
        c.remember(&mut m);
        let again = Compare::new(local, remote, &m);
        assert_eq!((again.drives[0].selected, again.drives[1].selected), (2, 2));
    }

    #[test]
    fn dont_send_is_respected_for_a_drive_of_the_same_name() {
        let local = Manifest { system: "A".into(), volumes: vec![vol("Data", &[("1", &["a"])])], ..Default::default() };
        let remote = Manifest { system: "B".into(), volumes: vec![vol("Data", &[])], ..Default::default() };
        let mut c = Compare::new(local, remote, &Mappings::default());
        c.drives[0].selected = c.drives[0].options.len() + 1; // Don't send
        assert_eq!(media(&c.plan()).0, 0);
    }

    #[test]
    fn progress_and_sizes() {
        assert_eq!(progress_value("[3/4] V/1/a.mxf"), 0.75);
        assert_eq!(progress_value("V/1"), 0.0);
        assert_eq!(format_size(3_600_000_000, "de"), "3,6 GB");
        assert_eq!(format_size(29_000_000, "en"), "29.0 MB");
    }
}
