mod patch;

use agent_client_protocol::schema::v2 as acp_v2;
use anyhow::{Result, anyhow, ensure};
use buffer_diff::BufferDiff;
use collections::{HashMap, HashSet};
use gpui::{App, AppContext, AsyncApp, Context, Entity, SharedString, Subscription, Task};
use itertools::Itertools;
use language::{
    Anchor, Buffer, Capability, DiskState, File, LanguageRegistry, OffsetRangeExt as _, Point,
    TextBuffer, modeline,
};
use markdown::Markdown;
use multi_buffer::{MultiBuffer, PathKey, excerpt_context_lines};
use project::Project;
use std::{cmp::Reverse, ops::Range, path::Path, sync::Arc};
use text::ReplicaId;
use util::ResultExt;

#[derive(Debug)]
pub struct DiffPatch {
    pub files: Vec<DiffPatchFile>,
    pub fallback: Option<Entity<Markdown>>,
}

#[derive(Debug)]
pub struct DiffPatchFile {
    pub change_index: usize,
    pub hunks: Vec<DiffPatchHunk>,
}

#[derive(Debug)]
pub struct DiffPatchHunk {
    pub header: SharedString,
    pub buffer: Entity<MultiBuffer>,
    _update_diff: Task<()>,
}

impl DiffPatch {
    pub(crate) fn new(
        source: &acp_v2::Diff,
        language_registry: &Arc<LanguageRegistry>,
        cx: &mut App,
    ) -> Self {
        let mut files = source
            .changes
            .iter()
            .enumerate()
            .map(|(change_index, _)| DiffPatchFile {
                change_index,
                hunks: Vec::new(),
            })
            .collect::<Vec<_>>();
        if source.patch.is_none() {
            return Self {
                files,
                fallback: None,
            };
        }

        match Self::parse(source) {
            Ok(parsed) => {
                for (change_index, file) in parsed {
                    let path = file.new_path.or(file.old_path).unwrap_or_default();
                    if let Some(render) = files.get_mut(change_index) {
                        render.hunks = file
                            .hunks
                            .into_iter()
                            .map(|hunk| {
                                DiffPatchHunk::new(hunk, &path, language_registry.clone(), cx)
                            })
                            .collect();
                    }
                }
                Self {
                    files,
                    fallback: None,
                }
            }
            Err(error) => {
                let preview = format!(
                    "Diff preview unavailable: {error}\n\n{}",
                    crate::ToolCallContent::patch_preview(source)
                );
                Self {
                    files,
                    fallback: Some(crate::ContentBlock::create_markdown(
                        preview,
                        language_registry,
                        cx,
                    )),
                }
            }
        }
    }

    fn parse(source: &acp_v2::Diff) -> Result<Vec<(usize, patch::PatchFile)>> {
        let patch = source
            .patch
            .as_ref()
            .ok_or_else(|| anyhow!("no patch supplied"))?;
        ensure!(
            patch.format == acp_v2::DiffPatchFormat::GitPatch,
            "unsupported patch format"
        );
        ensure!(
            !source.changes.is_empty(),
            "patch has no reported file changes"
        );
        let mut change_indices: HashMap<_, Option<usize>> = HashMap::default();
        for (index, change) in source.changes.iter().enumerate() {
            if let Some(paths) = diff_change_paths(change) {
                change_indices
                    .entry(paths)
                    .and_modify(|index| *index = None)
                    .or_insert(Some(index));
            }
        }
        let mut seen = HashSet::default();
        patch::parse_patch(&patch.text)?
            .into_iter()
            .map(|mut file| {
                let paths = (
                    file.old_path.as_deref().map(Path::new),
                    file.new_path.as_deref().map(Path::new),
                );
                let anonymous = paths == (None, None);
                let paths = if anonymous {
                    let [change] = source.changes.as_slice() else {
                        return Err(anyhow!(
                            "patch without filenames requires exactly one file change"
                        ));
                    };
                    diff_change_paths(change)
                        .ok_or_else(|| anyhow!("unsupported file operation"))?
                } else {
                    paths
                };
                let index = change_indices
                    .get(&paths)
                    .copied()
                    .flatten()
                    .ok_or_else(|| {
                        anyhow!(
                            "patch paths are ambiguous or do not match the reported file changes"
                        )
                    })?;
                ensure!(seen.insert(index), "repeated patch file");
                let change = source
                    .changes
                    .get(index)
                    .ok_or_else(|| anyhow!("missing file change"))?;
                ensure!(
                    matches!(change.file_type, None | Some(acp_v2::DiffFileType::Text)),
                    "text patch supplied for a non-text file"
                );
                if anonymous && let Some((old, new)) = diff_change_paths(change) {
                    file.old_path = old.map(|path| path.to_string_lossy().into_owned());
                    file.new_path = new.map(|path| path.to_string_lossy().into_owned());
                }
                ensure!(
                    file.hunks.iter().all(|hunk| {
                        (file.old_path.is_some() || hunk.old_count == 0)
                            && (file.new_path.is_some() || hunk.new_count == 0)
                    }),
                    "hunk contents conflict with the reported file operation"
                );
                Ok((index, file))
            })
            .collect()
    }
}

