/* Copyright © 2021 SixtyFPS GmbH <info@sixtyfps.info>
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */

use cargo_metadata::{diagnostic::DiagnosticLevel, Metadata, Node, PackageId};

use serde::Deserialize;
use sixtyfps::{Model, ModelHandle, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::UnboundedReceiver;

sixtyfps::include_modules!();

#[derive(Debug)]
enum Message {
    Quit,
    Action(Action),
    ReloadManifest(SharedString),
    ShowOpenDialog,
    Cancel,
}

fn main() {
    let handle = CargoUI::new();
    let (s, r) = tokio::sync::mpsc::unbounded_channel();
    let handle_weak = handle.as_weak();
    let worker_thread = std::thread::spawn(move || {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(worker_loop(r, handle_weak))
    });
    handle.set_workspace_valid(false);
    let send = s.clone();
    handle.on_action(move |action| send.send(Message::Action(action)).unwrap());
    let send = s.clone();
    handle.on_cancel(move || send.send(Message::Cancel).unwrap());
    let send = s.clone();
    handle.on_show_open_dialog(move || send.send(Message::ShowOpenDialog).unwrap());
    let send = s.clone();
    handle.on_reload_manifest(move |m| send.send(Message::ReloadManifest(m)).unwrap());
    handle.run();
    let _ = s.send(Message::Quit);
    worker_thread.join().unwrap();
}

async fn worker_loop(mut r: UnboundedReceiver<Message>, handle: sixtyfps::Weak<CargoUI>) {
    let mut manifest: Manifest = default_manifest().into();
    let mut run_cargo_future: Option<Pin<Box<dyn Future<Output = tokio::io::Result<()>>>>> =
        Some(Box::pin(read_metadata(manifest.clone(), handle.clone())));
    loop {
        let m = if let Some(fut) = &mut run_cargo_future {
            tokio::select! {
                m = r.recv() => {
                    m
                }
                res = fut => {
                    res.unwrap();
                    run_cargo_future = None;
                    r.recv().await
                }
            }
        } else {
            r.recv().await
        };

        match m {
            None => return,
            Some(Message::Quit) => return,
            Some(Message::Action(action)) => {
                run_cargo_future = Some(Box::pin(run_cargo(
                    action,
                    manifest.clone(),
                    handle.clone(),
                )));
            }
            Some(Message::Cancel) => {
                run_cargo_future = None;
            }
            Some(Message::ReloadManifest(m)) => {
                manifest = PathBuf::from(m.as_str()).into();
                run_cargo_future = Some(Box::pin(read_metadata(manifest.clone(), handle.clone())));
            }
            Some(Message::ShowOpenDialog) => {
                manifest = show_open_dialog(manifest);
                run_cargo_future = Some(Box::pin(read_metadata(manifest.clone(), handle.clone())));
            }
        }
    }
}

async fn run_cargo(
    action: Action,
    manifest: Manifest,
    handle: sixtyfps::Weak<CargoUI>,
) -> tokio::io::Result<()> {
    // FIXME: Would be nice if we did not need a thread_local
    thread_local! {static DIAG_MODEL: std::cell::RefCell<Rc<VecModel<Diag>>> = Default::default()}

    let h = handle.clone();
    sixtyfps::invoke_from_event_loop(move || {
        if let Some(h) = h.upgrade() {
            h.set_status("".into());
            h.set_is_building(true);
            let error_model = Rc::new(VecModel::<Diag>::default());
            h.set_errors(ModelHandle::from(
                error_model.clone() as Rc<dyn Model<Data = Diag>>
            ));
            DIAG_MODEL.with(|tl| tl.replace(error_model));
        }
    });

    struct ResetIsBuilding(sixtyfps::Weak<CargoUI>);
    impl Drop for ResetIsBuilding {
        fn drop(&mut self) {
            let h = self.0.clone();
            sixtyfps::invoke_from_event_loop(move || {
                if let Some(h) = h.upgrade() {
                    h.set_is_building(false)
                }
            })
        }
    }
    let _reset_is_building = ResetIsBuilding(handle.clone());

    let cargo_path = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cargo_command = tokio::process::Command::new(cargo_path);
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
    }
    if !action.package.is_empty() {
        cargo_command.arg("-p").arg(action.package.as_str());
    }
    let mut res = cargo_command
        .args(&["--message-format", "json"])
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
                let h = handle.clone();
                sixtyfps::invoke_from_event_loop(move || {
                    if let Some(h) = h.upgrade() {
                        h.set_status(line.into());
                    }
                });
            }
            line = stdout.next_line() => {
                let line = if let Some(line) = line? { line } else { break };
                let mut deserializer = serde_json::Deserializer::from_str(&line);
                deserializer.disable_recursion_limit();
                let msg = cargo_metadata::Message::deserialize(&mut deserializer).unwrap_or(cargo_metadata::Message::TextLine(line));

                if let Some(diag) = cargo_message_to_diag(msg){
                    sixtyfps::invoke_from_event_loop(move || {
                        DIAG_MODEL.with(|model| {
                            model.borrow().push(diag)
                        });
                    });
                }
            }
        }
    }

    sixtyfps::invoke_from_event_loop(move || {
        if let Some(h) = handle.upgrade() {
            h.set_status("Finished".into());
            if DIAG_MODEL.with(|model| model.borrow().row_count()) == 0 {
                h.set_build_pane_visible(false);
            }
        }
    });

    Ok(())
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
            .skip_while(|a| a == "ui" || a.starts_with('-'))
            .next()
        {
            Some(p) => p.into(),
            None => std::env::current_dir().unwrap_or_default(),
        },
    )
    .unwrap_or_default()
}

