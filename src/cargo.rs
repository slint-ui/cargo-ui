/* Copyright © 2021 SixtyFPS GmbH <info@sixtyfps.info>
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */

use super::{Action, CargoUI, CratesCompletionData, DependencyData, DependencyNode, Diag, Feature};
use anyhow::Context;
use cargo_metadata::{
    diagnostic::DiagnosticLevel, DependencyKind, Metadata, Node, PackageId, Version,
};
use futures::future::{Fuse, FusedFuture, FutureExt};
use itertools::Itertools;
use serde::Deserialize;
use sixtyfps::{ComponentHandle, Model, ModelHandle, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::str::FromStr;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

#[derive(Debug)]
pub struct FeatureSettings {
    enabled_features: Vec<SharedString>,
    enable_default_features: bool,
}

#[derive(Debug)]
pub enum CargoMessage {
    Quit,
    Action {
        action: Action,
        feature_settings: FeatureSettings,
    },
    ReloadManifest(SharedString),
    PackageSelected(SharedString),
    ShowOpenDialog,
    Cancel,
    /// Remove the dependency `.1` from package `.0`
    DependencyRemove {
        parent_package: SharedString,
        crate_name: SharedString,
        dep_kind: DependencyKind,
    },
    /// Upgrade the dependency `.1` in package `.0`
    DependencyUpgrade {
        parent_package: SharedString,
        crate_name: SharedString,
        dep_kind: DependencyKind,
    },
    DependencyAdd {
        crate_name: SharedString,
        dep_kind: DependencyKind,
    },
    Install(InstallJob),
    UpdateCompletion(SharedString),
}

pub struct CargoWorker {
    pub channel: UnboundedSender<CargoMessage>,
    worker_thread: std::thread::JoinHandle<()>,
}

impl CargoWorker {
    pub fn new(cargo_ui: &CargoUI) -> Self {
        let (channel, r) = tokio::sync::mpsc::unbounded_channel();
        let worker_thread = std::thread::spawn({
            let handle_weak = cargo_ui.as_weak();
            move || {
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(cargo_worker_loop(r, handle_weak))
                    .unwrap()
            }
        });
        Self {
            channel,
            worker_thread,
        }
    }

    pub fn join(self) -> std::thread::Result<()> {
        let _ = self.channel.send(CargoMessage::Quit);
        self.worker_thread.join()
    }
}

async fn cargo_worker_loop(
    mut r: UnboundedReceiver<CargoMessage>,
    handle: sixtyfps::Weak<CargoUI>,
) -> tokio::io::Result<()> {
    let mut manifest: Manifest = default_manifest().into();
    let mut metadata: Option<Metadata> = None;
    let mut crates_index: Option<crates_index::Index> = None;
    let mut package = SharedString::default();
    let mut update_features = true;
    let mut install_queue = VecDeque::new();
    let mut currently_installing = SharedString::default();

    let run_cargo_future = Fuse::terminated();
    let read_metadata_future = read_metadata(manifest.clone(), handle.clone()).fuse();
    let load_crate_index_future = load_crate_index().fuse();
    let install_completion_future = Fuse::terminated();
    let refresh_install_list_future = refresh_install_list(handle.clone()).fuse();
    let process_install_future = Fuse::terminated();
    futures::pin_mut!(
        run_cargo_future,
        read_metadata_future,
        load_crate_index_future,
        refresh_install_list_future,
        process_install_future,
        install_completion_future,
    );
    loop {
        let m = futures::select! {
            res = run_cargo_future => {
                res?;
                continue;
            }
            res = read_metadata_future => {
                metadata = res;
                if let Some(metadata) = &metadata {
                    apply_metadata(metadata, crates_index.as_ref(), update_features, &mut package, handle.clone());
                    update_features = false;
                }
                continue;
            }
            index = load_crate_index_future => {
                match index {
                    Ok(idx) => crates_index = Some(idx),
                    // TODO: ideally we should show that in the UI somehow
                    Err(error) => eprintln!("Error while fetching crate index: {}", error),
                };
                if let Some(metadata) = &metadata {
                    apply_metadata(metadata, crates_index.as_ref(), update_features, &mut package, handle.clone());
                    update_features = false;
                }
                if refresh_install_list_future.is_terminated() {
                    refresh_install_list_future.set(refresh_install_list(handle.clone()).fuse());
                }
                continue;
            }
            res = refresh_install_list_future =>  {
                apply_install_list(res?, crates_index.as_ref(), &install_queue, &currently_installing, handle.clone());
                continue;
            }
            res = process_install_future => {
                res?;
                refresh_install_list_future.set(refresh_install_list(handle.clone()).fuse());
                if let Some(job) = install_queue.pop_front() {
                    currently_installing = job.crate_name().clone();
                    process_install_future.set(process_install(job, handle.clone()).fuse());
                } else {
                    currently_installing = Default::default();
                }
                continue;
            }
            _ = install_completion_future => { continue; }
            m = r.recv().fuse() => {
                match m {
                    None => return Ok(()),
                    Some(m) => m,
                }
            }
        };

        match m {
            CargoMessage::Quit => return Ok(()),
            CargoMessage::Action {
                action,
                feature_settings,
            } => run_cargo_future
                .set(run_cargo(action, feature_settings, manifest.clone(), handle.clone()).fuse()),
            CargoMessage::Cancel => {
                run_cargo_future.set(Fuse::terminated());
            }
            CargoMessage::ReloadManifest(m) => {
                manifest = PathBuf::from(m.as_str()).into();
                update_features = true;
                read_metadata_future.set(read_metadata(manifest.clone(), handle.clone()).fuse());
            }
            CargoMessage::ShowOpenDialog => {
                manifest = show_open_dialog(manifest);
                update_features = true;
                read_metadata_future.set(read_metadata(manifest.clone(), handle.clone()).fuse());
            }
            CargoMessage::PackageSelected(pkg) => {
                package = pkg;
                if let Some(metadata) = &metadata {
                    apply_metadata(
                        metadata,
                        crates_index.as_ref(),
                        /*update_features*/ true,
                        &mut package,
                        handle.clone(),
                    );
                }
            }
            CargoMessage::DependencyRemove {
                parent_package,
                crate_name,
                dep_kind,
            } => {
                let pkg = PackageId {
                    repr: parent_package.into(),
                };
                if let Some(pkg) = metadata
                    .as_ref()
                    .and_then(|metadata| metadata.packages.iter().find(|p| p.id == pkg))
                {
                    match dependency_remove(
                        pkg.manifest_path.as_ref(),
                        crate_name.as_str(),
                        dep_kind,
                    ) {
                        Ok(()) => read_metadata_future
                            .set(read_metadata(manifest.clone(), handle.clone()).fuse()),
                        Err(e) => {
                            dbg!(e);
                            handle.clone().upgrade_in_event_loop(|h| {
                                h.set_status("Not yet supported".into());
                            });
                        }
                    }
                }
            }
            CargoMessage::DependencyAdd {
                crate_name,
                dep_kind,
            } => {
                if let Some((pkg, cr)) = metadata
                    .as_ref()
                    .and_then(|metadata| {
                        let pkg = package.as_str();
                        if pkg.is_empty() {
                            Some(&metadata[metadata.workspace_members.first()?])
                        } else {
                            metadata.packages.iter().find(|p| p.name == pkg)
                        }
                    })
                    .and_then(|p| Some((p, crates_index.as_ref()?.crate_(&crate_name)?)))
                {
                    match dependency_add(
                        pkg.manifest_path.as_ref(),
                        crate_name.as_str(),
                        cr.highest_stable_version()
                            .unwrap_or(cr.highest_version())
                            .version(),
                        dep_kind,
                    ) {
                        Ok(()) => read_metadata_future
                            .set(read_metadata(manifest.clone(), handle.clone()).fuse()),
                        Err(e) => {
                            handle.clone().upgrade_in_event_loop(move |h| {
                                h.set_status(format!("{}", e).into());
                            });
                        }
                    }
                }
            }
            CargoMessage::DependencyUpgrade {
                parent_package,
                crate_name,
                dep_kind,
            } => {
                let pkg = PackageId {
                    repr: parent_package.into(),
                };
                if let Some((pkg, cr)) = metadata
                    .as_ref()
                    .and_then(|metadata| metadata.packages.iter().find(|p| p.id == pkg))
                    .and_then(|p| Some((p, crates_index.as_ref()?.crate_(&crate_name)?)))
                {
                    match dependency_upgrade_to_version(
                        pkg.manifest_path.as_ref(),
                        crate_name.as_str(),
                        cr.highest_stable_version()
                            .unwrap_or(cr.highest_version())
                            .version(),
                        dep_kind,
                    ) {
                        Ok(()) => read_metadata_future
                            .set(read_metadata(manifest.clone(), handle.clone()).fuse()),
                        Err(e) => {
                            dbg!(e);
                            handle.clone().upgrade_in_event_loop(|h| {
                                h.set_status("Not yet supported".into());
                            });
                        }
                    }
                }
            }
            CargoMessage::Install(job) => {
                if process_install_future.is_terminated() {
                    currently_installing = job.crate_name().clone();
                    process_install_future.set(process_install(job, handle.clone()).fuse());
                } else {
                    install_queue.push_back(job);
                }
            }
            CargoMessage::UpdateCompletion(query) => {
                if let Some(idx) = crates_index.as_ref() {
                    install_completion_future.set(
                        install_completion(idx.path().to_owned(), query, handle.clone()).fuse(),
                    );
                }
            }
        }
    }
}

async fn load_crate_index() -> Result<crates_index::Index, String> {
    let mut index = crates_index::Index::new_cargo_default().map_err(|x| x.to_string())?;
    index.update().map_err(|x| x.to_string())?;
    Ok(index)
}

async fn run_cargo(
    action: Action,
    features: FeatureSettings,
    manifest: Manifest,
    handle: sixtyfps::Weak<CargoUI>,
) -> tokio::io::Result<()> {
    handle.clone().upgrade_in_event_loop(|h| {
        h.set_status("".into());
        h.set_is_building(true);
        let diagnostics_model = Rc::new(VecModel::<Diag>::default());
        h.set_diagnostics(ModelHandle::from(
            diagnostics_model.clone() as Rc<dyn Model<Data = Diag>>
        ));
    });

    struct ResetIsBuilding(sixtyfps::Weak<CargoUI>);
    impl Drop for ResetIsBuilding {
        fn drop(&mut self) {
            self.0
                .clone()
                .upgrade_in_event_loop(move |h| h.set_is_building(false))
        }
    }
    let _reset_is_building = ResetIsBuilding(handle.clone());

    let mut cargo_command = cargo_command();
    cargo_command.arg(action.command.as_str());
    cargo_command
        .arg("--manifest-path")
        .arg(manifest.path_to_cargo_toml());
    if action.profile == "release" {
        cargo_command.arg("--release");
    }
    if action.command == "run" && !action.extra.is_empty() {
        if let Some(example) = action.extra.strip_suffix(" (example)") {
            cargo_command.arg("--example").arg(example);
        } else {
            cargo_command.arg("--bin").arg(action.extra.as_str());
        }
    } else if action.command == "test" && !action.extra.is_empty() {
        cargo_command.arg("--test").arg(action.extra.as_str());
    }
    if !action.package.is_empty() {
        cargo_command.arg("-p").arg(action.package.as_str());
    }
    features.to_args(&mut cargo_command);
    cargo_command.args(&["--message-format", "json"]);

    if !action.arguments.is_empty() {
        cargo_command.arg("--");
        if let Some(args) = shlex::split(&action.arguments) {
            cargo_command.args(args);
        } else {
            handle.clone().upgrade_in_event_loop(move |h| {
                h.set_status("Error parsing command line arguments".into());
                h.set_build_pane_visible(false);
            });
            return Ok(());
        }
    }

    let mut res = cargo_command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let mut stdout = BufReader::new(res.stdout.take().unwrap()).lines();
    let mut stderr = BufReader::new(res.stderr.take().unwrap()).lines();
    loop {
        tokio::select! {
            line = stderr.next_line() => {
                let line = if let Some(line) = line? { line } else { break };
                handle.clone().upgrade_in_event_loop(move |h| {
                    h.set_status(line.into());
                });
            }
            line = stdout.next_line() => {
                let line = if let Some(line) = line? { line } else { break };
                let mut deserializer = serde_json::Deserializer::from_str(&line);
                deserializer.disable_recursion_limit();
                let msg = cargo_metadata::Message::deserialize(&mut deserializer).unwrap_or(cargo_metadata::Message::TextLine(line));

                if let Some(diag) = cargo_message_to_diag(msg) {
                    handle.clone().upgrade_in_event_loop(move |h|{
                        let model_handle = h.get_diagnostics();
                        let model = model_handle.as_any().downcast_ref::<VecModel<Diag>>().unwrap();
                        model.push(diag);
                    });
                }
            }
        }
    }

    handle.upgrade_in_event_loop(move |h| {
        h.set_status("Finished".into());
        let model_handle = h.get_diagnostics();
        let model = model_handle
            .as_any()
            .downcast_ref::<VecModel<Diag>>()
            .unwrap();

        if model.row_count() == 0 {
            h.set_build_pane_visible(false);
        }

        let error_count = model
            .iter()
            .filter(|diagnostic| diagnostic.level == 1)
            .count();
        let warning_count = model
            .iter()
            .filter(|diagnostic| diagnostic.level == 2)
            .count();

        let result = if error_count == 0 && warning_count == 0 {
            "✅".into()
        } else {
            format!("{} errors; {} warnings", error_count, warning_count).into()
        };

        match action.command.as_str() {
            "build" => h.set_build_results(result),
            "check" => h.set_check_results(result),
            _ => {}
        }
    });

    Ok(())
}

pub fn cargo_command() -> tokio::process::Command {
    let cargo_path = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    tokio::process::Command::new(cargo_path)
}

fn cargo_message_to_diag(msg: cargo_metadata::Message) -> Option<Diag> {
    match msg {
        cargo_metadata::Message::CompilerMessage(msg) => {
            let diag = Diag {
                short: msg.message.message.into(),
                expanded: msg.message.rendered.unwrap_or_default().into(),
                level: match msg.message.level {
                    DiagnosticLevel::Error => 1,
                    DiagnosticLevel::Warning => 2,
                    DiagnosticLevel::FailureNote => 3,
                    DiagnosticLevel::Note => 3,
                    DiagnosticLevel::Help => 3,
                    _ => 0,
                },
            };
            Some(diag)
        }
        cargo_metadata::Message::TextLine(line) => {
            let diag = Diag {
                short: line.into(),
                expanded: Default::default(),
                level: 0,
            };
            Some(diag)
        }
        _ => None,
    }
}

fn default_manifest() -> PathBuf {
    // skip the "ui" arg in case we are invoked with `cargo ui`
    dunce::canonicalize(
        match std::env::args()
            .skip(1)
            .find(|a| a != "ui" && !a.starts_with('-'))
        {
            Some(p) => p.into(),
            None => std::env::current_dir().unwrap_or_default(),
        },
    )
    .unwrap_or_default()
}

async fn read_metadata(manifest: Manifest, handle: sixtyfps::Weak<CargoUI>) -> Option<Metadata> {
    let manifest_str = manifest
        .path_to_cargo_toml()
        .to_string_lossy()
        .as_ref()
        .into();
    handle.clone().upgrade_in_event_loop(move |h| {
        h.set_workspace_valid(false);
        h.set_manifest_path(manifest_str);
        h.set_status("Loading metadata from Cargo.toml...".into());
    });

    let mut cmd = cargo_metadata::MetadataCommand::new();
    cmd.manifest_path(manifest.path_to_cargo_toml());
    match cmd.exec() {
        Ok(metadata) => {
            handle.upgrade_in_event_loop(move |h| {
                h.set_status("Cargo.toml loaded".into());
            });
            Some(metadata)
        }
        Err(e) => {
            handle.upgrade_in_event_loop(move |h| {
                h.set_status(format!("{}", e).into());
            });
            None
        }
    }
}

fn apply_metadata(
    metadata: &Metadata,
    crates_index: Option<&crates_index::Index>,
    mut update_features: bool,
    package: &mut SharedString,
    handle: sixtyfps::Weak<CargoUI>,
) {
    let mut packages = vec![SharedString::default()]; // keep one empty row
    let mut run_target = Vec::new();
    let mut test_target = Vec::new();
    let mut features: Option<Vec<Feature>> = None;
    if !package.is_empty()
        && !metadata
            .packages
            .iter()
            .filter(|p| metadata.workspace_members.contains(&p.id))
            .any(|p| package == p.name.as_str())
    {
        // if the selected package don't exist in the manifest, deselect it
        *package = SharedString::default();
        update_features = true;
    };

    let is_workspace = metadata.workspace_members.len() > 1;

    for p in metadata
        .packages
        .iter()
        .filter(|p| metadata.workspace_members.contains(&p.id))
    {
        packages.push(p.name.as_str().into());

        let is_selected = !is_workspace || package.is_empty() || package == p.name.as_str();

        if update_features {
            let default_features: HashSet<_> = p
                .features
                .get("default")
                .map(|default_features| default_features.iter().cloned().collect())
                .unwrap_or_default();

            if is_selected {
                // Use get_or_insert_with_default() when https://github.com/rust-lang/rust/issues/82901 is stable
                features.get_or_insert_with(|| Default::default()).extend(
                    p.features
                        .keys()
                        .filter(|name| *name != "default")
                        .map(|name| Feature {
                            name: if package.is_empty() && is_workspace {
                                [p.name.as_str(), name.as_str()].join("/").into()
                            } else {
                                name.into()
                            },
                            enabled: false,
                            enabled_by_default: default_features.contains(name),
                        }),
                );
            }
        }

        if !is_selected {
            continue;
        }
        for t in &p.targets {
            if t.kind.iter().any(|x| x == "bin") {
                run_target.push(SharedString::from(t.name.as_str()));
            } else if t.kind.iter().any(|x| x == "example") {
                run_target.push(SharedString::from(format!("{} (example)", t.name).as_str()));
            } else if t.kind.iter().any(|x| x == "test") {
                test_target.push(SharedString::from(t.name.as_str()));
            }
        }
    }
    let pkg = package.clone();
    handle.clone().upgrade_in_event_loop(move |h| {
        h.global::<DependencyData>()
            .set_package_selected(!is_workspace || !pkg.is_empty());
        h.set_current_package(pkg);
        // The model always has at least two entries, one for all and the first package,
        // so enable multi-package selection only if there is something else to select.
        h.set_allow_package_selection(is_workspace);
        h.set_packages(ModelHandle::from(
            Rc::new(VecModel::from(packages)) as Rc<dyn Model<Data = SharedString>>
        ));
        h.set_extra_run(ModelHandle::from(
            Rc::new(VecModel::from(run_target)) as Rc<dyn Model<Data = SharedString>>
        ));
        h.set_has_extra_tests(!test_target.is_empty());
        h.set_extra_test(ModelHandle::from(
            Rc::new(VecModel::from(test_target)) as Rc<dyn Model<Data = SharedString>>
        ));
        if let Some(features) = features {
            h.set_has_features(!features.is_empty());
            h.set_enable_default_features(true);
            h.set_package_features(ModelHandle::from(
                Rc::new(VecModel::from(features)) as Rc<dyn Model<Data = Feature>>
            ));
        }
        h.set_workspace_valid(true);
    });

    let mut depgraph_tree = Vec::new();
    if let Some(resolve) = &metadata.resolve {
        let mut duplicates = HashSet::new();
        let map: HashMap<_, _> = resolve.nodes.iter().map(|n| (n.id.clone(), n)).collect();
        for m in &metadata.workspace_members {
            if !package.is_empty() && package != metadata[m].name.as_str() {
                continue;
            }
            build_dep_tree(
                m,
                None,
                &SharedString::default(),
                &mut depgraph_tree,
                &mut duplicates,
                &metadata,
                crates_index,
                &map,
                0,
            );
        }
    }

    handle.upgrade_in_event_loop(move |h| {
        let model = Rc::new(DepGraphModel::from(depgraph_tree));
        h.global::<DependencyData>()
            .set_model(ModelHandle::new(model))
    });
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn show_open_dialog(manifest: Manifest) -> Manifest {
    use dialog::DialogBox;

    let mut dialog = dialog::FileSelection::new("Select a manifest (Cargo.toml)");
    dialog
        .title("Select a manifest")
        .mode(dialog::FileSelectionMode::Open);

    if let Some(directory) = manifest.directory() {
        dialog.path(directory);
    }

    match dialog.show() {
        Ok(Some(r)) => PathBuf::from(r).into(),
        Ok(None) => manifest,
        Err(e) => {
            eprintln!("{}", e);
            manifest
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn show_open_dialog(manifest: Manifest) -> Manifest {
    let mut dialog = rfd::FileDialog::new();
    dialog = dialog.set_title("Select a manifest");

    if let Some(directory) = manifest.directory() {
        dialog = dialog.set_directory(directory);
    }

    match dialog.pick_file() {
        Some(new_path) => new_path.into(),
        None => manifest,
    }
}

struct TreeNode {
    node: RefCell<DependencyNode>,
    children: Vec<TreeNode>,
}

fn build_dep_tree(
    package_id: &PackageId,
    node_dep: Option<&cargo_metadata::NodeDep>,
    parent_package: &SharedString,
    depgraph_tree: &mut Vec<TreeNode>,
    duplicates: &mut HashSet<PackageId>,
    metadata: &Metadata,
    crates_index: Option<&crates_index::Index>,
    map: &HashMap<PackageId, &Node>,
    indentation: i32,
) {
    let package = &metadata[package_id];
    let duplicated = duplicates.contains(package_id);
    let outdated = crates_index
        .and_then(|idx| idx.crate_(&package.name))
        .and_then(|c| c.highest_stable_version().cloned())
        .and_then(|v| Version::from_str(v.version()).ok())
        .map_or(false, |latest| latest > package.version);
    let dep_kind = node_dep
        .filter(|n| {
            !n.dep_kinds
                .iter()
                .all(|c| c.kind == cargo_metadata::DependencyKind::Normal)
        })
        .map(|n| {
            n.dep_kinds
                .iter()
                .map(|c| c.kind.to_string())
                .join(" ")
                .into()
        })
        .unwrap_or_default();
    let mut node = TreeNode {
        node: DependencyNode {
            has_children: false,
            indentation,
            open: indentation != 1,
            version: package.version.to_string().into(),
            crate_name: package.name.as_str().into(),
            outdated,
            duplicated,
            dep_kind,
            parent_package: parent_package.clone(),
        }
        .into(),
        children: Default::default(),
    };

    if !duplicates.contains(package_id) {
        duplicates.insert(package_id.clone());

        for d in &map[package_id].deps {
            build_dep_tree(
                &d.pkg,
                Some(&d),
                &package_id.repr.as_str().into(),
                &mut node.children,
                duplicates,
                metadata,
                crates_index,
                map,
                indentation + 1,
            );
        }
    }

    if !node.children.is_empty() {
        node.node.borrow_mut().has_children = true;
    }
    depgraph_tree.push(node);
}

struct DepGraphModel {
    /// path to the location in the tree
    cache: RefCell<Vec<Vec<usize>>>,
    tree: Vec<TreeNode>,
    notify: sixtyfps::ModelNotify,
}

impl From<Vec<TreeNode>> for DepGraphModel {
    fn from(tree: Vec<TreeNode>) -> Self {
        let self_ = Self {
            cache: Default::default(),
            tree,
            notify: Default::default(),
        };
        self_.relayout();
        self_
    }
}

impl DepGraphModel {
    fn get_node(&self, path: &[usize]) -> &TreeNode {
        let mut path_iter = path.iter();
        let mut node = &self.tree[*path_iter.next().unwrap()];
        for x in path_iter {
            node = &node.children[*x];
        }
        node
    }

    fn flatten_tree(path: &mut Vec<usize>, cache: &mut Vec<Vec<usize>>, nodes: &[TreeNode]) {
        for (i, n) in nodes.iter().enumerate() {
            path.push(i);
            cache.push(path.clone());
            if n.node.borrow().open {
                Self::flatten_tree(path, cache, &n.children);
            }
            let _x = path.pop();
            debug_assert_eq!(_x, Some(i));
        }
    }

    fn relayout(&self) {
        let mut cache = self.cache.borrow_mut();
        self.notify.row_removed(0, cache.len());
        let mut path = vec![];
        cache.clear();
        Self::flatten_tree(&mut path, &mut cache, &self.tree);
        self.notify.row_added(0, cache.len());
    }
}

impl sixtyfps::Model for DepGraphModel {
    type Data = DependencyNode;

    fn row_count(&self) -> usize {
        self.cache.borrow().len()
    }

    fn row_data(&self, row: usize) -> Self::Data {
        let node = self.get_node(&self.cache.borrow()[row]);
        node.node.borrow().clone()
    }

    fn attach_peer(&self, peer: sixtyfps::ModelPeer) {
        self.notify.attach(peer);
    }

    fn set_row_data(&self, row: usize, data: Self::Data) {
        self.get_node(&self.cache.borrow()[row]).node.replace(data);
        self.relayout();
    }
}

#[derive(Debug, Clone)]
struct Manifest(PathBuf);

impl From<PathBuf> for Manifest {
    fn from(mut directory_or_file: PathBuf) -> Self {
        if directory_or_file.is_dir() {
            directory_or_file.push("Cargo.toml");
        }
        Self(directory_or_file)
    }
}

impl Manifest {
    fn directory(&self) -> Option<&Path> {
        self.0.parent().filter(|path| path.is_dir())
    }

    fn path_to_cargo_toml(&self) -> &Path {
        &self.0
    }
}

impl FeatureSettings {
    pub fn new(ui: &CargoUI) -> Self {
        let enable_default_features = ui.get_enable_default_features();
        let enabled_features = ui
            .get_package_features()
            .iter()
            .filter_map(|feature| {
                if (feature.enabled && !feature.enabled_by_default && enable_default_features)
                    || (feature.enabled && !enable_default_features)
                {
                    Some(feature.name.clone())
                } else {
                    None
                }
            })
            .collect();
        Self {
            enabled_features,
            enable_default_features,
        }
    }

    fn to_args(&self, process: &mut tokio::process::Command) {
        if !self.enable_default_features {
            process.arg("--no-default-features");
        }
        if !self.enabled_features.is_empty() {
            process
                .arg("--features")
                .arg(self.enabled_features.iter().join(","));
        }
    }
}

fn to_table_name(dep_kind: DependencyKind) -> &'static str {
    match dep_kind {
        DependencyKind::Development => "dev-dependencies",
        DependencyKind::Build => "build-dependencies",
        _ => "dependencies",
    }
}

fn dependency_remove(pkg: &Path, dependency: &str, dep_kind: DependencyKind) -> anyhow::Result<()> {
    let manifest_contents = std::fs::read_to_string(pkg)
        .with_context(|| format!("Failed to load '{}'", pkg.display()))?;
    let mut document: toml_edit::Document = manifest_contents.parse()?;
    let table_name = to_table_name(dep_kind);
    let dependencies = &mut document[table_name];
    let removed = !std::mem::take(&mut dependencies[dependency]).is_none();
    if !removed {
        anyhow::bail!("'{}' was not in [{}]", dependency, dep_kind);
    }
    std::fs::write(pkg, document.to_string().as_bytes())
        .with_context(|| format!("Failed to write '{}'", pkg.display()))
}

fn dependency_upgrade_to_version(
    pkg: &Path,
    dependency: &str,
    version: &str,
    dep_kind: DependencyKind,
) -> anyhow::Result<()> {
    let manifest_contents = std::fs::read_to_string(pkg)
        .with_context(|| format!("Failed to load '{}'", pkg.display()))?;
    let mut document: toml_edit::Document = manifest_contents.parse()?;
    let table_name = to_table_name(dep_kind);
    let dep = &mut document[table_name][dependency];
    if dep.is_none() {
        anyhow::bail!("'{}' was not in [{}]", dependency, dep_kind);
    }
    if dep.is_str() {
        *dep = toml_edit::Item::Value(version.into());
    } else if dep.is_table_like() {
        dep["version"] = toml_edit::Item::Value(version.into());
    } else {
        anyhow::bail!("Could not understand the manifest");
    }
    std::fs::write(pkg, document.to_string().as_bytes())
        .with_context(|| format!("Failed to write '{}'", pkg.display()))
}

fn dependency_add(
    pkg: &Path,
    dependency: &str,
    version: &str,
    dep_kind: DependencyKind,
) -> anyhow::Result<()> {
    let manifest_contents = std::fs::read_to_string(pkg)
        .with_context(|| format!("Failed to load '{}'", pkg.display()))?;
    let mut document: toml_edit::Document = manifest_contents.parse()?;
    let table_name = to_table_name(dep_kind);
    let tbl = &mut document[table_name].or_insert(toml_edit::table());
    if !tbl.is_table_like() {
        anyhow::bail!("[{}] not a table", table_name);
    }
    let dep = &mut tbl[dependency];
    if !dep.is_none() {
        anyhow::bail!("{} is already a dependency", dependency);
    }
    *dep = toml_edit::Item::Value(version.into());

    std::fs::write(pkg, document.to_string().as_bytes())
        .with_context(|| format!("Failed to write '{}'", pkg.display()))
}

use crate::install::*;

async fn install_completion(
    mut idx_path: PathBuf,
    query: SharedString,
    handle: sixtyfps::Weak<CargoUI>,
) {
    let mut result = Vec::<SharedString>::new();
    if query.len() > 3 && query.is_ascii() {
        // `crates_index` does not allow to make search in a reasonable time, so I had to implement that myself
        // This only handle crates that have 4 or more characters
        idx_path.push(&query[0..2]);
        idx_path.push(&query[2..4]);
        if let Ok(mut rd) = tokio::fs::read_dir(idx_path).await {
            while let Ok(Some(entry)) = rd.next_entry().await {
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with(query.as_str()) {
                        result.push(name.into());
                        if result.len() > 50 {
                            // no need to put too many crate in the search result
                            break;
                        }
                    }
                }
            }
        }
    }
    handle.upgrade_in_event_loop(move |ui| {
        ui.global::<CratesCompletionData>()
            .set_completion_result(result.len() as i32);
        ui.global::<CratesCompletionData>()
            .set_completions(ModelHandle::from(
                Rc::new(VecModel::from(result)) as Rc<dyn Model<Data = SharedString>>
            ));
    });
}
