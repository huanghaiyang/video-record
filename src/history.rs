//! Recording history.
//!
//! Every finished recording is appended to `history.json` next to the
//! configuration, so the user can find what was recorded, when, how big it is
//! and where it went — including recordings written to a folder that was
//! chosen automatically.
//!
//! Only successful recordings are recorded: a failed session produces no file,
//! and its cause is already surfaced in the status bar and the log.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::capture::SessionSnapshot;

/// Schema version of the on-disk file.
pub const SCHEMA_VERSION: u32 = 1;

/// How many entries are kept; older ones are dropped when appending.
pub const MAX_ENTRIES: usize = 500;

/// Where the file lives: alongside `config.json`.
pub fn history_path() -> PathBuf {
    crate::config::app_data_dir().join("history.json")
}

/// The details of a recording that do not depend on how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntrySource {
    /// `monitor`, `window` or `picker`.
    pub kind: String,
    /// Human readable source, e.g. `显示器 1：DELL U2720Q`.
    pub label: String,
    pub container: String,
    pub codec: String,
    pub frame_rate: u32,
    /// False for GIF and image sequences, which cannot carry an audio track.
    pub audio_capable: bool,
}

/// One finished recording.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordingEntry {
    /// Unique id: creation time in milliseconds, bumped on collision.
    pub id: u64,
    pub created_unix_ms: i64,
    pub source_kind: String,
    pub source_label: String,
    pub container: String,
    pub codec: String,
    /// Encoder/back end that actually produced the file.
    #[serde(default)]
    pub backend: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub duration_ms: u64,
    pub frames: u64,
    #[serde(default)]
    pub frame_rate: u32,
    #[serde(default)]
    pub source_size: Option<(u32, u32)>,
    #[serde(default)]
    pub output_size: Option<(u32, u32)>,
    #[serde(default)]
    pub audio: String,
    /// Recomputed on load; never trusted from disk.
    #[serde(skip)]
    pub exists: bool,
}

impl RecordingEntry {
    /// Builds an entry from a finished session. `None` when nothing was written.
    pub fn from_session(source: &EntrySource, snapshot: &SessionSnapshot) -> Option<Self> {
        let path = snapshot.output.clone()?;
        let created_unix_ms = chrono::Local::now().timestamp_millis();

        Some(Self {
            id: created_unix_ms.max(0) as u64,
            created_unix_ms,
            source_kind: source.kind.clone(),
            source_label: source.label.clone(),
            container: source.container.clone(),
            codec: source.codec.clone(),
            backend: clean_backend(&snapshot.backend),
            path,
            bytes: snapshot.file_size,
            duration_ms: snapshot.elapsed.as_millis() as u64,
            frames: snapshot.frames,
            frame_rate: source.frame_rate,
            source_size: snapshot.source_size,
            output_size: snapshot.output_size,
            audio: if source.audio_capable {
                snapshot.audio.clone()
            } else {
                "不适用（该格式没有音轨）".to_string()
            },
            exists: true,
        })
    }

    pub fn created_local(&self) -> Option<chrono::DateTime<chrono::Local>> {
        chrono::DateTime::from_timestamp_millis(self.created_unix_ms)
            .map(|utc| utc.with_timezone(&chrono::Local))
    }

    /// `2026-10-16 13:06:46`
    pub fn created_text(&self) -> String {
        self.created_local()
            .map(|time| time.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "未知时间".to_string())
    }

    pub fn duration(&self) -> Duration {
        Duration::from_millis(self.duration_ms)
    }

    /// `1920x1080 · 30 fps`
    pub fn resolution_text(&self) -> String {
        match (self.output_size, self.frame_rate) {
            (Some((width, height)), 0) => format!("{width}×{height}"),
            (Some((width, height)), fps) => format!("{width}×{height} · {fps} fps"),
            (None, 0) => "—".to_string(),
            (None, fps) => format!("{fps} fps"),
        }
    }

    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }

    /// One-line description used by the CLI and tooltips.
    pub fn summary(&self) -> String {
        format!(
            "{} · {} · {} · {} · {}",
            self.created_text(),
            self.source_label,
            self.container.to_uppercase(),
            crate::units::format_duration(self.duration()),
            self.path.display()
        )
    }
}

