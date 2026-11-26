use aqua_fetch::model::{Item, Saved, Status, file_name, part_path, safe_rel, unique};
use aqua_fetch::{engine, s3, store};
use iced::futures::{channel::mpsc, stream};
use iced::widget::{button, column, container, image, progress_bar, row, scrollable, slider, space, stack, text, text_editor};
use iced::{Center, Color, ContentFit, Element, Fill, Font, Subscription, Task, Theme, task};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

mod theme;

/// Painted background. `None` falls back to the built-in sky gradient.
const BACKGROUND: Option<&[u8]> = Some(include_bytes!("../assets/background.jpg"));
const ICON: &[u8] = include_bytes!("../assets/icon.png");

fn main() -> iced::Result {
    iced::application(App::new, App::update, App::view)
        .title("Aqua Fetch")
        .subscription(App::subscription)
        .theme(|_: &App| Theme::Light)
        .default_font(Font::with_name("Lato"))
        .window(iced::window::Settings {
            size: (980.0, 720.0).into(),
            icon: iced::window::icon::from_file_data(ICON, None).ok(),
            ..Default::default()
        })
        .run()
}

#[derive(Debug, Clone)]
enum Msg {
    Edit(text_editor::Action),
    Add,
    Import,
    Imported(Option<String>),
    PickDir,
    DirPicked(Option<PathBuf>),
    Parallel(u8),
    StartAll,
    PauseAll,
    ClearDone,
    Pause(u64),
    Resume(u64),
    Remove(u64),
    /// Bucket name and its objects, for a prefix row.
    /// Row id, run number, then the bucket name and its objects.
    Listed(u64, u64, Result<(String, Vec<s3::Object>), String>),
    Ev(u64, engine::Event),
    /// Row id, run number, result.
    Finished(u64, u64, Result<(), String>),
    Tick,
}

struct App {
    saved: Saved,
    state_path: PathBuf,
    input: text_editor::Content,
    client: reqwest::Client,
    /// Row id to (run number, handle). The run number lets late messages from a stopped run be told apart.
    running: HashMap<u64, (u64, task::Handle)>,
    runs: u64,
    /// Bytes at the previous tick, for speed.
    last: HashMap<u64, u64>,
    note: String,
    background: Option<image::Handle>,
    icon: image::Handle,
}

/// One pasted line: `<link> [sha256=<hex> | <hex>]`. `None` for blanks and `#` comments.
fn parse_line(line: &str) -> Option<Result<(String, Option<String>), String>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut parts = line.split_whitespace();
    let url = parts.next()?;
    if !["http://", "https://", "s3://"].iter().any(|p| url.starts_with(p)) {
        return Some(Err(format!("not a link: {url}")));
    }
    let sha = match parts.next() {
        None => None,
        Some(p) => {
            let h = p.strip_prefix("sha256=").unwrap_or(p).to_ascii_lowercase();
            if h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Some(Err(format!("bad sha256 for {url}")));
            }
            Some(h)
        }
    };
    Some(Ok((url.to_owned(), sha)))
}

/// Folder name for the button: `~` for home, and only the tail of a long path.
fn short(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    let s = match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && s.starts_with(&h) => format!("~{}", &s[h.len()..]),
        _ => s,
    };
    let n = s.chars().count();
    if n > 36 { format!("...{}", s.chars().skip(n - 33).collect::<String>()) } else { s }
}

fn human(b: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut i) = (b as f64, 0);
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", units[i]) }
}

