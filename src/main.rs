use serde::Deserialize;
use sixtyfps::{Model, ModelHandle, SharedString, VecModel};
use std::future::Future;
use std::path::Path;
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
    handle.on_reload_manifest(move |m| send.send(Message::ReloadManifest(m)).unwrap());
    handle.run();
    let _ = s.send(Message::Quit);
    worker_thread.join().unwrap();
}

async fn worker_loop(mut r: UnboundedReceiver<Message>, handle: sixtyfps::Weak<CargoUI>) {
    let mut manifest = default_manifest();
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
                manifest = m;
                run_cargo_future = Some(Box::pin(read_metadata(manifest.clone(), handle.clone())));
            }
        }
    }
}

async fn run_cargo(
    action: Action,
    manifest: SharedString,
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

    // FIXME! we want to do that asynchronously
    // Also, we should not just launch cargo.
    let mut cargo_command = tokio::process::Command::new("cargo");
    cargo_command.arg(action.command.as_str());
    cargo_command.arg("--manifest-path").arg(manifest.as_str());
    if action.profile == "release" {
        cargo_command.arg("--release");
    }
    if action.command == "run" && !action.extra.is_empty() {
        cargo_command.arg("--bin").arg(action.extra.as_str());
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
                        h.set_status(line.as_str().into());
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
        }
    });

    Ok(())
}

fn cargo_message_to_diag(msg: cargo_metadata::Message) -> Option<Diag> {
    match msg {
        cargo_metadata::Message::CompilerMessage(msg) => {
            let message = msg
                .message
                .rendered
                .unwrap_or(msg.message.message)
                .as_str()
                .into();
            let diag = Diag { message };
            Some(diag)
        }
        _ => None,
    }
}

fn default_manifest() -> SharedString {
    match std::env::args().skip(1).next() {
        Some(p) => p.as_str().into(),
        None => {
            let path = Path::new("Cargo.toml");
            if path.exists() {
                path.canonicalize()
                    .map(|p| p.display().to_string().as_str().into())
                    .unwrap_or_default()
            } else {
                SharedString::default()
            }
        }
    }
}

async fn read_metadata(
    manifest: SharedString,
    handle: sixtyfps::Weak<CargoUI>,
) -> tokio::io::Result<()> {
    let h = handle.clone();
    let manifest_clone = manifest.clone();
    sixtyfps::invoke_from_event_loop(move || {
        if let Some(h) = h.upgrade() {
            h.set_workspace_valid(false);
            h.set_manifest_path(manifest_clone);
        }
    });

    let mut cmd = cargo_metadata::MetadataCommand::new();
    cmd.manifest_path(manifest.as_str());
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
    sixtyfps::invoke_from_event_loop(move || {
        if let Some(h) = handle.upgrade() {
            h.set_packages(ModelHandle::from(
                Rc::new(VecModel::from(packages)) as Rc<dyn Model<Data = SharedString>>
            ));
            h.set_extra_run(ModelHandle::from(
                Rc::new(VecModel::from(run_target)) as Rc<dyn Model<Data = SharedString>>
            ));
            h.set_extra_test(ModelHandle::from(
                Rc::new(VecModel::from(test_target)) as Rc<dyn Model<Data = SharedString>>
            ));
            h.set_status("Cargo.toml loaded".into());
            h.set_workspace_valid(true);
        }
    });
    Ok(())
}
