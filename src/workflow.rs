use std::path::{Path, PathBuf};

use tokio::sync::mpsc;

use crate::{
    Event, FileWatch, Instance, InstanceConfig, InstanceError, SourceChangeSummary,
    VirtualSourceFile,
};

/// Source-directory workflow helper for development reloads and release bundles.
#[derive(Debug, Clone)]
pub struct SourceProject {
    source_dir: PathBuf,
}

/// Filesystem change classification for a watched [`SourceProject`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadAction {
    None,
    Remount,
    CssChanged(Vec<PathBuf>),
}

/// Active watch handle for a [`SourceProject`].
#[derive(Debug)]
pub struct SourceProjectWatch {
    source_dir: PathBuf,
    watch: FileWatch,
}

impl SourceProject {
    pub fn new(source_dir: impl Into<PathBuf>) -> Self {
        Self {
            source_dir: source_dir.into(),
        }
    }

    pub fn source_dir(&self) -> &Path {
        &self.source_dir
    }

    /// Mount directly from the source directory. This requires the
    /// `jsx-compiler` feature for `.js`, `.mjs`, `.jsx`, `.tsx`, and `.ts`
    /// sources.
    pub fn mount_live(
        &self,
        config: InstanceConfig,
    ) -> Result<(Instance, mpsc::UnboundedReceiver<Event>), InstanceError> {
        #[cfg(not(test))]
        {
            Instance::new_from_dir(config, &self.source_dir)
        }
        #[cfg(test)]
        {
            Ok(Instance::new_from_dir(config, &self.source_dir))
        }
    }

    /// Mount a generated release bundle, typically from an `include!` module
    /// emitted by `solite_build::workflow::bundle_for_cargo`.
    pub fn mount_bundle(
        &self,
        config: InstanceConfig,
        files: Vec<VirtualSourceFile>,
    ) -> Result<(Instance, mpsc::UnboundedReceiver<Event>), InstanceError> {
        let _ = self;
        #[cfg(not(test))]
        {
            Instance::new_from_virtual_files(config, files)
        }
        #[cfg(test)]
        {
            Ok(Instance::new_from_virtual_files(config, files))
        }
    }

    pub fn watch(&self) -> notify::Result<SourceProjectWatch> {
        Ok(SourceProjectWatch {
            source_dir: self.source_dir.clone(),
            watch: Instance::watch_files(&self.source_dir)?,
        })
    }

    /// Reload changed imported CSS files in-place. Returns `false` when the
    /// paths were not imported CSS modules by the currently mounted instance.
    pub fn reload_imported_css(&self, instance: &mut Instance, paths: &[PathBuf]) -> bool {
        let _ = self;
        instance.reload_imported_stylesheets(paths)
    }
}

impl SourceProjectWatch {
    pub fn poll(&self) -> ReloadAction {
        match self.summary() {
            SourceChangeSummary {
                bundle_rebuild: true,
                ..
            } => ReloadAction::Remount,
            SourceChangeSummary {
                css_reload: true,
                css_paths,
                ..
            } => ReloadAction::CssChanged(css_paths),
            _ => ReloadAction::None,
        }
    }

    pub fn summary(&self) -> SourceChangeSummary {
        self.watch.poll_source_changes(&self.source_dir)
    }
}
