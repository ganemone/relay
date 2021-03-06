/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use crate::artifact_map::ArtifactMap;
use crate::config::Config;
use crate::errors::{Error, Result};
use crate::watchman::{
    categorize_files, extract_graphql_strings_from_file, read_to_string, Clock, FileGroup,
    FileSourceResult,
};
use common::{PerfLogEvent, PerfLogger};
use fnv::{FnvHashMap, FnvHashSet};
use graphql_syntax::GraphQLSource;
use interner::StringKey;
use io::BufReader;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::{fs::File, io, sync::Arc};

/// Name of a compiler project.
pub type ProjectName = StringKey;

/// Name of a source set; a source set corresponds to a set fo files
/// that can be shared by multiple compiler projects
pub type SourceSetName = StringKey;

/// Set of project names.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProjectSet {
    ProjectName(ProjectName),
    ProjectNames(Vec<ProjectName>),
}
impl ProjectSet {
    /// Inserts a new project name into this set.
    pub fn insert(&mut self, project_name: ProjectName) {
        match self {
            ProjectSet::ProjectName(existing_name) => {
                assert!(*existing_name != project_name);
                *self = ProjectSet::ProjectNames(vec![*existing_name, project_name]);
            }
            ProjectSet::ProjectNames(existing_names) => {
                assert!(!existing_names.contains(&project_name));
                existing_names.push(project_name);
            }
        }
    }
}

/// Represents the name of the source set, or list of source sets
#[derive(Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(untagged)]
pub enum SourceSet {
    SourceSetName(SourceSetName),
    SourceSetNames(Vec<SourceSetName>),
}

impl fmt::Display for SourceSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceSet::SourceSetName(name) => write!(f, "{}", name),
            SourceSet::SourceSetNames(names) => write!(
                f,
                "{}",
                names
                    .iter()
                    .map(|name| name.to_string())
                    .collect::<Vec<String>>()
                    .join(",")
            ),
        }
    }
}

type GraphQLSourceSet = FnvHashMap<PathBuf, Vec<GraphQLSource>>;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct GraphQLSources {
    pub pending: GraphQLSourceSet,
    pub processed: GraphQLSourceSet,
}

impl GraphQLSources {
    /// Merges additional pending sources into this states pending sources.
    fn merge_pending_sources(&mut self, additional_pending_sources: GraphQLSourceSet) {
        self.pending.extend(additional_pending_sources.into_iter());
    }

    fn commit_pending_sources(&mut self) {
        for (file_name, pending_graphql_sources) in self.pending.drain() {
            if pending_graphql_sources.is_empty() {
                self.processed.remove(&file_name);
            } else {
                self.processed.insert(file_name, pending_graphql_sources);
            }
        }
    }
}

pub type SchemaSources = FnvHashMap<ProjectName, Vec<String>>;

#[derive(Serialize, Deserialize, Debug)]
pub enum ArtifactMapKind {
    /// A simple set of paths of generated files. This kind is used when the
    /// compiler starts without a saved state and doesn't know the connection
    /// between generated files and the artifacts that produced them.
    Unconnected(FnvHashSet<PathBuf>),
    /// A known mapping from input documents to generated outputs.
    Mapping(ArtifactMap),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct CompilerState {
    pub graphql_sources: FnvHashMap<SourceSetName, GraphQLSources>,
    pub schemas: SchemaSources,
    pub extensions: SchemaSources,
    pub artifacts: FnvHashMap<ProjectName, Arc<ArtifactMapKind>>,
    pub clock: Clock,
}

impl CompilerState {
    fn set_pending_source_set(
        &mut self,
        source_set_name: SourceSetName,
        source_set: GraphQLSourceSet,
    ) {
        let pending_entry = &mut self
            .graphql_sources
            .entry(source_set_name)
            .or_default()
            .pending;
        if pending_entry.is_empty() {
            *pending_entry = source_set;
        } else {
            pending_entry.extend(source_set);
        }
    }