impl App {
    fn new() -> (Self, Task<Msg>) {
        let state_path = store::path();
        let (saved, bad) = store::load(&state_path);
        let mut app = App {
            saved,
            state_path,
            input: text_editor::Content::new(),
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(20))
                .read_timeout(Duration::from_secs(60))
                .build()
                .expect("TLS setup"),
            running: HashMap::new(),
            runs: 0,
            last: HashMap::new(),
            note: if bad { "The saved queue could not be read; it was kept as queue.json.bad.".into() } else { String::new() },
            background: BACKGROUND.map(image::Handle::from_bytes),
            icon: image::Handle::from_bytes(ICON),
        };
        let task = app.pump();
        (app, task)
    }

    fn item(&mut self, id: u64) -> Option<&mut Item> {
        self.saved.items.iter_mut().find(|i| i.id == id)
    }

    fn new_item(&mut self, url: String, dest: PathBuf, sha256: Option<String>, total: Option<u64>) -> Item {
        let id = self.saved.next_id;
        self.saved.next_id += 1;
        Item { id, url, dest, status: Status::Queued, bytes: 0, total, etag: None, sha256, speed: 0.0 }
    }

    /// Every path the queue writes to: each row's file and its `.part`.
    fn used(&self) -> HashSet<PathBuf> {
        self.saved.items.iter().flat_map(|i| [i.dest.clone(), part_path(&i.dest)]).collect()
    }

    /// A path under the download folder where neither it nor its `.part` clashes with
    /// a file on disk (ours or another program's) or with another row. Claims it in `used`.
    fn free_dest(&self, rel: PathBuf, used: &mut HashSet<PathBuf>) -> PathBuf {
        let dest = unique(self.saved.dir.join(rel), &|p| {
            let part = part_path(p);
            p.exists() || part.exists() || used.contains(p) || used.contains(&part)
        });
        used.extend([dest.clone(), part_path(&dest)]);
        dest
    }

    fn add_links(&mut self, text: &str) {
        let (mut added, mut dupes, mut bad) = (0, 0, Vec::new());
        let mut used = self.used();
        for line in text.lines() {
            let (url, sha) = match parse_line(line) {
                None => continue,
                Some(Err(e)) => {
                    bad.push(e);
                    continue;
                }
                Some(Ok(v)) => v,
            };
            // s3://bucket/key becomes its public https URL; a prefix row keeps its link and gets no file.
            let (url, is_file) = match s3::parse(&url) {
                Some(s3::Link::Prefix { .. }) => (url, false),
                Some(s3::Link::File(u)) => (u, true),
                None => (url, true),
            };
            if self.saved.items.iter().any(|i| i.url == url) {
                dupes += 1;
                continue;
            }
            let dest = if is_file { self.free_dest(file_name(&url), &mut used) } else { PathBuf::new() };
            let item = self.new_item(url, dest, sha, None);
            self.saved.items.push(item);
            added += 1;
        }
        self.note = format!("Added {added}.");
        if dupes > 0 {
            self.note += &format!(" Skipped {dupes} already in the queue.");
        }
        if !bad.is_empty() {
            self.note += &format!(" Ignored {}: {}", bad.len(), bad.join("; "));
        }
    }

    /// Replaces a listed prefix row with one row per object, keeping `bucket/key` as the folder layout.
    fn listed(&mut self, id: u64, bucket: &str, objects: Vec<s3::Object>) {
        let Some(pos) = self.saved.items.iter().position(|i| i.id == id) else { return };
        if objects.is_empty() {
            self.saved.items[pos].status = Status::Failed("No objects under this prefix".into());
            return;
        }
        self.saved.items.remove(pos);
        let mut urls: HashSet<String> = self.saved.items.iter().map(|i| i.url.clone()).collect();
        let mut used = self.used();
        let (mut new, mut odd) = (Vec::new(), 0);
        for o in objects {
            // HTTP clients collapse "." and ".." path parts, so such a key would fetch a different object.
            if o.key.split('/').any(|c| c == "." || c == "..") {
                odd += 1;
                continue;
            }
            if !urls.insert(o.url.clone()) {
                continue;
            }
            let rel = safe_rel(bucket).join(safe_rel(&o.key));
            let wanted = self.saved.dir.join(&rel);
            // Same name and size already on disk: treat as downloaded, so re-adding a data set only fetches what is missing.
            let have = !used.contains(&wanted) && wanted.metadata().is_ok_and(|m| m.is_file() && m.len() == o.size);
            let dest = if have {
                used.insert(wanted.clone());
                wanted
            } else {
                self.free_dest(rel, &mut used)
            };
            let mut item = self.new_item(o.url, dest, None, Some(o.size));
            if have {
                item.status = Status::Done;
                item.bytes = o.size;
            }
            new.push(item);
        }
        self.note = format!("Listed {} objects from {bucket}.", new.len());
        if odd > 0 {
            self.note += &format!(" Skipped {odd} with \".\" or \"..\" in the key.");
        }
        self.saved.items.splice(pos..pos, new);
    }

    fn stop(&mut self, id: u64) {
        if let Some((_, h)) = self.running.remove(&id) {
            h.abort();
        }
        self.last.remove(&id);
    }

    fn pause(&mut self, id: u64) {
        self.stop(id);
        if let Some(i) = self.item(id) {
            i.status = Status::Paused;
            i.speed = 0.0;
        }
    }

    /// Starts listings right away and downloads up to the "at once" limit.
    fn pump(&mut self) -> Task<Msg> {
        let parallel = self.saved.parallel;
        let mut active =
            self.saved.items.iter().filter(|i| matches!(i.status, Status::Downloading | Status::Verifying)).count();
        let mut tasks = Vec::new();
        for it in &mut self.saved.items {
            if it.status != Status::Queued {
                continue;
            }
            let (id, client) = (it.id, self.client.clone());
            let run = self.runs + 1;
            // Only prefix rows have no file path, so only they need parsing here.
            let prefix = if it.dest.as_os_str().is_empty() { s3::parse(&it.url) } else { None };
            let task = if let Some(s3::Link::Prefix { bucket, region, prefix }) = prefix {
                it.status = Status::Listing;
                Task::perform(
                    async move { s3::list(&client, &bucket, region.as_deref(), &prefix).await.map(|o| (bucket, o)) },
                    move |r| Msg::Listed(id, run, r),
                )
            } else if active < parallel {
                active += 1;
                it.status = Status::Downloading;
                let item = it.clone();
                let (tx, rx) = mpsc::unbounded();
                let job = async move {
                    let r = engine::download(&client, &item, move |e| {
                        let _ = tx.unbounded_send(Msg::Ev(id, e));
                    })
                    .await;
                    Msg::Finished(id, run, r)
                };
                Task::stream(stream::select(rx, stream::once(job)))
            } else {
                continue;
            };
            let (task, handle) = task.abortable();
            self.runs = run;
            if let Some((_, old)) = self.running.insert(id, (run, handle)) {
                old.abort();
            }
            tasks.push(task);
        }
        Task::batch(tasks)
    }

    fn update(&mut self, msg: Msg) -> Task<Msg> {
        match msg {
            // These three change nothing worth saving, so they return early.
            Msg::Edit(a) => {
                self.input.perform(a);
                return Task::none();
            }
            Msg::Tick => {
                for i in &mut self.saved.items {
                    if i.status == Status::Downloading {
                        let prev = self.last.insert(i.id, i.bytes).unwrap_or(i.bytes);
                        i.speed = i.speed * 0.5 + i.bytes.saturating_sub(prev) as f64 * 0.5;
                    }
                }
                return Task::none();
            }
            Msg::Ev(id, engine::Event::Progress { bytes, total }) => {
                if let Some(i) = self.item(id) {
                    i.bytes = bytes;
                    i.total = total.or(i.total);
                }
                return Task::none();
            }
            Msg::Ev(id, engine::Event::Etag(e)) => {
                if let Some(i) = self.item(id) {
                    i.etag = Some(e);
                }
            }
            Msg::Ev(id, engine::Event::Verifying) => {
                if let Some(i) = self.item(id).filter(|i| i.status == Status::Downloading) {
                    i.status = Status::Verifying;
                    i.speed = 0.0;
                }
            }
            Msg::Add => {
                let t = self.input.text();
                self.add_links(&t);
                self.input = text_editor::Content::new();
            }
            Msg::Import => {
                return Task::perform(
                    async {
                        let f = rfd::AsyncFileDialog::new().set_title("Import links").pick_file().await?;
                        tokio::fs::read_to_string(f.path()).await.ok()
                    },
                    Msg::Imported,
                );
            }
            Msg::Imported(t) => match t {
                Some(t) => self.add_links(&t),
                None => self.note = "Nothing imported.".into(),
            },
            Msg::PickDir => {
                return Task::perform(
                    async {
                        let d = rfd::AsyncFileDialog::new().set_title("Download folder").pick_folder().await?;
                        Some(d.path().to_owned())
                    },
                    Msg::DirPicked,
                );
            }
            Msg::DirPicked(d) => {
                if let Some(d) = d {
                    self.saved.dir = d;
                    self.note = "New links will be saved to the new folder.".into();
                }
            }
            Msg::Parallel(n) => self.saved.parallel = n as usize,
            Msg::StartAll => {
                for i in &mut self.saved.items {
                    if matches!(i.status, Status::Paused | Status::Failed(_)) {
                        i.status = Status::Queued;
                    }
                }
            }
            Msg::PauseAll => {
                let ids: Vec<u64> =
                    self.saved.items.iter().filter(|i| is_pending(&i.status)).map(|i| i.id).collect();
                for id in ids {
                    self.pause(id);
                }
                // Return before pump(), or it would start them again.
                self.save();
                return Task::none();
            }
            Msg::ClearDone => self.saved.items.retain(|i| i.status != Status::Done),
            Msg::Pause(id) => self.pause(id),
            Msg::Resume(id) => {
                if let Some(i) = self.item(id).filter(|i| matches!(i.status, Status::Paused | Status::Failed(_))) {
                    i.status = Status::Queued;
                }
            }
            Msg::Remove(id) => {
                self.stop(id);
                if let Some(pos) = self.saved.items.iter().position(|i| i.id == id) {
                    let it = self.saved.items.remove(pos);
                    // An unfinished row leaves only a useless partial file behind; finished files are never touched.
                    if it.status != Status::Done && !it.dest.as_os_str().is_empty() {
                        let _ = std::fs::remove_file(part_path(&it.dest));
                    }
                }
            }
            Msg::Listed(id, run, r) => {
                if self.running.get(&id).is_none_or(|(now, _)| *now != run) {
                    return Task::none(); // from a listing that was paused or removed
                }
                self.running.remove(&id);
                match r {
                    Ok((bucket, objects)) => self.listed(id, &bucket, objects),
                    Err(e) => {
                        if let Some(i) = self.item(id) {
                            i.status = Status::Failed(e);
                        }
                    }
                }
            }
            Msg::Finished(id, run, r) => {
                let current = self.running.get(&id).is_some_and(|(now, _)| *now == run);
                // Success is true whenever it arrives: the file is in place, even if the row was
                // paused or restarted a moment earlier. A failure only counts for the current run.
                if r.is_err() && !current {
                    return Task::none();
                }
                self.stop(id);
                if let Some(i) = self.item(id) {
                    i.speed = 0.0;
                    match r {
                        Ok(()) => {
                            i.status = Status::Done;
                            i.bytes = i.total.unwrap_or(i.bytes);
                        }
                        Err(e) => i.status = Status::Failed(e),
                    }
                }
            }
        }
        let task = self.pump();
        self.save();
        task
    }

    // shortcut: rewrites the whole queue on each state change, fine up to a few thousand rows; debounce it beyond that.
    fn save(&mut self) {
        if let Err(e) = store::save(&self.state_path, &self.saved) {
            self.note = format!("Could not save the queue: {e}");
        }
    }

    fn subscription(&self) -> Subscription<Msg> {
        if self.saved.items.iter().any(|i| i.status == Status::Downloading) {
            iced::time::every(Duration::from_secs(1)).map(|_| Msg::Tick)
        } else {
            Subscription::none()
        }
    }

    fn view(&self) -> Element<'_, Msg> {
        let items = &self.saved.items;
        let done = items.iter().filter(|i| i.status == Status::Done).count();
        let speed: f64 = items.iter().map(|i| i.speed).sum();

        let header = row![
            image(self.icon.clone()).width(48).height(48),
            text("Aqua Fetch").size(36).color(theme::WHITE),
            space().width(Fill),
            // On a glass pill, because the painted sky is nearly white in this corner.
            container(text(format!("{done} of {} done   {}/s", items.len(), human(speed as u64))).size(16))
                .padding([6, 16])
                .style(theme::row_glass),
        ]
        .spacing(12)
        .align_y(Center);

        let add = container(
            column![
                text_editor(&self.input)
                    .placeholder("Paste links here, one per line: https://..., s3://bucket/key, or s3://bucket/folder/ for everything in it.\nOptional checksum after a link: sha256=<hex>")
                    .on_action(Msg::Edit)
                    .height(110)
                    .padding(10)
                    .style(theme::editor),
                row![
                    btn("Add to queue", theme::GREEN, Msg::Add),
                    btn("Import .txt", theme::SILVER, Msg::Import),
                    space().width(Fill),
                    text(format!("At once: {}", self.saved.parallel)).color(theme::INK),
                    slider(1..=16u8, self.saved.parallel as u8, Msg::Parallel).width(130).style(theme::knob),
                    btn(format!("Save to: {}", short(&self.saved.dir)), theme::AQUA, Msg::PickDir),
                ]
                .spacing(10)
                .align_y(Center),
            ]
            .spacing(12),
        )
        .padding(16)
        .style(theme::glass);

        // shortcut: every row is rebuilt each frame, fine up to a few thousand rows; switch to a lazy list beyond that.
        let list: Element<'_, Msg> = if items.is_empty() {
            container(text("No downloads yet. Paste some links above.").size(18).color(theme::INK_SOFT))
                .center(Fill)
                .into()
        } else {
            scrollable(column(items.iter().map(|i| self.row(i))).spacing(8).padding([0, 14]))
                .height(Fill)
                .style(theme::scroll)
                .into()
        };

        let queue = container(
            column![
                row![
                    btn("Start all", theme::GREEN, Msg::StartAll),
                    btn("Pause all", theme::SILVER, Msg::PauseAll),
                    btn("Clear finished", theme::SILVER, Msg::ClearDone),
                    space().width(Fill),
                    text(&self.note).size(14).color(theme::INK_SOFT),
                ]
                .spacing(10)
                .align_y(Center),
                list,
            ]
            .spacing(12),
        )
        .padding(16)
        .height(Fill)
        .style(theme::glass);

        let back: Element<'_, Msg> = match &self.background {
            Some(h) => image(h.clone()).content_fit(ContentFit::Cover).width(Fill).height(Fill).into(),
            None => container(space()).width(Fill).height(Fill).style(theme::sky).into(),
        };
        stack![back, column![header, add, queue].spacing(16).padding(22)].into()
    }

    fn row<'a>(&'a self, i: &'a Item) -> Element<'a, Msg> {
        let name = if i.dest.as_os_str().is_empty() {
            i.url.clone()
        } else {
            i.dest.strip_prefix(&self.saved.dir).unwrap_or(&i.dest).display().to_string()
        };
        let (label, color): (String, Color) = match &i.status {
            Status::Queued => ("Queued".into(), theme::SILVER),
            Status::Listing => ("Listing bucket...".into(), theme::AQUA),
            Status::Downloading => (format!("{}/s", human(i.speed as u64)), theme::GREEN),
            Status::Verifying => ("Checking SHA-256...".into(), theme::AQUA),
            Status::Paused => ("Paused".into(), theme::AMBER),
            Status::Done => ("Done".into(), theme::AQUA),
            Status::Failed(_) => ("Failed".into(), theme::RED),
        };
        let fraction = match (&i.status, i.total) {
            (Status::Done, _) => 1.0,
            (_, Some(t)) if t > 0 => i.bytes as f32 / t as f32,
            _ => 0.0,
        };
        let size = match i.total {
            Some(t) if i.status == Status::Done => human(t),
            Some(t) => format!("{} of {}", human(i.bytes), human(t)),
            None if i.bytes > 0 => human(i.bytes),
            None => String::new(),
        };
        let action = match i.status {
            Status::Done => None,
            Status::Paused => Some(small("Resume", theme::AQUA, Msg::Resume(i.id))),
            Status::Failed(_) => Some(small("Retry", theme::AMBER, Msg::Resume(i.id))),
            _ => Some(small("Pause", theme::SILVER, Msg::Pause(i.id))),
        };
        // The reason gets its own line: it can be long (two hashes on a checksum mismatch).
        let (failed, why) = match &i.status {
            Status::Failed(e) => (true, Some(text(e).size(13).color(theme::RED))),
            _ => (false, None),
        };
        container(
            column![
                row![
                    text(name).size(15).color(theme::INK).width(Fill).wrapping(text::Wrapping::WordOrGlyph),
                    text(size).size(13).color(theme::INK_SOFT),
                    text(label).size(13).color(if failed { theme::RED } else { theme::INK_SOFT }),
                ]
                .push(action)
                .push(small("Remove", theme::SILVER, Msg::Remove(i.id)))
                .spacing(12)
                .align_y(Center),
                why,
                progress_bar(0.0..=1.0, fraction).girth(12).style(theme::progress(color)),
            ]
            .spacing(8),
        )
        .padding([10, 14])
        .style(theme::row_glass)
        .into()
    }
}