impl DiffPatchHunk {
    fn new(
        hunk: patch::PatchHunk,
        path: &str,
        language_registry: Arc<LanguageRegistry>,
        cx: &mut App,
    ) -> Self {
        let range_label = |start, count| {
            if count == 1 {
                format!("{start}")
            } else {
                format!("{start},{count}")
            }
        };
        let header = format!(
            "@@ -{} +{} @@",
            range_label(hunk.old_start, hunk.old_count),
            range_label(hunk.new_start, hunk.new_count),
        )
        .into();
        let new_buffer = cx.new(|cx| {
            let mut buffer = Buffer::local(hunk.new_text, cx);
            buffer.set_capability(Capability::ReadOnly, cx);
            buffer
        });
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::without_headers(Capability::ReadOnly);
            multibuffer.set_excerpts_for_path(
                PathKey::for_buffer(&new_buffer, cx),
                new_buffer.clone(),
                [Point::new(0, 0)..new_buffer.read(cx).max_point()],
                0,
                cx,
            );
            multibuffer
        });
        let task = cx.spawn({
            let multibuffer = multibuffer.clone();
            let path = path.to_owned();
            async move |cx| {
                let language = language_registry
                    .load_language_for_file_path(Path::new(&path))
                    .await
                    .log_err();
                new_buffer.update(cx, |buffer, cx| buffer.set_language(language.clone(), cx));
                let snapshot = new_buffer.read_with(cx, |buffer, _| buffer.snapshot());
                let diff = cx.new(|cx| {
                    // A hunk is only a snippet: it must never offer restore/apply operations.
                    BufferDiff::new(&snapshot, language, Some(language_registry), cx)
                });
                diff.update(cx, |diff, cx| {
                    diff.set_base_text(Some(hunk.old_text.into()), snapshot.text, cx)
                })
                .await;
                multibuffer.update(cx, |buffer, cx| {
                    buffer.add_diff(diff, cx);
                    buffer.set_all_diff_hunks_expanded(cx);
                });
            }
        });
        Self {
            header,
            buffer: multibuffer,
            _update_diff: task,
        }
    }
}

fn diff_change_paths(change: &acp_v2::DiffChange) -> Option<(Option<&Path>, Option<&Path>)> {
    match &change.operation {
        acp_v2::DiffChangeOperation::Add(change) => Some((None, Some(&change.path.0))),
        acp_v2::DiffChangeOperation::Delete(change) => Some((Some(&change.path.0), None)),
        acp_v2::DiffChangeOperation::Modify(change) => {
            Some((Some(&change.path.0), Some(&change.path.0)))
        }
        acp_v2::DiffChangeOperation::Move(change) | acp_v2::DiffChangeOperation::Copy(change) => {
            Some((Some(&change.old_path.0), Some(&change.path.0)))
        }
        _ => None,
    }
}

pub fn diff_change_label(change: &acp_v2::DiffChange) -> String {
    match &change.operation {
        acp_v2::DiffChangeOperation::Add(change) => format!("Added {}", change.path.0.display()),
        acp_v2::DiffChangeOperation::Delete(change) => {
            format!("Deleted {}", change.path.0.display())
        }
        acp_v2::DiffChangeOperation::Modify(change) => {
            format!("Modified {}", change.path.0.display())
        }
        acp_v2::DiffChangeOperation::Move(change) => format!(
            "Moved {} → {}",
            change.old_path.0.display(),
            change.path.0.display()
        ),
        acp_v2::DiffChangeOperation::Copy(change) => format!(
            "Copied {} → {}",
            change.old_path.0.display(),
            change.path.0.display()
        ),
        _ => "Unsupported file operation".to_owned(),
    }
}

pub enum Diff {
    Pending(PendingDiff),
    Finalized(FinalizedDiff),
}