    pub fn from_file_source_changes(
        config: &Config,
        file_source_changes: &FileSourceResult,
        setup_event: &impl PerfLogEvent,
        perf_logger: &impl PerfLogger,
    ) -> Result<Self> {
        let categorized = setup_event.time("categorize_files_time", || {
            categorize_files(config, &file_source_changes.files)
        });

        let mut result = Self {
            graphql_sources: Default::default(),
            artifacts: Default::default(),
            extensions: Default::default(),
            schemas: Default::default(),
            clock: file_source_changes.clock.clone(),
        };

        for (category, files) in categorized {
            match category {
                FileGroup::Source { source_set } => {
                    let log_event = perf_logger.create_event("categorize");
                    log_event.string("source_set_name", source_set.to_string());
                    let extract_timer = log_event.start("extract_graphql_strings_from_file_time");
                    let sources = files
                        .par_iter()
                        .filter(|file| *file.exists)
                        .filter_map(|file| {
                            match extract_graphql_strings_from_file(
                                &file_source_changes.resolved_root,
                                &file,
                            ) {
                                Ok(graphql_strings) if graphql_strings.is_empty() => None,
                                Ok(graphql_strings) => {
                                    Some(Ok(((*file.name).to_owned(), graphql_strings)))
                                }
                                Err(err) => Some(Err(err)),
                            }
                        })
                        .collect::<Result<_>>()?;
                    log_event.stop(extract_timer);
                    match source_set {
                        SourceSet::SourceSetName(source_set_name) => {
                            result.set_pending_source_set(source_set_name, sources);
                        }
                        SourceSet::SourceSetNames(names) => {
                            for source_set_name in names {
                                result.set_pending_source_set(source_set_name, sources.clone());
                            }
                        }
                    }
                }
                FileGroup::Schema { project_set } => {
                    let schema_sources = files
                        .iter()
                        .map(|file| read_to_string(&file_source_changes.resolved_root, file))
                        .collect::<Result<Vec<String>>>()?;
                    match project_set {
                        ProjectSet::ProjectName(project_name) => {
                            result.schemas.insert(project_name, schema_sources);
                        }
                        ProjectSet::ProjectNames(project_names) => {
                            for project_name in project_names {
                                result.schemas.insert(project_name, schema_sources.clone());
                            }
                        }
                    };
                }
                FileGroup::Extension { project_set } => {
                    let extension_sources: Vec<String> = files
                        .iter()
                        .map(|file| read_to_string(&file_source_changes.resolved_root, file))
                        .collect::<Result<Vec<String>>>()?;

                    match project_set {
                        ProjectSet::ProjectName(project_name) => {
                            result.extensions.insert(project_name, extension_sources);
                        }
                        ProjectSet::ProjectNames(project_names) => {
                            for project_name in project_names {
                                result
                                    .extensions
                                    .insert(project_name, extension_sources.clone());
                            }
                        }
                    };
                }
                FileGroup::Generated { project_name } => {
                    result.artifacts.insert(
                        project_name,
                        Arc::new(ArtifactMapKind::Unconnected(
                            files
                                .into_iter()
                                .map(|file| file.name.into_inner())
                                .collect(),
                        )),
                    );
                }
            }
        }

        Ok(result)
    }

    pub fn has_pending_changes(&self) -> bool {
        self.graphql_sources
            .values()
            .any(|sources| !sources.pending.is_empty())
    }

    pub fn project_has_pending_changes(&self, project_name: ProjectName) -> bool {
        self.graphql_sources
            .get(&project_name)
            .map_or(false, |sources| !sources.pending.is_empty())
    }

    pub fn has_processed_changes(&self) -> bool {
        self.graphql_sources
            .values()
            .any(|sources| !sources.processed.is_empty())
    }