fn is_pending(s: &Status) -> bool {
    matches!(s, Status::Queued | Status::Listing | Status::Downloading | Status::Verifying)
}

fn btn<'a>(label: impl text::IntoFragment<'a>, color: Color, msg: Msg) -> Element<'a, Msg> {
    button(text(label).size(15)).padding([8, 18]).style(theme::glossy(color)).on_press(msg).into()
}

fn small<'a>(label: &'a str, color: Color, msg: Msg) -> Element<'a, Msg> {
    button(text(label).size(13)).padding([4, 12]).style(theme::glossy(color)).on_press(msg).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines() {
        let sha = "a".repeat(64);
        assert!(parse_line("  ").is_none());
        assert!(parse_line("# note").is_none());
        assert_eq!(parse_line(" https://h/x ").unwrap(), Ok(("https://h/x".into(), None)));
        assert_eq!(
            parse_line(&format!("s3://b/k sha256={}", sha.to_uppercase())).unwrap(),
            Ok(("s3://b/k".into(), Some(sha.clone())))
        );
        assert_eq!(parse_line(&format!("https://h/x {sha}")).unwrap(), Ok(("https://h/x".into(), Some(sha))));
        assert!(parse_line("ftp://h/x").unwrap().is_err());
        assert!(parse_line("https://h/x sha256=nope").unwrap().is_err());
    }

    #[test]
    fn sizes() {
        assert_eq!(short(std::path::Path::new("/data/sets")), "/data/sets");
        assert_eq!(short(std::path::Path::new(&format!("/{}", "x".repeat(60)))).chars().count(), 36);
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1536), "1.5 KB");
        assert_eq!(human(5 * 1024 * 1024 * 1024), "5.0 GB");
    }
}