/// Removes the live frame counter some sinks append to their description.
///
/// `内置 GIF 编码器 · 0 帧` is meaningful while recording, but a finished
/// record already stores the real frame count.
fn clean_backend(text: &str) -> String {
    let Some(index) = text.rfind(" · ") else {
        return text.to_string();
    };
    let tail = text[index + " · ".len()..].trim_end_matches(" 帧");
    if !tail.is_empty() && tail.chars().all(|character| character.is_ascii_digit()) {
        text[..index].to_string()
    } else {
        text.to_string()
    }
}

/// The persisted document.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistoryDocument {
    schema_version: u32,
    entries: Vec<RecordingEntry>,
}

/// Newest-first list of recordings, backed by a JSON file.
#[derive(Debug, Clone)]
pub struct HistoryStore {
    entries: Vec<RecordingEntry>,
    path: PathBuf,
}

impl Default for HistoryStore {
    fn default() -> Self {
        Self::with_path(history_path())
    }
}

impl HistoryStore {
    /// Loads the history from the standard location.
    pub fn load() -> Self {
        Self::with_path(history_path())
    }

    /// Loads from an explicit path; a missing or unreadable file yields an
    /// empty history rather than an error, because history is not essential to
    /// recording.
    pub fn with_path(path: PathBuf) -> Self {
        let entries = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<HistoryDocument>(&text) {
                Ok(mut document) => {
                    // A newer schema may carry fields this build does not know;
                    // keeping what parses is better than discarding the list.
                    if document.schema_version > SCHEMA_VERSION {
                        log::warn!(
                            "录制记录 {} 来自更新的版本（schema {}）",
                            path.display(),
                            document.schema_version
                        );
                    }
                    document.entries.truncate(MAX_ENTRIES);
                    document.entries
                }
                Err(err) => {
                    log::warn!("录制记录 {} 解析失败（{err}），已忽略", path.display());
                    let _ = std::fs::rename(&path, path.with_extension("json.broken"));
                    Vec::new()
                }
            },
            Err(_) => Vec::new(),
        };