    /// Merges the provided pending changes from the file source into the compiler state.
    /// Returns a boolean indicating if any new changes were merged.
    pub fn merge_file_source_changes(
        &mut self,
        config: &Config,
        file_source_changes: &FileSourceResult,
        setup_event: &impl PerfLogEvent,
        perf_logger: &impl PerfLogger,
    ) -> Result<bool> {
        let mut has_changed = false;

        let categorized = setup_event.time("categorize_files_time", || {
            categorize_files(config, &file_source_changes.files)
        });

        for (category, files) in categorized {
            match category {
                FileGroup::Source { source_set } => {
                    // TODO: possible optimization to only set this if the
                    // extracted sources actually differ.
                    has_changed = true;

                    let log_event = perf_logger.create_event("categorize");
                    log_event.string("source_set_name", source_set.to_string());
                    let extract_timer = log_event.start("extract_graphql_strings_from_file_time");
                    let sources = files
                        .par_iter()
                        .map(|file| {
                            let graphql_strings = if *file.exists {
                                extract_graphql_strings_from_file(
                                    &file_source_changes.resolved_root,
                                    &file,
                                )?
                            } else {
                                Vec::new()
                            };
                            Ok(((*file.name).to_owned(), graphql_strings))
                        })
                        .collect::<Result<_>>()?;
                    log_event.stop(extract_timer);
                    match source_set {
                        SourceSet::SourceSetName(source_set_name) => {
                            self.graphql_sources
                                .entry(source_set_name)
                                .or_default()
                                .merge_pending_sources(sources);
                        }
                        SourceSet::SourceSetNames(names) => {
                            for source_set_name in names {
                                self.graphql_sources
                                    .entry(source_set_name)
                                    .or_default()
                                    .merge_pending_sources(sources.clone());
                            }
                        }
                    }
                }
                FileGroup::Schema { project_set } => {
                    has_changed = true;

                    let schema_sources = files
                        .iter()
                        .map(|file| read_to_string(&file_source_changes.resolved_root, file))
                        .collect::<Result<Vec<String>>>()?;
                    match project_set {
                        ProjectSet::ProjectName(project_name) => {
                            self.schemas.insert(project_name, schema_sources);
                        }
                        ProjectSet::ProjectNames(project_names) => {
                            for project_name in project_names {
                                self.schemas.insert(project_name, schema_sources.clone());
                            }
                        }
                    };
                }
                FileGroup::Extension { project_set } => {
                    has_changed = true;

                    let extension_sources: Vec<String> = files
                        .iter()
                        .map(|file| read_to_string(&file_source_changes.resolved_root, file))
                        .collect::<Result<Vec<String>>>()?;

                    match project_set {
                        ProjectSet::ProjectName(project_name) => {
                            self.extensions.insert(project_name, extension_sources);
                        }
                        ProjectSet::ProjectNames(project_names) => {
                            for project_name in project_names {
                                self.extensions
                                    .insert(project_name, extension_sources.clone());
                            }
                        }
                    };
                }
                FileGroup::Generated { project_name: _ } => {
                    // TODO
                }
            }
        }

        Ok(has_changed)
    }

    pub fn complete_compilation(&mut self) {
        for sources in self.graphql_sources.values_mut() {
            sources.commit_pending_sources();
        }
    }

    pub fn serialize_to_file(&self, path: &PathBuf) -> Result<()> {
        let writer = File::create(path).map_err(|err| Error::WriteFileError {
            file: path.clone(),
            source: err,
        })?;
        serde_json::to_writer(writer, self).map_err(|err| Error::SerializationError {
            file: path.clone(),
            source: err,
        })?;
        Ok(())
    }

    pub fn deserialize_from_file(path: &PathBuf) -> Result<Self> {
        let file = File::open(path).map_err(|err| Error::ReadFileError {
            file: path.clone(),
            source: err,
        })?;
        let reader = BufReader::new(file);
        let state = serde_json::from_reader(reader).map_err(|err| Error::DeserializationError {
            file: path.clone(),
            source: err,
        })?;
        Ok(state)
    }
}