impl Diff {
    pub fn finalized(
        path: String,
        file: Option<Arc<dyn File>>,
        old_text: Option<String>,
        new_text: String,
        language_registry: Arc<LanguageRegistry>,
        cx: &mut Context<Self>,
    ) -> Self {
        let multibuffer = cx.new(|_cx| MultiBuffer::without_headers(Capability::ReadOnly));
        let new_buffer = cx.new(|cx| {
            let text_buffer = TextBuffer::new(
                ReplicaId::LOCAL,
                cx.entity_id().as_non_zero_u64().into(),
                new_text,
            );
            let mut buffer = Buffer::build(text_buffer, file, Capability::ReadWrite, cx);
            let modeline = modeline::parse_modeline_from_rope(
                buffer.as_rope(),
                modeline::modeline_line_count(cx),
            );
            buffer.set_modeline(modeline, cx);
            buffer
        });
        let base_text_exists = old_text.is_some();
        let base_text = old_text.clone().unwrap_or(String::new()).into();
        let task = cx.spawn({
            let multibuffer = multibuffer.clone();
            let path = path.clone();
            let buffer = new_buffer.clone();
            async move |_, cx| {
                let language = language_registry
                    .load_language_for_file_path(Path::new(&path))
                    .await
                    .log_err();

                buffer.update(cx, |buffer, cx| buffer.set_language(language.clone(), cx));
                buffer.update(cx, |buffer, _| buffer.parsing_idle()).await;

                let diff = build_buffer_diff(
                    old_text.unwrap_or("".into()).into(),
                    base_text_exists,
                    &buffer,
                    cx,
                )
                .await?;

                multibuffer.update(cx, |multibuffer, cx| {
                    let hunk_ranges = {
                        let buffer = buffer.read(cx);
                        diff.read(cx)
                            .snapshot(cx)
                            .hunks_intersecting_range(
                                Anchor::min_for_buffer(buffer.remote_id())
                                    ..Anchor::max_for_buffer(buffer.remote_id()),
                                buffer,
                            )
                            .map(|diff_hunk| diff_hunk.buffer_range.to_point(buffer))
                            .collect::<Vec<_>>()
                    };

                    multibuffer.set_excerpts_for_path(
                        PathKey::for_buffer(&buffer, cx),
                        buffer.clone(),
                        hunk_ranges,
                        excerpt_context_lines(cx),
                        cx,
                    );
                    multibuffer.add_diff(diff, cx);
                });

                anyhow::Ok(())
            }
        });

        Self::Finalized(FinalizedDiff {
            multibuffer,
            path,
            base_text,
            new_buffer,
            _update_diff: task,
        })
    }

    pub fn new(buffer: Entity<Buffer>, cx: &mut Context<Self>) -> Self {
        let buffer_text_snapshot = buffer.read(cx).text_snapshot();
        let language = buffer.read(cx).language().cloned();
        let language_registry = buffer.read(cx).language_registry();
        let buffer_diff = cx.new(|cx| {
            let mut diff =
                BufferDiff::new_unchanged(&buffer_text_snapshot, language, language_registry, cx);
            diff.set_operations(Arc::new(buffer_diff::RestoreDiffOperations));
            diff
        });

        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::without_headers(Capability::ReadOnly);
            multibuffer.add_diff(buffer_diff.clone(), cx);
            multibuffer
        });

        Self::Pending(PendingDiff {
            multibuffer,
            base_text: Arc::from(buffer_text_snapshot.text().as_str()),
            _subscription: cx.observe(&buffer, |this, _, cx| {
                if let Diff::Pending(diff) = this {
                    diff.update(cx);
                }
            }),
            new_buffer: buffer,
            diff: buffer_diff,
            revealed_ranges: Vec::new(),
            update_diff: Task::ready(Ok(())),
        })
    }

    pub fn reveal_range(&mut self, range: Range<Anchor>, cx: &mut Context<Self>) {
        if let Self::Pending(diff) = self {
            diff.reveal_range(range, cx);
        }
    }

    pub fn finalize(&mut self, cx: &mut Context<Self>) {
        if let Self::Pending(diff) = self {
            *self = Self::Finalized(diff.finalize(cx));
        }
    }

    /// Returns the original text before any edits were applied.
    pub fn base_text(&self) -> &Arc<str> {
        match self {
            Self::Pending(PendingDiff { base_text, .. }) => base_text,
            Self::Finalized(FinalizedDiff { base_text, .. }) => base_text,
        }
    }

    /// Returns the buffer being edited (for pending diffs) or the snapshot buffer (for finalized diffs).
    pub fn buffer(&self) -> &Entity<Buffer> {
        match self {
            Self::Pending(PendingDiff { new_buffer, .. }) => new_buffer,
            Self::Finalized(FinalizedDiff { new_buffer, .. }) => new_buffer,
        }
    }

    pub fn file_path(&self, cx: &App) -> Option<String> {
        match self {
            Self::Pending(PendingDiff { new_buffer, .. }) => new_buffer
                .read(cx)
                .file()
                .map(|file| file.full_path(cx).to_string_lossy().into_owned()),
            Self::Finalized(FinalizedDiff { path, .. }) => Some(path.clone()),
        }
    }

    pub fn multibuffer(&self) -> &Entity<MultiBuffer> {
        match self {
            Self::Pending(PendingDiff { multibuffer, .. }) => multibuffer,
            Self::Finalized(FinalizedDiff { multibuffer, .. }) => multibuffer,
        }
    }

    pub fn to_markdown(&self, cx: &App) -> String {
        let buffer_text = self
            .multibuffer()
            .read(cx)
            .all_buffers()
            .iter()
            .map(|buffer| buffer.read(cx).text())
            .join("\n");
        let path = match self {
            Diff::Pending(PendingDiff {
                new_buffer: buffer, ..
            }) => buffer
                .read(cx)
                .file()
                .map(|file| file.path().display(file.path_style(cx))),
            Diff::Finalized(FinalizedDiff { path, .. }) => Some(path.as_str().into()),
        };
        format!(
            "Diff: {}\n```\n{}\n```\n",
            path.unwrap_or(MultiBuffer::DEFAULT_TITLE.into()),
            buffer_text
        )
    }

    pub fn has_revealed_range(&self, cx: &App) -> bool {
        !self.multibuffer().read(cx).is_empty()
    }

    pub fn needs_update(&self, old_text: &str, new_text: &str, cx: &App) -> bool {
        match self {
            Diff::Pending(PendingDiff {
                base_text,
                new_buffer,
                ..
            }) => {
                base_text.as_ref() != old_text
                    || !new_buffer.read(cx).as_rope().chunks().equals_str(new_text)
            }
            Diff::Finalized(FinalizedDiff {
                base_text,
                new_buffer,
                ..
            }) => {
                base_text.as_ref() != old_text
                    || !new_buffer.read(cx).as_rope().chunks().equals_str(new_text)
            }
        }
    }
}