async fn read_metadata(
    manifest: Manifest,
    handle: sixtyfps::Weak<CargoUI>,
) -> tokio::io::Result<()> {
    let h = handle.clone();
    let manifest_str = manifest
        .path_to_cargo_toml()
        .to_string_lossy()
        .as_ref()
        .into();
    sixtyfps::invoke_from_event_loop(move || {
        if let Some(h) = h.upgrade() {
            h.set_workspace_valid(false);
            h.set_manifest_path(manifest_str);
            h.set_status("Loading metadata from Cargo.toml...".into());
        }
    });

    let mut cmd = cargo_metadata::MetadataCommand::new();
    cmd.manifest_path(manifest.path_to_cargo_toml());
    let metadata = match cmd.exec() {
        Ok(metadata) => metadata,
        Err(e) => {
            sixtyfps::invoke_from_event_loop(move || {
                if let Some(h) = handle.upgrade() {
                    h.set_status(format!("{}", e).into());
                }
            });
            return Ok(());
        }
    };
    let mut packages = vec![SharedString::default()]; // keep one empty row
    let mut run_target = Vec::new();
    let mut test_target = Vec::new();
    for p in metadata
        .packages
        .iter()
        .filter(|p| metadata.workspace_members.contains(&p.id))
    {
        packages.push(p.name.as_str().into());
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
    let h = handle.clone();
    sixtyfps::invoke_from_event_loop(move || {
        if let Some(h) = h.upgrade() {
            h.set_current_package(SharedString::default());
            h.set_packages_count(packages.len() as i32);
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
            h.set_status("Cargo.toml loaded".into());
            h.set_workspace_valid(true);
        }
    });

    let mut depgraph_tree = Vec::new();
    if let Some(resolve) = &metadata.resolve {
        let mut duplicates = HashSet::new();
        let map: HashMap<_, _> = resolve.nodes.iter().map(|n| (n.id.clone(), n)).collect();
        for m in &metadata.workspace_members {
            build_dep_tree(m, &mut depgraph_tree, &mut duplicates, &metadata, &map, 0);
        }
    }

    let h = handle.clone();
    sixtyfps::invoke_from_event_loop(move || {
        if let Some(h) = h.upgrade() {
            let model = Rc::new(DepGraphModel::from(depgraph_tree));
            h.set_deptree(ModelHandle::new(model))
        }
    });
    Ok(())
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
    depgraph_tree: &mut Vec<TreeNode>,
    duplicates: &mut HashSet<PackageId>,
    metadata: &Metadata,
    map: &HashMap<PackageId, &Node>,
    indentation: i32,
) {
    let package = &metadata[package_id];
    let mut text = package.name.as_str().into();
    if duplicates.contains(package_id) {
        text += " (duplicated)";
    }
    let mut node = TreeNode {
        node: DependencyNode {
            has_children: false,
            indentation,
            open: true,
            text,
        }
        .into(),
        children: Default::default(),
    };

    if !duplicates.contains(package_id) {
        duplicates.insert(package_id.clone());

        for d in &map[package_id].dependencies {
            build_dep_tree(
                d,
                &mut node.children,
                duplicates,
                metadata,
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
        while let Some(x) = path_iter.next() {
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