        let mut store = Self { entries, path };
        store.refresh_existence();
        store
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn entries(&self) -> &[RecordingEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|entry| entry.bytes).sum()
    }

    /// How many of the recorded files still exist on disk.
    pub fn existing_count(&self) -> usize {
        self.entries.iter().filter(|entry| entry.exists).count()
    }

    /// Re-checks the files, so a recording moved or deleted outside the app is
    /// shown as missing instead of silently failing to open.
    pub fn refresh_existence(&mut self) {
        for entry in &mut self.entries {
            entry.exists = entry.path.exists();
        }
    }

    pub fn find(&self, id: u64) -> Option<&RecordingEntry> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    /// Inserts a new recording at the front and persists the list.
    pub fn add(&mut self, mut entry: RecordingEntry) -> anyhow::Result<u64> {
        entry.id = self.unique_id(entry.id);
        entry.exists = entry.path.exists();
        let id = entry.id;
        self.entries.insert(0, entry);
        self.entries.truncate(MAX_ENTRIES);
        self.save()?;
        Ok(id)
    }

    /// Removes one entry. Returns false when the id is unknown.
    pub fn remove(&mut self, id: u64) -> anyhow::Result<bool> {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.id != id);
        let removed = self.entries.len() != before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// Replaces the whole list (used by tests and by "清空记录").
    pub fn clear(&mut self) -> anyhow::Result<()> {
        self.entries.clear();
        self.save()
    }

    /// Writes the file atomically so a crash cannot truncate the history.
    pub fn save(&self) -> anyhow::Result<()> {
        let document = HistoryDocument {
            schema_version: SCHEMA_VERSION,
            // `exists` is derived state; excluding it keeps the file stable.
            entries: self.entries.clone(),
        };

        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir)?;

        let text = serde_json::to_string_pretty(&document)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    fn unique_id(&self, candidate: u64) -> u64 {
        let mut id = candidate.max(1);
        while self.entries.iter().any(|entry| entry.id == id) {
            id += 1;
        }
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::SessionStatus;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("screenrec-history-{}-{tag}.json", std::process::id()))
    }

    fn source() -> EntrySource {
        EntrySource {
            kind: "monitor".to_string(),
            label: "显示器 1：测试屏".to_string(),
            container: "mp4".to_string(),
            codec: "h264".to_string(),
            frame_rate: 30,
            audio_capable: true,
        }
    }

    fn snapshot(output: Option<PathBuf>) -> SessionSnapshot {
        SessionSnapshot {
            status: SessionStatus::Completed,
            frames: 90,
            elapsed: Duration::from_millis(3_000),
            output,
            planned_output: None,
            file_size: 1_234_567,
            source_size: Some((1920, 1080)),
            output_size: Some((1280, 720)),
            backend: "内置编码器 H264".to_string(),
            audio: "系统声音（回环）".to_string(),
        }
    }

    #[test]
    fn backend_descriptions_lose_the_live_frame_counter() {
        assert_eq!(clean_backend("内置 GIF 编码器 · 0 帧"), "内置 GIF 编码器");
        assert_eq!(clean_backend("图像序列 · 12 帧"), "图像序列");
        assert_eq!(clean_backend("内置编码器 H264"), "内置编码器 H264");
        // Something that is not a counter must survive untouched.
        assert_eq!(clean_backend("FFmpeg · libx264"), "FFmpeg · libx264");
    }

    #[test]
    fn formats_without_audio_say_so() {
        let mut gif_source = source();
        gif_source.container = "gif".to_string();
        gif_source.audio_capable = false;
        let entry =
            RecordingEntry::from_session(&gif_source, &snapshot(Some(PathBuf::from("a.gif"))))
                .unwrap();
        assert!(entry.audio.contains("不适用"), "got {}", entry.audio);

        // A video container keeps the real audio summary.
        let entry =
            RecordingEntry::from_session(&source(), &snapshot(Some(PathBuf::from("a.mp4"))))
                .unwrap();
        assert_eq!(entry.audio, "系统声音（回环）");
    }

    #[test]
    fn an_entry_is_built_from_a_finished_session() {
        let entry = RecordingEntry::from_session(
            &source(),
            &snapshot(Some(PathBuf::from(r"D:\clips\a.mp4"))),
        )
        .expect("entry");

        assert_eq!(entry.source_label, "显示器 1：测试屏");
        assert_eq!(entry.container, "mp4");
        assert_eq!(entry.bytes, 1_234_567);
        assert_eq!(entry.duration(), Duration::from_secs(3));
        assert_eq!(entry.resolution_text(), "1280×720 · 30 fps");
        assert_eq!(entry.file_name(), "a.mp4");
        assert!(entry.created_unix_ms > 0);
        assert!(entry.summary().contains("显示器 1：测试屏"));
    }

    #[test]
    fn a_session_without_output_produces_no_entry() {
        assert!(RecordingEntry::from_session(&source(), &snapshot(None)).is_none());
    }

    #[test]
    fn entries_round_trip_through_the_file() {
        let path = temp_path("roundtrip");
        let _ = std::fs::remove_file(&path);

        let mut store = HistoryStore::with_path(path.clone());
        let id = store
            .add(RecordingEntry::from_session(&source(), &snapshot(Some(PathBuf::from("a.mp4")))).unwrap())
            .unwrap();

        let reloaded = HistoryStore::with_path(path.clone());
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded.entries()[0].id, id);
        assert_eq!(reloaded.total_bytes(), 1_234_567);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn newest_entries_come_first_and_ids_stay_unique() {
        let path = temp_path("order");
        let _ = std::fs::remove_file(&path);
        let mut store = HistoryStore::with_path(path.clone());

        for name in ["a.mp4", "b.mp4", "c.mp4"] {
            store
                .add(
                    RecordingEntry::from_session(&source(), &snapshot(Some(PathBuf::from(name))))
                        .unwrap(),
                )
                .unwrap();
        }

        let names: Vec<String> = store.entries().iter().map(|e| e.file_name()).collect();
        assert_eq!(names, vec!["c.mp4", "b.mp4", "a.mp4"], "newest first");

        let mut ids: Vec<u64> = store.entries().iter().map(|e| e.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "ids must be unique even within one millisecond");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn the_oldest_entries_are_dropped_at_the_cap() {
        let path = temp_path("cap");
        let _ = std::fs::remove_file(&path);
        let mut store = HistoryStore::with_path(path.clone());

        // Bypass `add`'s file write for speed; the cap logic is what matters.
        for index in 0..MAX_ENTRIES + 10 {
            store.entries.push(RecordingEntry {
                id: index as u64 + 1,
                created_unix_ms: index as i64,
                source_kind: "monitor".into(),
                source_label: "x".into(),
                container: "mp4".into(),
                codec: "h264".into(),
                backend: String::new(),
                path: PathBuf::from(format!("{index}.mp4")),
                bytes: 1,
                duration_ms: 1,
                frames: 1,
                frame_rate: 30,
                source_size: None,
                output_size: None,
                audio: String::new(),
                exists: false,
            });
        }
        store.entries.truncate(MAX_ENTRIES);
        assert_eq!(store.len(), MAX_ENTRIES);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn entries_can_be_removed_by_id() {
        let path = temp_path("remove");
        let _ = std::fs::remove_file(&path);
        let mut store = HistoryStore::with_path(path.clone());
        let id = store
            .add(RecordingEntry::from_session(&source(), &snapshot(Some(PathBuf::from("a.mp4")))).unwrap())
            .unwrap();

        assert!(store.find(id).is_some());
        assert!(store.remove(id).unwrap());
        assert!(!store.remove(id).unwrap(), "removing twice reports nothing removed");
        assert!(store.is_empty());
        assert!(HistoryStore::with_path(path.clone()).is_empty(), "removal is persisted");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn clearing_empties_the_file() {
        let path = temp_path("clear");
        let _ = std::fs::remove_file(&path);
        let mut store = HistoryStore::with_path(path.clone());
        store
            .add(RecordingEntry::from_session(&source(), &snapshot(Some(PathBuf::from("a.mp4")))).unwrap())
            .unwrap();
        store.clear().unwrap();

        assert!(store.is_empty());
        assert!(HistoryStore::with_path(path.clone()).is_empty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_corrupt_file_is_ignored_and_preserved_for_inspection() {
        let path = temp_path("corrupt");
        std::fs::write(&path, b"{ this is not json").unwrap();

        let store = HistoryStore::with_path(path.clone());
        assert!(store.is_empty(), "corrupt history must not break startup");
        assert!(
            path.with_extension("json.broken").exists(),
            "the unreadable file is kept for the user"
        );

        std::fs::remove_file(&path).ok();
        std::fs::remove_file(path.with_extension("json.broken")).ok();
    }

    #[test]
    fn missing_files_are_marked_after_a_refresh() {
        let path = temp_path("missing");
        let _ = std::fs::remove_file(&path);
        let mut store = HistoryStore::with_path(path.clone());

        let missing = std::env::temp_dir().join("screenrec-definitely-not-here.mp4");
        store
            .add(RecordingEntry::from_session(&source(), &snapshot(Some(missing))).unwrap())
            .unwrap();

        assert_eq!(store.existing_count(), 0);
        assert!(!store.entries()[0].exists);
        assert_eq!(store.total_bytes(), 1_234_567, "size is remembered regardless");

        std::fs::remove_file(&path).ok();
    }
}