pub struct PendingDiff {
    multibuffer: Entity<MultiBuffer>,
    base_text: Arc<str>,
    new_buffer: Entity<Buffer>,
    diff: Entity<BufferDiff>,
    revealed_ranges: Vec<Range<Anchor>>,
    _subscription: Subscription,
    update_diff: Task<Result<()>>,
}

impl PendingDiff {
    pub fn update(&mut self, cx: &mut Context<Diff>) {
        let buffer = self.new_buffer.clone();
        let buffer_diff = self.diff.clone();
        let base_text = self.base_text.clone();
        self.update_diff = cx.spawn(async move |diff, cx| {
            let text_snapshot = buffer.read_with(cx, |buffer, _| buffer.text_snapshot());
            let base_text_snapshot = buffer_diff.read_with(cx, |diff, cx| diff.base_text(cx));
            let update = buffer_diff
                .update(cx, |diff, cx| {
                    diff.update_diff(
                        text_snapshot.clone(),
                        &base_text_snapshot,
                        Some(base_text.clone()),
                        cx,
                    )
                })
                .await;
            buffer_diff.update(cx, |diff, cx| {
                diff.set_snapshot(update.clone(), cx);
            });
            diff.update(cx, |diff, cx| {
                if let Diff::Pending(diff) = diff {
                    diff.update_visible_ranges(cx);
                }
            })
        });
    }

    pub fn reveal_range(&mut self, range: Range<Anchor>, cx: &mut Context<Diff>) {
        self.revealed_ranges.push(range);
        self.update_visible_ranges(cx);
    }

    fn finalize(&self, cx: &mut Context<Diff>) -> FinalizedDiff {
        let ranges = self.excerpt_ranges(cx);
        let base_text = self.base_text.clone();
        let new_buffer = self.new_buffer.read(cx);

        let path = new_buffer
            .file()
            .map(|file| file.path().display(file.path_style(cx)))
            .unwrap_or(MultiBuffer::DEFAULT_TITLE.into())
            .into();
        let replica_id = new_buffer.replica_id();

        // Replace the buffer in the multibuffer with the snapshot
        let buffer = cx.new(|cx| {
            let language = self.new_buffer.read(cx).language().cloned();
            let file = self.new_buffer.read(cx).file().cloned();
            let modeline = self
                .new_buffer
                .read(cx)
                .modeline()
                .map(|modeline| (**modeline).clone());
            let buffer = TextBuffer::new_normalized(
                replica_id,
                cx.entity_id().as_non_zero_u64().into(),
                self.new_buffer.read(cx).line_ending(),
                self.new_buffer.read(cx).as_rope().clone(),
            );
            let mut buffer = Buffer::build(buffer, file, Capability::ReadWrite, cx);
            buffer.set_language(language, cx);
            buffer.set_modeline(modeline, cx);
            buffer
        });

        let buffer_diff = cx.spawn({
            let buffer = buffer.clone();
            async move |_this, cx| {
                buffer.update(cx, |buffer, _| buffer.parsing_idle()).await;
                build_buffer_diff(base_text, true, &buffer, cx).await
            }
        });

        let update_diff = cx.spawn(async move |this, cx| {
            let buffer_diff = buffer_diff.await?;
            this.update(cx, |this, cx| {
                this.multibuffer().update(cx, |multibuffer, cx| {
                    let path_key = PathKey::for_buffer(&buffer, cx);
                    multibuffer.clear(cx);
                    multibuffer.set_excerpts_for_path(
                        path_key,
                        buffer,
                        ranges,
                        excerpt_context_lines(cx),
                        cx,
                    );
                    multibuffer.add_diff(buffer_diff.clone(), cx);
                });

                cx.notify();
            })
        });

        FinalizedDiff {
            path,
            base_text: self.base_text.clone(),
            multibuffer: self.multibuffer.clone(),
            new_buffer: self.new_buffer.clone(),
            _update_diff: update_diff,
        }
    }

