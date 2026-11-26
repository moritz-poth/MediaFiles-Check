# Drift report – MediaFiles Check, 2026-11-25

Documents checked: `CLAUDE.md`, `README.md`, `docs/help/en.md` (de/fr/es have the same 20 sections and line count as en; their wording was not compared line by line). Code state: `main` at the beta.11 bump (0b1df84 + version commit).

## CLAUDE.md

- **Projects section (line 16):** still describes the old windows as current – "ProjectsWindow is 680 px wide, `MissingList` rows open per group (`ProjectsState.open`)" and AMA files "in their own `AmaWindow` ('Show AMA clips…' in the result, own open set `ProjectsState::ama_open`)". `ProjectsWindow`, `AmaWindow` and `MissingList` do not exist in the code; missing media and AMA clips open in `MediaWindow` (kind 0/1). The same section later says the old `MissingList` is gone, so the section contradicts itself.
- **UI section:** "`MediaWindow` (900 × 680, Show media… / Show clips…)" – the window is `preferred-width: 1240px`, `preferred-height: 720px`, `min-width: 900px`; the buttons are "Show media…" and "Show AMA clips…" (no "Show clips…" any more).
- **UI section:** settings tabs listed as "General / Updates / Support / About" – the code has a fifth tab, Rebalancing (`tab == 4`, shown second), which the Rebalancing section mentions.
- **Main window: tabs and steps:** "Tabs (index): 0 Exchange, 1 Check, 2 Projects, 3 Maintain, 4 Tasks – shown in the order Exchange, Check, Maintain, Projects, Tasks", "sidebar (172 px", "`page0`…`page4`", "Projects, Exchange and Tasks keep their state in globals … drawn by `ProjectsPane`/`ExchangePane`/`TasksPane`" – the code has tabs 6 Scan, 0 Exchange, 1 Check DBs, 3 Maintain MXFs, 2 Project links, 5 Media (tab 4 unused), a 204 px sidebar (`width: 204px`), no `page4` and no `TasksPane`; `Tasks` has no `view`. "One set of drive switches for Check and Exchange" – Check has no drive list any more (central scan).
- **Tasks section:** "Live entries (not saved): runs with databases not rewritten (Start = `rebalance-db`/`dup-db`) and folders waiting to be renamed back" – the Scan-tab section below says these live entries are no longer repeated in Tasks; the code shows only saved tasks (`TasksView`).
- **First launch:** "explains the five tabs" – the app has six; the welcome card lists Scan, Exchange, Check, Maintain, Projects (old short names, no Media) while the tabs are called Check DBs, Maintain MXFs, Project links. Code and doc both lag behind the tab rename and the Media tab.
- **Support section:** "the self-test list has a fixed height of 140 px" – the code uses `height: 200px` (the UI section also says 200).
- **Media tab:** "Last 5 scans kept in `media-scans/`" – `SCANS_KEPT = 20` (the Scan-tab section says 20).
- **avb section:** "`mfcheck --bins`, `MFC_DRIVES=a:b` adds the ID match" – `src/main.rs` has no `--bins` option (only `--de` and `--projects FOLDER [REQUEST]`); `MFC_DRIVES` belongs to `--projects`.
- **Layout rules (2026-11-15):** "Drive lists look the same everywhere (`DriveList`: …) – Check, Exchange and Media share the switches" – `DriveList` does not exist any more (the Check section says it was removed); drive switches live only on the Scan page.
- **Duplicates section:** "History = page `runs` with `runs-kind` 1" – there is no `runs-kind` property; `Page.runs` is Maintain › History with both run kinds in one list (`history`, `RunRow.kind`).
- **Missing links:** the Exchange section names `wire_exchange` and the hardening helpers but not the files that hold the I/O tests' helpers (`tests/common/mod.rs`) beyond one mention; the error `Note`/`tab-error` mechanism is described only inside the Exchange › Import paragraph, not in the UI section where a reader looks for message styles.

## README.md

- **Maintain MXFs bullet:** starts with "*Rename for Media Composer*" as a Maintain MXFs tool – renaming moved to Check DBs › New databases (code: `Page.maintain` belongs to tab 1; Maintain MXFs steps are Rebalancing / Duplicates / History).
- **Project links bullet:** "Render and AMA-linked files are only counted" – AMA-linked files are also listed (Missing media › **Show AMA clips…** opens `MediaWindow` kind 1).
- **Intro / Exchange bullet:** no mention of the Scan tab as its own first tab, although "One window with six tabs" counts it; the bullet list has five entries.

## docs/help/en.md

- **"What MediaFiles Check does":** "**Maintain MXFs** has the tools that change something: rename folders for Media Composer, rebalancing, duplicates, and their history" – renaming is under Check DBs › New databases (the manual's own section "Rename for Media Composer" says so). The list also leaves out the Scan tab.
- **Updates:** "A new version shows as a bar under the tabs" – since 0b1df84 it is a floating card at the bottom of the main window with × (`UpdateCard`). Same sentence in de/fr/es.
- **The window:** "Every scan (Scan, Check, Rebalancing, Duplicates, Projects, Media) shows the time left …" – Project links and Media have no scan of their own any more; they use the Scan tab's scan.
- **Check the MXF databases:** the finding names ("**No database**", "**Database is empty**", "**Database unreadable**", "**Entries in the database without a file**") differ from the app's wording (`Finding::describe`: "no database (n …)", "database is empty (…)", "database unreadable: …", "n entries in the database without a file").
- **Messages:** errors now appear as a note at the bottom of the window with × (`Note`, all tabs) – the manual does not describe it.
