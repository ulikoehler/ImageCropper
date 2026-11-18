//! `imagecropper <subcommand> --serve=ADDR:PORT` — web-UI server.
//!
//! Two WebSocket channels: `/data` (control, events, thumbnail push) and
//! `/bulk` (manifest + full-resolution images). HTTP only serves the embedded
//! client assets.

pub mod actions;
pub mod http;
pub mod io_gate;
pub mod manifest;
pub mod preview_store;
pub mod proto;
pub mod push;
pub mod watch;
mod ws_bulk;
mod ws_data;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use axum::{routing::get, Router};
use rand::seq::SliceRandom;
use tokio::sync::broadcast;

use crate::fs_utils::par_map;

pub use actions::{Action, ActionDone, ActionEnvelope, ActionKind, ActionQueue, RotateOp};
pub use io_gate::IoGate;
pub use manifest::{Manifest, ManifestItem, ManifestPatch, ServeMode};
pub use preview_store::{Preview, PreviewStore};
pub use proto::*;
pub use push::PushQueue;

/// Server-wide events broadcast to every connected /data client.
#[derive(Debug, Clone)]
pub enum ServerEvent {
    /// JSON-encoded client event (`{"ev": …}`).
    Json(String),
}

pub struct ServerState {
    pub manifest: Arc<RwLock<Manifest>>,
    /// Scan roots (also embedded in the manifest for clients).
    pub roots: Vec<PathBuf>,
    pub previews: Arc<PreviewStore>,
    pub actions: Arc<ActionQueue>,
    pub events: broadcast::Sender<ServerEvent>,
    pub mode: ServeMode,
    pub mouse_select: crate::cli::MouseSelectMode,
    pub token: Option<String>,
    pub initial_credit: u64,
    /// Bumped on every resync broadcast — per-client push queues reset on it
    /// because the resync reassigned all manifest ids.
    pub resync_seq: std::sync::atomic::AtomicU64,
    /// Paths that left the album (deleted/moved) with the signature they had
    /// when they left. A rescan that listed the directory *before* the op
    /// landed re-inserts them otherwise — that is how already-deleted files
    /// used to come back.
    gone: std::sync::Mutex<std::collections::HashMap<PathBuf, (u64, std::time::Instant)>>,
    /// Client-first IO priority: live client requests mark it, background
    /// work (deletes/moves, crops, speculative preload) yields to it.
    pub io_gate: Arc<IoGate>,
    /// Set when the manifest changed — the pinned (RAM-resident) thumbnail
    /// set has to be recomputed.
    pins_dirty: std::sync::atomic::AtomicBool,
    /// Counters behind `/debugz` — the web client only sees its own state;
    /// this answers "is the server still scanning/pushing/producing?".
    pub dbg: DbgStats,
    /// Sort keys memoized by (path, sig): continuous re-sorting during a
    /// scan must not re-stat/re-parse every item on each pass — the
    /// signature already covers (len, mtime), so a hit is always exact.
    /// The u128 packs the order key (size → len, mtime → nanos, EXIF →
    /// packed datetime); `None` = no key (files without EXIF sort last).
    key_cache: std::sync::Mutex<HashMap<PathBuf, (u64, Option<u128>)>>,
}

/// Lightweight instrumentation for `/debugz`. Everything is cheap (atomics
/// or a small mutexed map) — it must never gate the hot path.
#[derive(Default)]
pub struct DbgStats {
    /// Thumbnails pushed by the sweep path / WANT replies, and their bytes.
    pub sweep_sent: std::sync::atomic::AtomicU64,
    pub sweep_bytes: std::sync::atomic::AtomicU64,
    pub want_sent: std::sync::atomic::AtomicU64,
    /// CL_WANT ids received across all connections.
    pub want_recv: std::sync::atomic::AtomicU64,
    /// CL_BAD_PREVIEW corrupt-preview reports honored.
    pub bad_recv: std::sync::atomic::AtomicU64,
    /// Forced regenerations triggered by an immediate re-want of an id.
    pub rewant_regen: std::sync::atomic::AtomicU64,
    /// Produces that returned `None` (unsupported/corrupt/stopped).
    pub produce_none: std::sync::atomic::AtomicU64,
    /// Produces abandoned by the 30 s wait bound (hung reads).
    pub produce_timeout: std::sync::atomic::AtomicU64,
    /// Pushes dropped by the stale-id guard (resync/upsert mid-produce).
    pub stale_skip: std::sync::atomic::AtomicU64,
    /// CL_CREDIT bytes granted by clients.
    pub credit_recv: std::sync::atomic::AtomicU64,
    /// Preview produce failures by kind (`io`, `decode`, `encode`, …) —
    /// the breakdown behind `produce_none`.
    pub preview_errs: std::sync::Mutex<std::collections::HashMap<String, u64>>,
    /// Scan feeder progress (mirrors the stderr counters).
    pub scan_found: std::sync::atomic::AtomicU64,
    pub scan_done: std::sync::atomic::AtomicBool,
    /// Per-connection PushQueue snapshots keyed by connection id.
    pub queues: std::sync::Mutex<std::collections::HashMap<u64, serde_json::Value>>,
    /// Connection id allocator (debug output only).
    pub next_conn: std::sync::atomic::AtomicU64,
}

impl DbgStats {
    /// Count one preview produce failure by kind.
    pub fn note_preview_err(&self, kind: &str) {
        *self
            .preview_errs
            .lock()
            .unwrap()
            .entry(kind.to_string())
            .or_insert(0) += 1;
    }
}

impl ServerState {
    pub fn new(
        files: Vec<PathBuf>,
        roots: Vec<PathBuf>,
        mode: ServeMode,
        mouse_select: crate::cli::MouseSelectMode,
        parallel: usize,
        token: Option<String>,
        push_credit_kb: u32,
        move_dir: Option<PathBuf>,
        force_delete: bool,
    ) -> Arc<Self> {
        let manifest = Manifest::build(&files, &roots, mode, mouse_select, 1);
        // One gate for the whole server: preview production marks live
        // client requests on it, deletes/crops/preload yield to them.
        let io_gate = IoGate::new();
        // RAM retention is exactly the pinned set: the first PIN_LIMIT
        // (2000) live thumbnails. Unpinned budget is 0 — every found image
        // is still produced + persisted to disk in the background, but
        // only the pinned top stays resident.
        let previews = Arc::new(PreviewStore::with_gate(
            parallel,
            0,
            Arc::clone(&io_gate),
        ));
        for item in &manifest.items {
            if let Some(path) = manifest.path_of(item.id) {
                previews.note_dims(&path, (item.w, item.h));
                previews.note_sig(&path, item.sig);
            }
        }
        previews.request_all(files.iter());
        let (events, _) = broadcast::channel(256);
        Arc::new(Self {
            manifest: Arc::new(RwLock::new(manifest)),
            roots,
            previews,
            actions: Arc::new(ActionQueue::with_gate(
                parallel.max(1).min(4),
                // Deletes — trash moves and --force-delete unlinks — run
                // on a fixed 32-thread pool, independent of -j.
                crate::fs_utils::DELETE_WORKERS,
                move_dir,
                Arc::clone(&io_gate),
                force_delete,
            )),
            events,
            mode,
            mouse_select,
            token,
            initial_credit: push_credit_kb as u64 * 1024,
            resync_seq: std::sync::atomic::AtomicU64::new(0),
            gone: std::sync::Mutex::new(std::collections::HashMap::new()),
            io_gate,
            pins_dirty: std::sync::atomic::AtomicBool::new(true),
            key_cache: std::sync::Mutex::new(HashMap::new()),
            dbg: DbgStats::default(),
        })
    }