    fn update_visible_ranges(&mut self, cx: &mut Context<Diff>) {
        let ranges = self.excerpt_ranges(cx);
        self.multibuffer.update(cx, |multibuffer, cx| {
            multibuffer.set_excerpts_for_path(
                PathKey::for_buffer(&self.new_buffer, cx),
                self.new_buffer.clone(),
                ranges,
                excerpt_context_lines(cx),
                cx,
            );
            let end = multibuffer.len(cx);
            Some(multibuffer.snapshot(cx).offset_to_point(end).row + 1)
        });
        cx.notify();
    }

    fn excerpt_ranges(&self, cx: &App) -> Vec<Range<Point>> {
        let buffer = self.new_buffer.read(cx);
        let mut ranges = self
            .diff
            .read(cx)
            .snapshot(cx)
            .hunks_intersecting_range(
                Anchor::min_for_buffer(buffer.remote_id())
                    ..Anchor::max_for_buffer(buffer.remote_id()),
                buffer,
            )
            .map(|diff_hunk| diff_hunk.buffer_range.to_point(buffer))
            .collect::<Vec<_>>();
        ranges.extend(
            self.revealed_ranges
                .iter()
                .map(|range| range.to_point(buffer)),
        );
        ranges.sort_unstable_by_key(|range| (range.start, Reverse(range.end)));

        // Merge adjacent ranges
        let mut ranges = ranges.into_iter().peekable();
        let mut merged_ranges = Vec::new();
        while let Some(mut range) = ranges.next() {
            while let Some(next_range) = ranges.peek() {
                if range.end >= next_range.start {
                    range.end = range.end.max(next_range.end);
                    ranges.next();
                } else {
                    break;
                }
            }

            merged_ranges.push(range);
        }
        merged_ranges
    }
}

pub struct FinalizedDiff {
    path: String,
    base_text: Arc<str>,
    new_buffer: Entity<Buffer>,
    multibuffer: Entity<MultiBuffer>,
    _update_diff: Task<Result<()>>,
}

/// Resolves a worktree file handle for `path` so that the detached buffers
/// backing finalized diff cards resolve path-dependent settings (such as
/// .editorconfig and worktree-specific overrides) like the real buffer would.
pub fn file_for_path(project: &Entity<Project>, path: &Path, cx: &App) -> Option<Arc<dyn File>> {
    let project = project.read(cx);
    let project_path = project.find_project_path(path, cx)?;
    let worktree = project.worktree_for_id(project_path.worktree_id, cx)?;
    let entry = worktree
        .read(cx)
        .entry_for_path(&project_path.path)
        .cloned();
    let file: Arc<dyn File> = if let Some(entry) = entry {
        project::File::for_entry(entry, worktree)
    } else {
        let is_local = worktree.read(cx).is_local();
        Arc::new(project::File {
            worktree,
            path: project_path.path,
            disk_state: DiskState::New,
            entry_id: None,
            is_local,
            is_private: false,
        })
    };
    Some(file)
}

