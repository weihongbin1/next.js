use std::{
    async_iter::AsyncIterator,
    pin::Pin,
    task::{Context, Poll},
};

use anyhow::Result;
use turbo_tasks::{
    primitives::{BoolVc, StringsVc},
    CompletionVc,
};
use turbo_tasks_fs::{DirectoryContent, DirectoryEntry, FileSystemEntryType, FileSystemPathVc};
use turbopack_dev_server::source::specificity::SpecificityVc;

use crate::next_config::NextConfigVc;

/// A final route in the pages directory.
#[turbo_tasks::value]
pub enum PagesStructureItem {
    Page {
        project_path: FileSystemPathVc,
        next_router_path: FileSystemPathVc,
        specificity: SpecificityVc,
    },
    Api {
        project_path: FileSystemPathVc,
        next_router_path: FileSystemPathVc,
        specificity: SpecificityVc,
    },
}

#[turbo_tasks::value_impl]
impl PagesStructureItemVc {
    #[turbo_tasks::function]
    async fn new(
        project_path: FileSystemPathVc,
        next_router_path: FileSystemPathVc,
        specificity: SpecificityVc,
        is_api: BoolVc,
    ) -> Result<Self> {
        if *is_api.await? {
            Ok(PagesStructureItem::Api {
                project_path,
                next_router_path,
                specificity,
            }
            .cell())
        } else {
            Ok(PagesStructureItem::Page {
                project_path,
                next_router_path,
                specificity,
            }
            .cell())
        }
    }

    /// Returns a completion that changes when any route in the whole tree
    /// changes.
    #[turbo_tasks::function]
    pub async fn routes_changed(self) -> Result<CompletionVc> {
        match *self.await? {
            PagesStructureItem::Page {
                next_router_path, ..
            } => next_router_path.await?,
            PagesStructureItem::Api {
                next_router_path, ..
            } => next_router_path.await?,
        };
        Ok(CompletionVc::new())
    }
}

/// A (sub)directory in the pages directory with all analyzed routes and
/// folders.
#[turbo_tasks::value]
pub struct PagesStructure {
    pub project_path: FileSystemPathVc,
    pub items: Vec<PagesStructureItemVc>,
    pub children: Vec<PagesStructureVc>,
}

#[turbo_tasks::value_impl]
impl PagesStructureVc {
    /// Returns the path to the directory of this structure in the project file
    /// system.
    #[turbo_tasks::function]
    pub async fn project_path(self) -> Result<FileSystemPathVc> {
        Ok(self.await?.project_path)
    }

    /// Returns a completion that changes when any route in the whole tree
    /// changes.
    #[turbo_tasks::function]
    pub async fn routes_changed(self) -> Result<CompletionVc> {
        for item in self.await?.items.iter() {
            item.routes_changed().await?;
        }
        for child in self.await?.children.iter() {
            child.routes_changed().await?;
        }
        Ok(CompletionVc::new())
    }
}

#[turbo_tasks::value(transparent)]
pub struct OptionPagesStructure(Option<PagesStructureVc>);

#[turbo_tasks::value_impl]
impl OptionPagesStructureVc {
    #[turbo_tasks::function]
    pub async fn routes_changed(self) -> Result<CompletionVc> {
        if let Some(pages_structure) = *self.await? {
            pages_structure.routes_changed().await?;
        }
        Ok(CompletionVc::new())
    }
}

/// Finds and returns the [PagesStructure] of the pages directory if existing.
#[turbo_tasks::function]
pub async fn find_pages_structure(
    project_root: FileSystemPathVc,
    next_router_root: FileSystemPathVc,
    next_config: NextConfigVc,
) -> Result<OptionPagesStructureVc> {
    let pages_root = project_root.join("pages");
    let pages_root = if *pages_root.get_type().await? == FileSystemEntryType::Directory {
        pages_root
    } else {
        let src_pages_root = project_root.join("src/pages");
        if *src_pages_root.get_type().await? == FileSystemEntryType::Directory {
            src_pages_root
        } else {
            return Ok(OptionPagesStructureVc::cell(None));
        }
    }
    .resolve()
    .await?;

    Ok(OptionPagesStructureVc::cell(Some(
        get_pages_structure_for_directory(
            pages_root,
            next_router_root,
            SpecificityVc::exact(),
            0,
            next_router_root.join("api"),
            next_config.page_extensions(),
        ),
    )))
}

/// Handles a directory in the pages directory (or the pages directory itself).
/// Calls itself recursively for sub directories or the
/// [create_page_source_for_file] method for files.
#[turbo_tasks::function]
async fn get_pages_structure_for_directory(
    project_path: FileSystemPathVc,
    next_router_path: FileSystemPathVc,
    specificity: SpecificityVc,
    position: u32,
    next_router_api_root: FileSystemPathVc,
    page_extensions: StringsVc,
) -> Result<PagesStructureVc> {
    let page_extensions_raw = &*page_extensions.await?;

    let mut children = vec![];
    let mut items = vec![];
    let dir_content = project_path.read_dir().await?;
    if let DirectoryContent::Entries(entries) = &*dir_content {
        for (name, entry) in entries.iter() {
            let specificity = if name.starts_with("[[") || name.starts_with("[...") {
                specificity.with_catch_all(position)
            } else if name.starts_with('[') {
                specificity.with_dynamic_segment(position)
            } else {
                specificity
            };
            match entry {
                DirectoryEntry::File(file_project_path) => {
                    if let Some((basename, extension)) = name.rsplit_once('.') {
                        if page_extensions_raw
                            .iter()
                            .any(|allowed| allowed == extension)
                        {
                            let next_router_path = if basename == "index" {
                                next_router_path
                            } else {
                                next_router_path.join(basename)
                            };
                            items.push((
                                name,
                                PagesStructureItemVc::new(
                                    *file_project_path,
                                    next_router_path,
                                    specificity,
                                    next_router_path.is_inside(next_router_api_root),
                                ),
                            ))
                        }
                    }
                }
                DirectoryEntry::Directory(dir_project_path) => {
                    children.push((
                        name,
                        get_pages_structure_for_directory(
                            *dir_project_path,
                            next_router_path.join(name),
                            specificity,
                            position + 1,
                            next_router_api_root,
                            page_extensions,
                        ),
                    ));
                }
                _ => {}
            }
        }
    }

    // Ensure deterministic order since read_dir is not deterministic
    items.sort_by_key(|(k, _)| *k);

    // Ensure deterministic order since read_dir is not deterministic
    children.sort_by_key(|(k, _)| *k);

    Ok(PagesStructure {
        project_path,
        items: items.into_iter().map(|(_, v)| v).collect(),
        children: children.into_iter().map(|(_, v)| v).collect(),
    }
    .cell())
}