    /// Recompute which thumbnails must stay resident in RAM: the first
    /// [`preview_store::PIN_LIMIT`] live items, skipping anything queued for
    /// delete/move or already departed. Cheap enough to run on every manifest
    /// change, but the pump throttles it.
    pub fn refresh_pins(&self) {
        self.pins_dirty
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let pins: Vec<PathBuf> = {
            let m = self.manifest.read().unwrap();
            let mut pins = Vec::with_capacity(preview_store::PIN_LIMIT);
            for i in 0..m.items.len() {
                if pins.len() >= preview_store::PIN_LIMIT {
                    break;
                }
                if m.items[i].removed {
                    continue;
                }
                let Some(path) = m.path_of(i as u32) else {
                    continue;
                };
                if self.actions.is_pending(&path) || self.is_gone(&path) {
                    continue;
                }
                pins.push(path);
            }
            pins
        };
        self.previews.set_pins(pins);
    }

    /// Mark the pin set stale (manifest changed).
    fn pins_changed(&self) {
        self.pins_dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// How long a departed path stays suppressed. Long enough to cover a
    /// rescan that started before the delete landed; a file re-created with
    /// different content is let straight through.
    const GONE_TTL: Duration = Duration::from_secs(300);

    /// Remember that `path` left the album so a stale directory listing can't
    /// put it back. The signature is kept so a *re-created* file (different
    /// size/mtime) is still accepted.
    fn note_gone(&self, path: &Path) {
        let sig = {
            let m = self.manifest.read().unwrap();
            m.position_of(path)
                .and_then(|id| m.items.get(id as usize).map(|i| i.sig))
                .unwrap_or(0)
        };
        let mut gone = self.gone.lock().unwrap();
        if gone.len() > 4096 {
            let ttl = Self::GONE_TTL;
            gone.retain(|_, (_, at)| at.elapsed() < ttl);
        }
        let entry = gone
            .entry(path.to_path_buf())
            .or_insert((sig, std::time::Instant::now()));
        // A later call (post-success) can no longer read the item's sig —
        // keep the one recorded while it was still live.
        if entry.0 == 0 {
            entry.0 = sig;
        }
    }

    /// True while `path` must stay out of the manifest: it left the album and
    /// either no longer exists or still holds the content it had when it left.
    fn is_gone(&self, path: &Path) -> bool {
        let mut gone = self.gone.lock().unwrap();
        let Some(&(sig, at)) = gone.get(path) else {
            return false;
        };
        if at.elapsed() >= Self::GONE_TTL {
            gone.remove(path);
            return false;
        }
        match crate::fs_utils::previews::preview_sig_u64(path) {
            // Gone from disk (or unreadable) — keep it out of the listing.
            None => true,
            // Same content as when it left: a stale listing, not a new file.
            Some(now) if sig != 0 && now == sig => true,
            // Re-created with different content — it is a new file.
            Some(_) => {
                gone.remove(path);
                false
            }
        }
    }

    /// Manifest id → absolute path.
    pub fn path_of(&self, id: u32) -> Option<PathBuf> {
        self.manifest.read().unwrap().path_of(id)
    }

    /// Number of items currently in the manifest (grows while scanning).
    pub fn item_count(&self) -> usize {
        self.manifest.read().unwrap().items.len()
    }

    /// Absolute path → item id.
    pub fn position_of(&self, path: &Path) -> Option<u32> {
        self.manifest.read().unwrap().position_of(path)
    }

    /// Per-file manifest metadata for upserts. `known` carries whatever
    /// the scan already knows for free: `(w, h, bytes, sig)` — `w == 0`
    /// means dims are still unknown (a `Found` event: signature+size came
    /// with the walker's stat, dims still need a header read). `None`
    /// means nothing is known (action-driven upserts) — stat + sniff.
    fn sniff_file(
        &self,
        path: PathBuf,
        known: Option<(u32, u32, u64, u64)>,
    ) -> (PathBuf, u32, u32, u64, u64) {
        if let Some((w, h, bytes, sig)) = known {
            if w != 0 && h != 0 {
                return (path, w, h, bytes, sig);
            }
            // Signature known, dims not: an unchanged signature reuses
            // stored dims — no stat and no header read at all.
            if sig != 0 {
                let m = self.manifest.read().unwrap();
                if let Some(id) = m.positions.get(&path) {
                    let it = &m.items[*id as usize];
                    if it.sig == sig {
                        return (path, it.w, it.h, bytes, sig);
                    }
                }
            }
            // Only the header read remains — sig+bytes need no stat.
            let (w, h) = image::image_dimensions(&path).unwrap_or((0, 0));
            return (path, w, h, bytes, sig);
        }
        // Nothing known: stat for the signature, reuse dims on match,
        // then sniff the header and size.
        let sig = crate::fs_utils::previews::preview_sig_u64(&path).unwrap_or(0);
        if sig != 0 {
            let m = self.manifest.read().unwrap();
            if let Some(id) = m.positions.get(&path) {
                let it = &m.items[*id as usize];
                if it.sig == sig {
                    return (path, it.w, it.h, it.bytes, sig);
                }
            }
        }
        let (w, h) = image::image_dimensions(&path).unwrap_or((0, 0));
        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        (path, w, h, bytes, sig)
    }

    /// Insert newly discovered files or update existing entries in place
    /// (streaming scan + post-action upserts). Keeps the positional id of
    /// known paths so client indexing stays stable; unknown paths append.
    /// Queues previews and broadcasts a `manifest-patch` event so clients
    /// upsert differentially instead of re-pulling the manifest.
    /// Returns how many items were inserted or updated.
    /// `known` = `(w, h, bytes, sig)` when the scan already knows them
    /// (`w == 0` → dims still to sniff); `None` → fully sniffed here.
    pub fn upsert_files(&self, files: Vec<(PathBuf, Option<(u32, u32, u64, u64)>)>) -> usize {
        // Phase 1 (no lock): sniff per-file metadata — this is the I/O part
        // and must not block manifest readers. Parallel across the batch:
        // each file costs an open+header-read, which dominates cold scans
        // on fast storage.
        let items: Vec<(PathBuf, Option<(u32, u32, u64, u64)>)> = files
            .into_iter()
            // Queued for delete/move — "marked for deletion", must not
            // resurface before the op lands. Already departed (a scan batch
            // enumerated the directory before the rename) — must not come
            // back either.
            .filter(|(p, _)| !self.actions.is_pending(p) && !self.is_gone(p))
            .collect();
        let sniffed: Vec<(PathBuf, u32, u32, u64, u64)> =
            par_map(&items, |(p, known)| self.sniff_file(p.clone(), *known));
        // Feed the dims/sig hint maps before queueing — manifest sniffing
        // already paid for this, so preview cache hits won't stat again.
        for (p, w, h, _, sig) in &sniffed {
            self.previews.note_dims(p, (*w, *h));
            self.previews.note_sig(p, *sig);
        }

        let (added, patch_json) = {
            let mut m = self.manifest.write().unwrap();
            let mut patch = ManifestPatch {
                version: m.version + 1,
                ..Default::default()
            };
            let mut dir_index = std::collections::HashMap::<PathBuf, u32>::new();
            let mut added = Vec::new();
            for (path, w, h, bytes, sig) in sniffed {
                // Re-check under the manifest lock: the filter above ran
                // before a (slow-mount) sniff — a delete/move may have
                // landed in between, and a tombstoned path must never
                // come back as a fresh append or live upsert.
                if self.actions.is_pending(&path) || self.is_gone(&path) {
                    continue;
                }
                if let Some(&id) = m.positions.get(&path) {
                    // In-place upsert: keep the id so client tiles stay put.
                    let item = &mut m.items[id as usize];
                    item.w = w;
                    item.h = h;
                    item.bytes = bytes;
                    item.sig = sig;
                    item.removed = false;
                    patch.upsert.push(item.clone());
                    added.push(path);
                    continue;
                }
                let parent = path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf();
                let dir_idx = *dir_index.entry(parent.clone()).or_insert_with(|| {
                    // Intern into the manifest's dir table (existing or new).
                    let parent_str = parent.to_string_lossy();
                    match m
                        .dirs
                        .iter()
                        .position(|d| d.as_str() == parent_str.as_ref())
                    {
                        Some(i) => i as u32,
                        None => {
                            m.dirs.push(parent.to_string_lossy().into_owned());
                            patch.dirs_added.push(parent.to_string_lossy().into_owned());
                            (m.dirs.len() - 1) as u32
                        }
                    }
                });
                let id = m.items.len() as u32;
                let item = ManifestItem {
                    id,
                    name: path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    dir_index: dir_idx,
                    w,
                    h,
                    bytes,
                    sig,
                    removed: false,
                };
                m.positions.insert(path.clone(), id);
                m.items.push(item.clone());
                patch.upsert.push(item);
                added.push(path);
            }
            if patch.upsert.is_empty() {
                return 0;
            }
            m.version = patch.version;
            let json = serde_json::to_string(&serde_json::json!({
                "ev": "manifest-patch",
                "patch": patch,
            }))
            .ok();
            (added, json)
        };

        self.previews.request_all(added.iter());
        if let Some(json) = patch_json {
            let _ = self.events.send(ServerEvent::Json(json));
        }
        if !added.is_empty() {
            self.pins_changed();
        }
        added.len()
    }

    /// Tombstone items (deleted/moved files). Broadcasts a `manifest-patch`
    /// with `remove` so every connected client updates differentially —
    /// ids stay positional, so no reload is needed.
    pub fn remove_ids(&self, ids: &[u32]) {
        let json = {
            let mut m = self.manifest.write().unwrap();
            let mut touched = Vec::new();
            for &id in ids {
                let Some(path) = m.path_of(id) else {
                    continue;
                };
                if let Some(item) = m.items.get_mut(id as usize) {
                    item.removed = true;
                }
                m.positions.remove(&path);
                touched.push(id);
            }
            if touched.is_empty() {
                return;
            }
            m.version += 1;
            serde_json::to_string(&serde_json::json!({
                "ev": "manifest-patch",
                "patch": {
                    "version": m.version,
                    "upsert": [],
                    "remove": touched,
                    "dirs_added": [],
                },
            }))
            .ok()
        };
        if let Some(json) = json {
            let _ = self.events.send(ServerEvent::Json(json));
        }
        // The pinned set is "first N live items" — tombstones change it.
        self.pins_changed();
    }

    /// Tombstone the item at `path` — safe to call across id reassignment:
    /// the path resolves to whatever id currently holds it, so a rescan
    /// mid-action can't tombstone the wrong slot.
    pub fn remove_path(&self, path: &Path) {
        // Remember the departure *before* the slot is tombstoned: a rescan
        // that already listed this directory must not put it back.
        self.note_gone(path);
        let id = self.manifest.read().unwrap().position_of(path);
        if let Some(id) = id {
            self.remove_ids(&[id]);
        }
    }

    /// Undo a tombstone after a failed delete/move — the file is still on
    /// disk, so the item goes live again (broadcast as an `upsert` patch).
    /// Falls back to [`Self::upsert_files`] when a rescan already compacted
    /// the slot away.
    pub fn restore_path(&self, path: &Path) {
        // The op failed, so the file never left: forget the departure or the
        // item could never come back.
        self.gone.lock().unwrap().remove(path);
        enum Out {
            Live,
            Missing,
            Restored(Option<String>),
        }
        let out = {
            let mut m = self.manifest.write().unwrap();
            if m.positions.contains_key(path) {
                Out::Live
            } else {
                // positions only tracks live items — scan for a tombstoned
                // slot that still resolves to `path`.
                let id = (0..m.items.len()).find(|&i| {
                    let item = &m.items[i];
                    m.dirs
                        .get(item.dir_index as usize)
                        .map(|d| Path::new(d).join(&item.name) == path)
                        .unwrap_or(false)
                });
                match id {
                    Some(i) => {
                        m.items[i].removed = false;
                        m.positions.insert(path.to_path_buf(), i as u32);
                        let item = m.items[i].clone();
                        m.version += 1;
                        let json = serde_json::to_string(&serde_json::json!({
                            "ev": "manifest-patch",
                            "patch": {
                                "version": m.version,
                                "upsert": [item],
                                "remove": [],
                                "dirs_added": [],
                            },
                        }))
                        .ok();
                        Out::Restored(json)
                    }
                    None => Out::Missing,
                }
            }
        };
        match out {
            Out::Live => {}
            Out::Restored(json) => {
                if let Some(json) = json {
                    let _ = self.events.send(ServerEvent::Json(json));
                }
            }
            Out::Missing => {
                self.upsert_files(vec![(path.to_path_buf(), None)]);
            }
        }
    }

    /// The source can't even be stat'ed — missing entirely, or under a
    /// path component that denies access — so for the user it is gone
    /// regardless of what action just failed on it. Evict the entry
    /// (tombstone + removal patch) instead of resurrecting a dead tile.
    /// Returns false while the file is still readable.
    pub fn evict_if_unreadable(&self, path: &Path) -> bool {
        if crate::fs_utils::previews::preview_sig_u64(path).is_some() {
            return false;
        }
        self.remove_path(path);
        true
    }

    /// Sort `files` by the `-o` order, using the signature-keyed sort-key
    /// cache so repeated passes don't re-stat or re-parse every file.
    /// Equivalent to [`crate::cli::sort_files`] plus memoization.
    /// `sigs` supplies known signatures (manifest or scan hints) so the
    /// per-file validation `stat` is skipped entirely — on slow mounts
    /// that is the difference between N round-trips and zero.
    pub fn sort_with_order(
        &self,
        files: &mut Vec<PathBuf>,
        order: crate::cli::SortOrder,
        inverse: bool,
        sigs: Option<&HashMap<PathBuf, u64>>,
    ) {
        use crate::cli::SortOrder as S;
        match order {
            S::Filename => files.sort(),
            S::Randomize => files.shuffle(&mut rand::rng()),
            S::Modified
            | S::Size
            | S::SizeAsc
            | S::SizeDesc
            | S::Exif
            | S::ExifAsc
            | S::ExifDesc => {
                let descending = matches!(order, S::SizeDesc | S::ExifDesc);
                let mut keyed: Vec<(Option<u128>, PathBuf)> = files
                    .drain(..)
                    .map(|path| {
                        let sig = sigs
                            .and_then(|m| m.get(&path).copied())
                            .unwrap_or_else(|| {
                                crate::fs_utils::previews::preview_sig_u64(&path).unwrap_or(0)
                            });
                        let cached = self
                            .key_cache
                            .lock()
                            .unwrap()
                            .get(&path)
                            .filter(|(s, _)| *s == sig)
                            .map(|(_, k)| *k);
                        let key = cached.unwrap_or_else(|| {
                            let k = match order {
                                S::Modified => std::fs::metadata(&path)
                                    .ok()
                                    .and_then(|m| m.modified().ok())
                                    .and_then(|t| {
                                        t.duration_since(std::time::UNIX_EPOCH).ok()
                                    })
                                    .map(|d| d.as_nanos()),
                                S::Size | S::SizeAsc | S::SizeDesc => {
                                    Some(std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
                                        as u128)
                                }
                                _ => crate::fs_utils::exif_datetime_key(&path).map(
                                    |(y, mo, d, h, mi, s)| {
                                        // Monotone pack: each field < 100,
                                        // so decimal concat sorts right.
                                        ((((y as u128 * 100 + mo as u128) * 100 + d as u128)
                                            * 100
                                            + h as u128)
                                            * 100
                                            + mi as u128)
                                            * 100
                                            + s as u128
                                    },
                                ),
                            };
                            self.key_cache
                                .lock()
                                .unwrap()
                                .insert(path.clone(), (sig, k));
                            k
                        });
                        (key, path)
                    })
                    .collect();
                keyed.sort_by(|(a, _), (b, _)| match (a, b) {
                    (Some(x), Some(y)) => {
                        if descending {
                            y.cmp(x)
                        } else {
                            x.cmp(y)
                        }
                    }
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                });
                files.extend(keyed.into_iter().map(|(_, p)| p));
            }
        }
        if inverse && order != S::Randomize {
            files.reverse();
        }
    }

    /// Re-apply the `-o` order to the CURRENT manifest and resync when it
    /// changed — the streaming scan's continuous sorting: without it the
    /// grid stays in filesystem order until the (possibly minutes-long)
    /// walk finishes. A resync only goes out when the order actually
    /// differs; `Randomize` is exempt (a shuffle per batch is meaningless).
    pub fn resort_partial(&self, order: crate::cli::SortOrder, inverse: bool) {
        if order == crate::cli::SortOrder::Randomize || self.actions.pending() > 0 {
            // See rebuild_manifest_diff: a resync during a delete/move
            // renumbers slots the client tombstoned → tiles "reappear".
            return;
        }
        // Manifest order + per-path sigs/hints in one pass — sorting and
        // the hinted rebuild then need zero filesystem I/O for files the
        // manifest already describes.
        let (mut files, sigs, hints) = {
            let m = self.manifest.read().unwrap();
            let mut files = Vec::new();
            let mut sigs = HashMap::new();
            let mut hints = HashMap::new();
            for i in 0..m.items.len() {
                if let Some(p) = m.path_of(i as u32) {
                    let it = &m.items[i];
                    files.push(p.clone());
                    sigs.insert(p.clone(), it.sig);
                    hints.insert(p, (it.w, it.h, it.bytes, it.sig));
                }
            }
            (files, sigs, hints)
        };
        let current = files.clone();
        self.sort_with_order(&mut files, order, inverse, Some(&sigs));
        if files != current {
            self.rebuild_manifest_hinted(files, &hints, &self.roots);
            self.broadcast_resync();
        }
    }

    /// Scan finished: apply the final `-o` ordering, rebuild the manifest
    /// (ids are reassigned) and broadcast a `resync` patch so clients
    /// replace their lists in place — no reconnect or manifest re-pull.
    pub fn finish_scan(&self, order: crate::cli::SortOrder, inverse: bool) {
        // Rebuilding renumbers positional ids; the resync patch keeps
        // client and server in agreement. While a delete/move is pending
        // that refill of tombstoned slots looks like tiles "reappearing",
        // and a client action carrying a pre-rebuild id could hit the
        // wrong file — defer ordering; the next rescan applies it.
        if self.actions.pending() > 0 {
            return;
        }
        let (mut files, sigs, hints) = {
            let m = self.manifest.read().unwrap();
            let mut files = Vec::new();
            let mut sigs = HashMap::new();
            let mut hints = HashMap::new();
            for i in 0..m.items.len() {
                if let Some(p) = m.path_of(i as u32) {
                    let it = &m.items[i];
                    files.push(p.clone());
                    sigs.insert(p.clone(), it.sig);
                    hints.insert(p, (it.w, it.h, it.bytes, it.sig));
                }
            }
            (files, sigs, hints)
        };
        self.sort_with_order(&mut files, order, inverse, Some(&sigs));
        self.rebuild_manifest_hinted(files, &hints, &self.roots);
        self.broadcast_resync();
    }

    /// Broadcast the current manifest as a `resync` patch event.
    pub fn broadcast_resync(&self) {
        self.resync_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let patch = self.manifest.read().unwrap().full_patch();
        if let Ok(json) = serde_json::to_string(&serde_json::json!({
            "ev": "manifest-patch",
            "patch": patch,
        })) {
            let _ = self.events.send(ServerEvent::Json(json));
        }
    }

    /// Start the background pump: drains preview results + action
    /// completions and forwards them as broadcast events.
    pub fn spawn_pumps(self: &Arc<Self>) {
        let state = self.clone();
        std::thread::spawn(move || {
            let mut last_pin = std::time::Instant::now() - std::time::Duration::from_secs(1);
            let mut last_fill = std::time::Instant::now();
            loop {
                // Keep the top of the album resident in RAM. The pin set is
                // recomputed when the manifest changed; missing pinned
                // previews are re-queued periodically so the set converges
                // (a failed/evicted preview must not leave a hole).
                if state.pins_dirty.load(std::sync::atomic::Ordering::Relaxed)
                    && last_pin.elapsed() >= std::time::Duration::from_millis(250)
                {
                    state.refresh_pins();
                    last_pin = std::time::Instant::now();
                }
                if last_fill.elapsed() >= std::time::Duration::from_secs(2) {
                    for path in state.previews.missing_pins() {
                        state.previews.request(path);
                    }
                    last_fill = std::time::Instant::now();
                }
                // Preview completions → per-path ready events.
                for (path, preview) in state.previews.poll() {
                    if let Err(f) = &preview {
                        state.dbg.note_preview_err(f.kind);
                        // Deleted outside ImageCropper — background
                        // produces (preload/pins) discover it too.
                        if f.kind == "gone" {
                            state.remove_path(&path);
                        }
                    }
                    if let Some(id) = state.position_of(&path) {
                        let dims = preview
                            .as_ref()
                            .ok()
                            .map(|p| format!(",\"w\":{},\"h\":{}", p.w, p.h))
                            .unwrap_or_default();
                        let _ = state.events.send(ServerEvent::Json(format!(
                            "{{\"ev\":\"thumb-ready\",\"id\":{id}{dims}}}"
                        )));
                    }
                }
                // Action completions → differential manifest updates + events.
                for mut done in state.actions.poll() {
                    use crate::server::actions::ActionKind;
                    match (done.ok, done.kind) {
                        // Path-based: ids may have been reassigned by a rescan
                        // while the action was in flight — done.src is stable.
                        (true, ActionKind::Delete | ActionKind::Move) => {
                            state.remove_path(&done.src);
                        }
                        (false, ActionKind::Delete | ActionKind::Move) => {
                            if state.evict_if_unreadable(&done.src) {
                                // The source is unreachable — for the user
                                // it is gone, which is exactly what the op
                                // asked for. Ack it cleanly instead of
                                // resurrecting the tombstone and spamming
                                // one "file no longer readable" per file.
                                done.ok = true;
                                done.error = None;
                            } else {
                                // Undo the eager tombstone: the file never left.
                                state.restore_path(&done.src);
                            }
                        }
                        (true, ActionKind::Crop | ActionKind::Resave) => {
                            if let Some(new_path) = &done.new_path {
                                if new_path != &done.src {
                                    state.remove_path(&done.src);
                                }
                                state.upsert_files(vec![(new_path.clone(), None)]);
                            }
                        }
                        (false, _) => {
                            // A failed transform still evicts a dead
                            // source — the error ack stays (the op truly
                            // failed), but the tile must not linger.
                            state.evict_if_unreadable(&done.src);
                        }
                    }
                    // Stale signature: our manifest entry predates a disk
                    // change (or the mount reports a different mtime than at
                    // scan time). After the tombstone restore above, re-sync
                    // the entry — dims + sig — and broadcast the patch BEFORE
                    // the ack below, so a client retry has the current sig
                    // instead of failing forever.
                    if !done.ok
                        && done
                            .error
                            .as_deref()
                            .is_some_and(|e| e.contains(crate::server::actions::STALE_SIG_MSG))
                    {
                        state.upsert_files(vec![(done.src.clone(), None)]);
                    }
                    if crate::fs_utils::verbose() {
                        eprintln!(
                            "action {:?} id={} ok={} err={:?}",
                            done.kind, done.id, done.ok, done.error
                        );
                    }
                    if let Ok(json) = serde_json::to_string(&serde_json::json!({
                        "ev": "action-done",
                        "tag": done.tag,
                        "ok": done.ok,
                        "error": done.error,
                        "new_sig": done.new_sig,
                        "bytes": done.bytes,
                    })) {
                        let _ = state.events.send(ServerEvent::Json(json));
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        });
    }

    /// Rebuild from a fresh directory listing **without renumbering**:
    /// surviving paths keep their ids, vanished paths are tombstoned, new
    /// paths append. Broadcasts one differential patch.
    ///
    /// A rescan must never renumber: every id reassignment forces clients
    /// to wipe and refetch every thumbnail, drop their want slots and
    /// selection — during a delete storm that leaves the grid permanently
    /// blank while resyncs keep clearing what just arrived.
    pub fn rebuild_manifest_diff(&self, files: Vec<PathBuf>, roots: &[PathBuf], reorder: bool) {
        self.rebuild_manifest_diff_hinted(files, &HashMap::new(), roots, reorder)
    }

    /// [`Self::rebuild_manifest_diff`] with scan-time `(w, h, bytes, sig)`
    /// hints per path — a seeded rescan knows them for free, so unchanged
    /// files cost zero I/O even on slow mounts.
    pub fn rebuild_manifest_diff_hinted(
        &self,
        files: Vec<PathBuf>,
        hints: &HashMap<PathBuf, (u32, u32, u64, u64)>,
        roots: &[PathBuf],
        reorder: bool,
    ) {
        // Pending deletes must not resurface; departed paths must not come
        // back (the listing may predate the rename).
        let files: Vec<PathBuf> = files
            .into_iter()
            .filter(|p| !self.actions.is_pending(p) && !self.is_gone(p))
            .collect();
        // When `reorder` is set the listing is in the user's `-o` order
        // (filename/EXIF/size/…). The diff path can only tombstone and
        // append — if applying it would leave surviving items out of the
        // requested order (a new file sorting mid-list, or a reorder),
        // fall back to a full rebuild + resync. Clients keep thumbs and
        // selection across resyncs via content keys, and order changes are
        // rare — deletes/shrinks keep the same relative order and still
        // take the cheap diff path.
        // Never renumber mid-delete/move: a resync shifts positional ids,
        // so slots the client already tombstoned get refilled by
        // different images — the visible "reappearing" tiles. While
        // destructive work is pending the cheap diff path keeps ids
        // stable; the next rescan re-establishes order.
        if reorder && self.actions.pending() == 0 {
            let current: Vec<PathBuf> = {
                let m = self.manifest.read().unwrap();
                (0..m.items.len())
                    .filter_map(|i| m.path_of(i as u32))
                    .collect()
            };
            let file_set: std::collections::HashSet<&PathBuf> = files.iter().collect();
            let cur_set: std::collections::HashSet<&PathBuf> = current.iter().collect();
            // The diff path would produce exactly: survivors in current
            // order, then new files appended in listing order.
            let mut expected: Vec<PathBuf> = current
                .iter()
                .filter(|p| file_set.contains(*p))
                .cloned()
                .collect();
            expected.extend(
                files
                    .iter()
                    .filter(|p| !cur_set.contains(*p))
                    .cloned(),
            );
            if expected != files {
                self.rebuild_manifest_hinted(files, hints, roots);
                self.broadcast_resync();
                return;
            }
        }
        // Phase 1 (no lock): sniff metadata for the listing. Reuse stored
        // dims/sig when the cheap signature is unchanged — a rescan then
        // stats instead of re-parsing headers on a slow mount.
        let prior: HashMap<PathBuf, (u32, u32, u64, u64)> = {
            let m = self.manifest.read().unwrap();
            (0..m.items.len())
                .filter_map(|i| {
                    let it = &m.items[i];
                    m.path_of(i as u32)
                        .map(|p| (p, (it.w, it.h, it.bytes, it.sig)))
                })
                .collect()
        };
        let sniffed: Vec<(PathBuf, u32, u32, u64, u64)> =
            par_map(&files, |path| sniff_item(path, hints.get(path).copied(), &prior));
        for (p, w, h, _, sig) in &sniffed {
            self.previews.note_dims(p, (*w, *h));
            self.previews.note_sig(p, *sig);
        }

        let (json, added) = {
            let mut m = self.manifest.write().unwrap();
            let mut patch = ManifestPatch {
                version: m.version + 1,
                ..Default::default()
            };
            let mut present: std::collections::HashSet<PathBuf> =
                std::collections::HashSet::with_capacity(sniffed.len());
            let mut added = Vec::new();
            let mut dir_index: HashMap<PathBuf, u32> = HashMap::new();
            for (path, w, h, bytes, sig) in sniffed {
                // Re-check under the lock: a delete/move may have landed
                // between the filter above and this insert (the sniff is
                // the slow part on remote mounts).
                if self.actions.is_pending(&path) || self.is_gone(&path) {
                    continue;
                }
                present.insert(path.clone());
                if let Some(&id) = m.positions.get(&path) {
                    // Surviving file: keep the id, refresh metadata.
                    let item = &mut m.items[id as usize];
                    if (item.w, item.h, item.bytes, item.sig) != (w, h, bytes, sig) {
                        item.w = w;
                        item.h = h;
                        item.bytes = bytes;
                        item.sig = sig;
                        let snapshot = item.clone();
                        patch.upsert.push(snapshot);
                    }
                    continue;
                }
                let parent = path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf();
                let dir_idx = *dir_index.entry(parent.clone()).or_insert_with(|| {
                    let parent_str = parent.to_string_lossy();
                    match m
                        .dirs
                        .iter()
                        .position(|d| d.as_str() == parent_str.as_ref())
                    {
                        Some(i) => i as u32,
                        None => {
                            m.dirs.push(parent.to_string_lossy().into_owned());
                            patch.dirs_added.push(parent.to_string_lossy().into_owned());
                            (m.dirs.len() - 1) as u32
                        }
                    }
                });
                let id = m.items.len() as u32;
                let item = ManifestItem {
                    id,
                    name: path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    dir_index: dir_idx,
                    w,
                    h,
                    bytes,
                    sig,
                    removed: false,
                };
                m.positions.insert(path.clone(), id);
                m.items.push(item.clone());
                patch.upsert.push(item);
                added.push(path);
            }
            // Live items no longer in the listing are gone from disk —
            // tombstone them in place (no compaction, no renumbering).
            let vanished: Vec<(u32, PathBuf)> = m
                .positions
                .iter()
                .filter(|(p, _)| !present.contains(*p))
                .map(|(p, &id)| (id, p.clone()))
                .collect();
            for (id, path) in vanished {
                m.positions.remove(&path);
                if let Some(item) = m.items.get_mut(id as usize) {
                    if !item.removed {
                        item.removed = true;
                        patch.remove.push(id);
                    }
                }
            }
            if patch.upsert.is_empty() && patch.remove.is_empty() {
                return;
            }
            m.version = patch.version;
            (
                serde_json::to_string(&serde_json::json!({
                    "ev": "manifest-patch",
                    "patch": patch,
                }))
                .ok(),
                added,
            )
        };

        self.previews.request_all(added.iter());
        if let Some(json) = json {
            let _ = self.events.send(ServerEvent::Json(json));
        }
        self.pins_changed();
    }

    /// Rebuild the manifest (e.g. after fs changes) and bump the version.
    /// Returns the new manifest for broadcast/patch purposes.
    pub fn rebuild_manifest(&self, files: Vec<PathBuf>, roots: &[PathBuf]) {
        self.rebuild_manifest_hinted(files, &HashMap::new(), roots)
    }

    /// [`Self::rebuild_manifest`] with scan-time `(w, h, bytes, sig)`
    /// hints per path — callers that just sorted the manifest can pass
    /// its own signatures, making an unchanged-file rebuild cost zero I/O.
    pub fn rebuild_manifest_hinted(
        &self,
        files: Vec<PathBuf>,
        hints: &HashMap<PathBuf, (u32, u32, u64, u64)>,
        roots: &[PathBuf],
    ) {
        // Paths queued for delete/move are "marked for deletion" — a rebuild
        // must not resurrect them just because the file still exists. Paths
        // that already left (the listing predates the rename) must not come
        // back either.
        let files: Vec<PathBuf> = files
            .into_iter()
            .filter(|p| !self.actions.is_pending(p) && !self.is_gone(p))
            .collect();
        // Reuse stored dims/sig for files whose (size, mtime) signature is
        // unchanged — a rescan then stats each file instead of re-parsing
        // every image header, which matters a lot on slow/remote mounts.
        let (version, prior) = {
            let m = self.manifest.read().unwrap();
            let prior: std::collections::HashMap<PathBuf, (u32, u32, u64, u64)> =
                (0..m.items.len())
                    .filter_map(|i| {
                        let it = &m.items[i];
                        m.path_of(i as u32)
                            .map(|p| (p, (it.w, it.h, it.bytes, it.sig)))
                    })
                    .collect();
            (m.version + 1, prior)
        };
        // Sniff in parallel up front — `Manifest::build_with`'s closure is
        // serial, so pre-computing keeps a full rebuild from becoming a
        // serial stat+open per file.
        let sniffed: std::collections::HashMap<PathBuf, (u32, u32, u64, u64)> =
            par_map(&files, |p| sniff_item(p, hints.get(p).copied(), &prior))
                .into_iter()
                .map(|(p, w, h, b, s)| (p, (w, h, b, s)))
                .collect();
        for (p, (w, h, _, sig)) in &sniffed {
            self.previews.note_dims(p, (*w, *h));
            self.previews.note_sig(p, *sig);
        }
        let sniff = move |path: &Path| {
            sniffed.get(path).copied().unwrap_or((0, 0, 0, 0))
        };
        // Re-filter right before the swap: a delete/move may have landed
        // while `files` was being sniffed — the tombstone must win.
        let files: Vec<PathBuf> = files
            .into_iter()
            .filter(|p| !self.actions.is_pending(p) && !self.is_gone(p))
            .collect();
        let fresh =
            Manifest::build_with(&files, roots, self.mode, self.mouse_select, version, sniff);
        *self.manifest.write().unwrap() = fresh;
        // The album order/contents changed — re-pin the top of it.
        self.pins_changed();
    }
}

/// Stat + dims for one path during a manifest (re)build, reusing `prior`
/// dims/sig when the (size, mtime) signature is unchanged — a rescan then
/// stats each file instead of re-parsing every image header, which matters
/// a lot on slow/remote mounts. One `metadata` call yields both the folded
/// signature and the byte size — the old code stat'd twice per changed
/// file.
fn sniff_path(
    path: &Path,
    prior: &std::collections::HashMap<PathBuf, (u32, u32, u64, u64)>,
) -> (PathBuf, u32, u32, u64, u64) {
    let (len, nanos) = crate::fs_utils::previews::source_signature(path).unwrap_or((0, 0));
    let sig = len ^ nanos as u64 ^ (nanos >> 64) as u64;
    if let Some(&(w, h, bytes, old_sig)) = prior.get(path) {
        if sig == old_sig && sig != 0 {
            return (path.to_path_buf(), w, h, bytes, sig);
        }
    }
    let (w, h) = image::image_dimensions(path).unwrap_or((0, 0));
    (path.to_path_buf(), w, h, len, sig)
}

/// [`sniff_path`] plus scan hints: `hint` = `(w, h, bytes, sig)` the scan
/// already knows — `w == 0` means dims are unknown (header read only, no
/// stat); a fully-known hint (seeded rescan, `Cached` event) costs zero
/// I/O. A stale hint degrades to the hinted sig + a header read — still
/// no stat.
fn sniff_item(
    path: &Path,
    hint: Option<(u32, u32, u64, u64)>,
    prior: &std::collections::HashMap<PathBuf, (u32, u32, u64, u64)>,
) -> (PathBuf, u32, u32, u64, u64) {
    if let Some((w, h, bytes, sig)) = hint {
        if sig != 0 {
            if w != 0 && h != 0 {
                return (path.to_path_buf(), w, h, bytes, sig);
            }
            // Signature known, dims not: an unchanged signature reuses
            // stored dims — no stat and no header read at all.
            if let Some(&(pw, ph, _, psig)) = prior.get(path) {
                if psig == sig {
                    return (path.to_path_buf(), pw, ph, bytes, sig);
                }
            }
            // Only the header read remains — sig+bytes need no stat.
            let (w, h) = image::image_dimensions(path).unwrap_or((0, 0));
            return (path.to_path_buf(), w, h, bytes, sig);
        }
    }
    sniff_path(path, prior)
}

/// Spawn the streaming-scan feeder: paths from a [`crate::cli::ScanStream`]
/// are appended to the manifest in batches (so clients see partial results
/// immediately, even on slow mounts), then the final `-o` ordering is
/// applied once the scan completes.
pub fn spawn_scan_feeder(state: &Arc<ServerState>, stream: crate::cli::ScanStream) {
    use crate::fs_utils::ScanEvent;
    // Small batches so the client sees items appear smoothly instead of in
    // big steps — a flush still caps the patch rate at FLUSH intervals.
    const BATCH: usize = 8;
    const FLUSH: Duration = Duration::from_millis(200);
    // While fewer than this many items are indexed (and the scan is still
    // running), destructive work defers entirely: the album filling in is
    // the priority, deletes/moves can wait.
    const PRIORITY_UNTIL: usize = 2000;

    let state = state.clone();
    std::thread::spawn(move || {
        let _ = state
            .events
            .send(ServerEvent::Json("{\"ev\":\"scan-start\"}".to_string()));
        if crate::fs_utils::verbose() {
            eprintln!("scan: started");
        }
        // RAII: the flag clears itself if this thread exits by any path —
        // a stuck hold would tax every action job for the rest of the run.
        let mut hold = Some(state.io_gate.scan_hold());
        let mut batch: Vec<(PathBuf, Option<(u32, u32, u64, u64)>)> = Vec::new();
        let mut found = 0usize;
        let t0 = std::time::Instant::now();
        let mut last_report = t0;
        loop {
            let timed_out = match stream.rx.recv_timeout(FLUSH) {
                Ok(ScanEvent::Found { path, bytes, sig }) => {
                    // Feed the preview pipeline immediately — listing →
                    // read → compute run as parallel stages, so waiting
                    // for this file's manifest upsert would serialize
                    // thumbnails behind the metadata sniff. The upsert's
                    // own `request_all` stays as the dedup'd safety net.
                    state.previews.note_sig(&path, sig);
                    if !state.actions.is_pending(&path) && !state.is_gone(&path) {
                        state.previews.request(path.clone());
                    }
                    // sig+bytes came free with the walker's stat — the
                    // upsert never needs to re-stat this file.
                    batch.push((path, Some((0, 0, bytes, sig))));
                    found += 1;
                    false
                }
                Ok(ScanEvent::Cached { path, w, h, bytes, sig }) => {
                    state.previews.note_sig(&path, sig);
                    state.previews.note_dims(&path, (w, h));
                    if !state.actions.is_pending(&path) && !state.is_gone(&path) {
                        state.previews.request(path.clone());
                    }
                    batch.push((path, Some((w, h, bytes, sig))));
                    found += 1;
                    false
                }
                Ok(ScanEvent::Error(e)) => {
                    eprintln!("scan: {e}");
                    // Surface scan errors to clients — a directory that
                    // can't be read must not silently show "0 images".
                    if let Ok(json) = serde_json::to_string(
                        &serde_json::json!({"ev": "scan-error", "msg": e}),
                    ) {
                        let _ = state.events.send(ServerEvent::Json(json));
                    }
                    false
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => true,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };
            // Slow mounts can take minutes to enumerate — say we're alive.
            if crate::fs_utils::verbose() && last_report.elapsed().as_secs() >= 2 {
                eprintln!("scan: {found} files discovered so far…");
                last_report = std::time::Instant::now();
            }
            // Ctrl-C: stop feeding upserts/preview requests into a state
            // that's already shutting down.
            if crate::fs_utils::sigint_received() {
                break;
            }
            if batch.len() >= BATCH || (timed_out && !batch.is_empty()) {
                state.upsert_files(std::mem::take(&mut batch));
                // --resort=realtime: the `-o` order applies after EVERY
                // batch, not just at scan end — a resync goes out only
                // when the order actually changed.
                if stream.resort == crate::cli::ResortMode::Realtime {
                    state.resort_partial(stream.order, stream.inverse);
                }
                state
                    .dbg
                    .scan_found
                    .store(found as u64, std::sync::atomic::Ordering::Relaxed);
                if hold.is_some() && state.item_count() >= PRIORITY_UNTIL {
                    hold.take(); // drops the guard → flag cleared
                    if crate::fs_utils::verbose() {
                        eprintln!("scan: {PRIORITY_UNTIL} items indexed — background work resumed");
                    }
                }
            }
        }
        drop(hold);
        if !batch.is_empty() {
            state.upsert_files(batch);
            if stream.resort == crate::cli::ResortMode::Realtime {
                state.resort_partial(stream.order, stream.inverse);
            }
        }
        if crate::fs_utils::verbose() {
            eprintln!(
                "scan: finished — {found} files discovered in {:?}, indexing…",
                t0.elapsed()
            );
        }
        state.finish_scan(stream.order, stream.inverse);
        // Persist the scan result so the next run seeds instantly.
        {
            let m = state.manifest.read().unwrap();
            let mut per_root: std::collections::HashMap<
                PathBuf,
                Vec<(PathBuf, String, u64, u64, u32, u32)>,
            > = std::collections::HashMap::new();
            for (i, it) in m.items.iter().enumerate() {
                if it.removed {
                    continue;
                }
                let Some(p) = m.path_of(i as u32) else { continue };
                let Some(parent) = p.parent() else { continue };
                let root = state.roots.iter().find(|r| p.starts_with(r));
                let Some(root) = root else { continue };
                // it.bytes/it.sig are the signature the scan already
                // recorded — no per-file stat needed to persist the pack.
                per_root.entry(root.clone()).or_default().push((
                    parent.to_path_buf(),
                    it.name.clone(),
                    it.bytes,
                    it.sig,
                    it.w,
                    it.h,
                ));
            }
            drop(m);
            for (root, files) in per_root {
                crate::fs_utils::files_pack::save(&root, &files);
            }
        }
        if crate::fs_utils::verbose() {
            eprintln!("scan: {} items indexed", state.item_count());
        }
        // Let clients distinguish "still scanning" from "done, found
        // nothing" — the manifest resync alone can't express that.
        let _ = state.events.send(ServerEvent::Json(format!(
            "{{\"ev\":\"scan-end\",\"count\":{found}}}"
        )));
        state
            .dbg
            .scan_found
            .store(found as u64, std::sync::atomic::Ordering::Relaxed);
        state
            .dbg
            .scan_done
            .store(true, std::sync::atomic::Ordering::Relaxed);
    });
}

/// Token check at WS upgrade time (`?token=` query param). Returns `true`
/// when no token is configured or the supplied token matches. Both sockets
/// enforce it — `/bulk` must never be a backdoor around `/data`'s HELLO.
pub(crate) fn ws_authed(state: &ServerState, query: &std::collections::HashMap<String, String>) -> bool {
    state.token.is_none() || query.get("token").map(String::as_str) == state.token.as_deref()
}

/// Build the axum router (public for tests binding ephemeral ports).
pub fn build_router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/", get(http::index))
        .route("/app.js", get(http::app_js))
        .route("/favicon.ico", get(http::favicon))
        .route("/healthz", get(http::healthz))
        .route("/debugz", get(http::debugz))
        .route("/orig", get(http::orig))
        .route("/data", get(ws_data::handler))
        .route("/bulk", get(ws_bulk::handler))
        .with_state(state)
}

/// Ctrl-C shutdown: stop preview generation immediately, then finish every
/// queued delete/move/crop. There is deliberately no deadline — the queue is
/// the user's work and it must land. A second Ctrl-C abandons the drain.
async fn shutdown(state: &ServerState) {
    state.previews.stop();
    // Refuse new GPU work too: in-flight decodes/resizes still finish,
    // but nothing submits after this point — required for a quiescent
    // device at process exit.
    crate::gpujpeg::stop();
    let mut last = usize::MAX;
    let mut last_report = std::time::Instant::now();
    loop {
        let n = state.actions.pending();
        if n == 0 {
            break;
        }
        if n != last {
            eprintln!("shutdown: finishing {n} queued action(s)…");
            last = n;
            last_report = std::time::Instant::now();
        } else if last_report.elapsed() >= std::time::Duration::from_secs(5) {
            eprintln!("shutdown: still finishing {n} queued action(s)… (Ctrl-C again to quit)");
            last_report = std::time::Instant::now();
        }
        if crate::fs_utils::sigint_count() > 1 {
            eprintln!("shutdown: {n} action(s) left unfinished");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // No GPU call may be in flight when the process exits: shared-library
    // destructors (the Vulkan driver) run during exit() and segfault if a
    // detached worker is still inside a driver call. Wait for the
    // counted GPU scopes to drain — bounded: a genuinely wedged driver
    // call must not hang shutdown forever.
    let gpu_t0 = std::time::Instant::now();
    loop {
        if crate::gpujpeg::wait_idle(std::time::Duration::from_millis(200)) {
            break;
        }
        if crate::fs_utils::sigint_count() > 1 {
            break;
        }
        if gpu_t0.elapsed() >= std::time::Duration::from_secs(30) {
            eprintln!("shutdown: GPU work did not drain in 30s — exiting anyway");
            break;
        }
    }
}

/// Serve on `addr`. Blocks; used by the `--serve` paths in main.rs.
/// `on_ready` runs after the socket is bound and the URL printed — slow
/// startup work (scan feeding, FS watcher registration on remote mounts)
/// belongs there so the server is reachable first.
pub fn run_server(
    addr: SocketAddr,
    state: Arc<ServerState>,
    open_browser: bool,
    on_ready: impl FnOnce() + Send + 'static,
) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let bound = listener.local_addr()?;
        // The token goes straight into the URL so the client's WS
        // connects carry `?token=` automatically.
        let url = match &state.token {
            Some(t) => format!("http://{bound}/?token={t}"),
            None => format!("http://{bound}/"),
        };
        eprintln!("Serving at {url}");
        state.spawn_pumps();
        if open_browser {
            // A wildcard bind (0.0.0.0) isn't a useful browser address —
            // open loopback instead.
            let browser_url = if bound.ip().is_unspecified() {
                let loopback: IpAddr = if bound.is_ipv6() {
                    Ipv6Addr::LOCALHOST.into()
                } else {
                    Ipv4Addr::LOCALHOST.into()
                };
                let local = SocketAddr::new(loopback, bound.port());
                url.replacen(&bound.to_string(), &local.to_string(), 1)
            } else {
                url.clone()
            };
            let _ = webbrowser::open(&browser_url);
        }
        on_ready();
        tokio::select! {
            r = axum::serve(listener, build_router(state.clone())) => { r?; }
            r = tokio::signal::ctrl_c() => {
                r?;
                eprintln!("Ctrl-C: stopping preview generation, finishing queued actions…");
                shutdown(&state).await;
            }
        }
        Ok(())
    });
    // Bound the teardown wait: a `spawn_blocking` task stuck on a
    // dead-mount read must not hold process exit forever. Detached
    // leftovers are safe: gpujpeg::stop() means they can no longer start
    // a driver call, and a normal exit() is then race-free — shutdown()
    // already waited for GPU_ACTIVE == 0.
    rt.shutdown_timeout(std::time::Duration::from_secs(2));
    result
}