async fn build_buffer_diff(
    old_text: Arc<str>,
    base_text_exists: bool,
    buffer: &Entity<Buffer>,
    cx: &mut AsyncApp,
) -> Result<Entity<BufferDiff>> {
    let language = cx.update(|cx| buffer.read(cx).language().cloned());
    let language_registry = cx.update(|cx| buffer.read(cx).language_registry());
    let buffer = cx.update(|cx| buffer.read(cx).snapshot());
    let base_text = base_text_exists.then(|| old_text);

    let diff = cx.new(|cx| {
        let mut diff = BufferDiff::new(&buffer, language, language_registry, cx);
        diff.set_operations(Arc::new(buffer_diff::RestoreDiffOperations));
        diff
    });
    diff.update(cx, |diff, cx| {
        diff.set_base_text(base_text, buffer.text, cx)
    })
    .await;
    Ok(diff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use indoc::indoc;
    use language::Buffer;
    use project::FakeFs;
    use serde_json::json;
    use settings::SettingsStore;
    use std::num::NonZeroU32;
    use util::path;

    use crate::Diff;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    fn source(patch: &str, changes: serde_json::Value) -> acp_v2::Diff {
        serde_json::from_value(json!({
            "changes": changes,
            "patch": {"format": "git_patch", "text": patch}
        }))
        .expect("diff fixture")
    }

    #[test]
    fn test_patch_file_association() {
        let bare = "@@ -10 +10 @@\n-before\n+after\n";
        let single = json!([{"operation": "modify", "path": "/one"}]);
        let parsed = DiffPatch::parse(&source(bare, single.clone())).expect("unique bare hunk");
        assert_eq!(parsed[0].0, 0);
        assert_eq!(parsed[0].1.new_path.as_deref(), Some("/one"));
        assert!(
            DiffPatch::parse(&source(
                bare,
                json!([
                    {"operation": "modify", "path": "/one"},
                    {"operation": "modify", "path": "/two"}
                ])
            ))
            .is_err(),
            "bare hunks must not guess a file"
        );
        let unified = "--- /one\n+++ /one\n@@ -10 +10 @@\n-before\n+after\n";
        assert!(DiffPatch::parse(&source(unified, single)).is_ok());
        assert!(
            DiffPatch::parse(&source(
                unified,
                json!([
                    {"operation": "modify", "path": "/two"}
                ])
            ))
            .is_err(),
            "mismatched paths must fall back"
        );
        assert!(
            DiffPatch::parse(&source(
                unified,
                json!([
                    {"operation": "modify", "path": "/one", "fileType": "binary"}
                ])
            ))
            .is_err(),
            "non-text file must not be rendered as a text diff"
        );
        assert!(
            DiffPatch::parse(&source(
                bare,
                json!([
                    {"operation": "add", "path": "/one"}
                ])
            ))
            .is_err(),
            "added file cannot have deleted content"
        );
        let rename = "--- /old\n+++ /new\n@@ -1 +1 @@\n-before\n+after\n";
        assert!(
            DiffPatch::parse(&source(
                rename,
                json!([
                    {"operation": "move", "oldPath": "/old", "path": "/new"}
                ])
            ))
            .is_ok()
        );
    }

    #[test]
    fn test_patch_file_association_rejects_ambiguity_and_unknown_types() {
        let patch = "diff --git /one /one\n--- /one\n+++ /one\n@@ -1 +1 @@\n-old\n+new\n";
        assert!(
            DiffPatch::parse(&source(
                patch,
                json!([
                    {"operation": "modify", "path": "/one"},
                    {"operation": "modify", "path": "/one"}
                ])
            ))
            .is_err(),
            "duplicate declared paths are ambiguous"
        );
        assert!(
            DiffPatch::parse(&source(
                &patch.repeat(2),
                json!([
                    {"operation": "modify", "path": "/one"}
                ])
            ))
            .is_err(),
            "repeated patch sections must not replace each other"
        );
        for file_type in ["_future", "directory", "symlink"] {
            assert!(
                DiffPatch::parse(&source(
                    patch,
                    json!([
                        {"operation": "modify", "path": "/one", "fileType": file_type}
                    ])
                ))
                .is_err(),
                "{file_type} is not an unspecified file type"
            );
        }
        assert!(
            DiffPatch::parse(&source(
                patch,
                json!([
                    {"operation": "_future", "path": "/one"}
                ])
            ))
            .is_err()
        );
        assert!(
            DiffPatch::parse(&source(
                "--- /old\n+++ /new\n@@ -1 +1 @@\n-before\n+after\n",
                json!([{"operation": "copy", "oldPath": "/old", "path": "/new"}])
            ))
            .is_ok()
        );
    }

    #[test]
    fn test_patch_file_association_preserves_declared_order_for_large_patches() {
        let changes = (0..1_000)
            .map(|index| json!({"operation": "modify", "path": format!("/file-{index}")}))
            .collect::<Vec<_>>();
        let patch = (0..1_000)
            .rev()
            .map(|index| format!(
                "diff --git /file-{index} /file-{index}\n--- /file-{index}\n+++ /file-{index}\n@@ -1 +1 @@\n-old\n+new\n"
            ))
            .collect::<String>();
        let parsed = DiffPatch::parse(&source(&patch, json!(changes))).expect("large patch");
        assert_eq!(parsed.len(), 1_000);
        for ((change_index, file), expected) in parsed.iter().zip((0..1_000).rev()) {
            assert_eq!(*change_index, expected);
            assert_eq!(file.new_path.as_ref(), Some(&format!("/file-{expected}")));
        }
    }

    #[gpui::test]
    async fn test_patch_hunks_render_read_only_snippets(cx: &mut TestAppContext) {
        let patch = indoc! {"
            diff --git /one /one
            --- /one
            +++ /one
            @@ -100,2 +100,2 @@
             context
            -before
            +after
            @@ -900 +900 @@
            -old
            +new
            diff --git /deleted /deleted
            --- /deleted
            +++ /dev/null
            @@ -1 +0,0 @@
            -deleted
            diff --git /added /added
            --- /dev/null
            +++ /added
            @@ -0,0 +1 @@
            +added
        "};
        let source = source(
            patch,
            json!([
                {"operation": "modify", "path": "/one"},
                {"operation": "delete", "path": "/deleted"},
                {"operation": "add", "path": "/added"},
                {"operation": "modify", "path": "/image", "fileType": "binary"},
                {"operation": "move", "oldPath": "/old", "path": "/new"}
            ]),
        );
        let render = cx.update(|cx| {
            let languages = Arc::new(LanguageRegistry::test(cx.background_executor().clone()));
            DiffPatch::new(&source, &languages, cx)
        });
        cx.run_until_parked();
        assert!(render.fallback.is_none());
        assert_eq!(
            render.files.len(),
            5,
            "patchless files must remain represented"
        );
        assert_eq!(render.files[0].hunks.len(), 2);
        assert_eq!(
            render.files[0].hunks[0].header.as_ref(),
            "@@ -100,2 +100,2 @@"
        );
        assert_eq!(render.files[0].hunks[1].header.as_ref(), "@@ -900 +900 @@");
        assert!(render.files[3].hunks.is_empty());
        assert!(render.files[4].hunks.is_empty());
        cx.update(|cx| {
            for (file_index, expected) in [
                (0, "context\nbefore\nafter\n"),
                (1, "deleted\n"),
                (2, "added\n"),
            ] {
                let buffer = render.files[file_index].hunks[0].buffer.read(cx);
                assert!(buffer.read_only());
                let snapshot = buffer.snapshot(cx);
                assert_eq!(snapshot.text(), expected);
                assert!(snapshot.diff_hunks().next().is_some());
                for source_buffer in buffer.all_buffers() {
                    assert_eq!(source_buffer.read(cx).capability(), Capability::ReadOnly);
                }
            }
            assert_eq!(
                render.files[0].hunks[1].buffer.read(cx).snapshot(cx).text(),
                "old\nnew\n",
                "sparse hunks must not synthesize the intervening 798 lines"
            );
        });
    }

    #[gpui::test]
    fn test_patch_fallback_retains_file_rows_and_raw_text(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let languages = Arc::new(LanguageRegistry::test(cx.background_executor().clone()));
            let mut source = source(
                "not a patch\n```",
                json!([
                    {"operation": "modify", "path": "/one"}
                ]),
            );
            let render = DiffPatch::new(&source, &languages, cx);
            assert_eq!(render.files.len(), 1);
            assert!(render.files[0].hunks.is_empty());
            let fallback = render.fallback.expect("malformed patch fallback");
            assert!(fallback.read(cx).source().contains("not a patch\n```"));
            source.patch = None;
            let render = DiffPatch::new(&source, &languages, cx);
            assert_eq!(render.files.len(), 1);
            assert!(render.files[0].hunks.is_empty());
            assert!(render.fallback.is_none());
            source.patch = Some(acp_v2::DiffPatch::new("opaque format\n``` nested"));
            source.patch.as_mut().expect("patch").format =
                acp_v2::DiffPatchFormat::Other("_future".into());
            let render = DiffPatch::new(&source, &languages, cx);
            assert!(render.fallback.is_some());
            let content = crate::ToolCallContent::DiffPatch { source, render };
            assert!(
                content
                    .to_markdown(cx)
                    .contains("opaque format\n``` nested"),
                "export must retain supplied text even when its format is unsupported"
            );
        });
    }

    #[gpui::test]
    async fn test_pending_diff(cx: &mut TestAppContext) {
        let buffer = cx.new(|cx| Buffer::local("hello!", cx));
        let _diff = cx.new(|cx| Diff::new(buffer.clone(), cx));
        buffer.update(cx, |buffer, cx| {
            buffer.set_text("HELLO!", cx);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_finalized_diff_carries_file_association(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "a.txt": "one\ntwo\n" }))
            .await;
        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        let language_registry = project.read_with(cx, |project, _| project.languages().clone());

        let diff = cx.new(|cx| {
            let file = file_for_path(&project, Path::new(path!("/project/a.txt")), cx);
            assert!(file.is_some());
            Diff::finalized(
                "a.txt".to_string(),
                file,
                Some("one\ntwo\n".to_string()),
                "one\nTWO\n".to_string(),
                language_registry,
                cx,
            )
        });
        cx.run_until_parked();

        diff.read_with(cx, |diff, cx| {
            let buffers = diff.multibuffer().read(cx).all_buffers();
            assert_eq!(buffers.len(), 1);
            for buffer in buffers {
                let buffer = buffer.read(cx);
                let file = buffer.file().expect("diff buffer should have a file");
                assert_eq!(file.path().as_unix_str(), "a.txt");
            }
        });
    }

    #[gpui::test]
    async fn test_finalized_diff_carries_file_association_for_new_file(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({})).await;
        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        let language_registry = project.read_with(cx, |project, _| project.languages().clone());

        let diff = cx.new(|cx| {
            let file = file_for_path(&project, Path::new(path!("/project/new.txt")), cx)
                .expect("new project path should have a file association");
            assert_eq!(file.path().as_unix_str(), "new.txt");
            assert_eq!(file.disk_state(), DiskState::New);
            Diff::finalized(
                "new.txt".to_string(),
                Some(file),
                None,
                "new contents\n".to_string(),
                language_registry,
                cx,
            )
        });
        cx.run_until_parked();

        diff.read_with(cx, |diff, cx| {
            let buffers = diff.multibuffer().read(cx).all_buffers();
            assert_eq!(buffers.len(), 1);
            let buffer = buffers
                .iter()
                .next()
                .expect("diff should contain one buffer")
                .read(cx);
            let file = buffer.file().expect("diff buffer should have a file");
            assert_eq!(file.path().as_unix_str(), "new.txt");
            assert_eq!(file.disk_state(), DiskState::New);
        });
    }

    #[gpui::test]
    async fn test_finalized_diff_parses_modeline(cx: &mut TestAppContext) {
        init_test(cx);
        let language_registry = Arc::new(LanguageRegistry::test(cx.executor()));
        let diff = cx.new(|cx| {
            Diff::finalized(
                "a.c".to_string(),
                None,
                Some("one\n".to_string()),
                "/* vim: set ts=3 noet: */\none\ntwo\n".to_string(),
                language_registry,
                cx,
            )
        });
        cx.run_until_parked();

        diff.read_with(cx, |diff, cx| {
            let buffers = diff.multibuffer().read(cx).all_buffers();
            assert_eq!(buffers.len(), 1);
            for buffer in buffers {
                let modeline = buffer
                    .read(cx)
                    .modeline()
                    .expect("diff buffer should have a modeline");
                assert_eq!(modeline.tab_size, NonZeroU32::new(3));
                assert_eq!(modeline.hard_tabs, Some(true));
            }
        });
    }

    #[gpui::test]
    async fn test_finalize_preserves_modeline(cx: &mut TestAppContext) {
        init_test(cx);
        let buffer = cx.new(|cx| Buffer::local("one\ntwo\n", cx));
        buffer.update(cx, |buffer, cx| {
            buffer.set_modeline(
                Some(language::ModelineSettings {
                    tab_size: NonZeroU32::new(3),
                    ..Default::default()
                }),
                cx,
            );
        });
        let diff = cx.new(|cx| Diff::new(buffer.clone(), cx));
        buffer.update(cx, |buffer, cx| buffer.set_text("one\nTWO\n", cx));
        cx.run_until_parked();

        diff.update(cx, |diff, cx| diff.finalize(cx));
        cx.run_until_parked();

        diff.read_with(cx, |diff, cx| {
            let buffers = diff.multibuffer().read(cx).all_buffers();
            assert_eq!(buffers.len(), 1);
            for buffer in buffers {
                let modeline = buffer
                    .read(cx)
                    .modeline()
                    .expect("finalized diff buffer should keep its modeline");
                assert_eq!(modeline.tab_size, NonZeroU32::new(3));
            }
        });
    }

    #[gpui::test]
    async fn test_finalize_preserves_file_association(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "a.txt": "one\ntwo\n" }))
            .await;
        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/project/a.txt"), cx)
            })
            .await
            .unwrap();

        let diff = cx.new(|cx| Diff::new(buffer.clone(), cx));
        buffer.update(cx, |buffer, cx| buffer.set_text("one\nTWO\n", cx));
        cx.run_until_parked();

        diff.update(cx, |diff, cx| diff.finalize(cx));
        cx.run_until_parked();

        diff.read_with(cx, |diff, cx| {
            let buffers = diff.multibuffer().read(cx).all_buffers();
            assert_eq!(buffers.len(), 1);
            for buffer in buffers {
                let buffer = buffer.read(cx);
                let file = buffer
                    .file()
                    .expect("finalized diff buffer should keep its file");
                assert_eq!(file.path().as_unix_str(), "a.txt");
            }
        });
    }
}
