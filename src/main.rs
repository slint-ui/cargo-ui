use sixtyfps::{Model, ModelHandle, SharedString, VecModel};
use std::rc::Rc;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::mpsc::UnboundedReceiver,
};

sixtyfps::include_modules!();

#[derive(Debug)]
enum Message {
    Quit,
    Action {
        cmd: SharedString,
        package: SharedString,
        mode: SharedString,
    },
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
    let handle_weak = handle.as_weak();
    let metadata_thread = std::thread::spawn(move || {
        read_metadata(handle_weak);
    });
    let send = s.clone();
    handle.on_action(move |cmd, target, mode| {
        send.send(Message::Action {
            cmd,
            package: target,
            mode,
        })
        .unwrap()
    });
    let send = s.clone();
    handle.on_cancel(move || send.send(Message::Cancel).unwrap());
    handle.run();
    let _ = s.send(Message::Quit);
    metadata_thread.join().unwrap();
    worker_thread.join().unwrap();
}

async fn worker_loop(mut r: UnboundedReceiver<Message>, handle: sixtyfps::Weak<CargoUI>) {
    let mut run_cargo_future = None;
    if false {
        // Just to help type inference of run_cargo_future
        run_cargo_future = Some(Box::pin(run_cargo(
            Default::default(),
            Default::default(),
            Default::default(),
            handle.clone(),
        )));
    }
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
            Some(Message::Action { cmd, package, mode }) => {
                run_cargo_future = Some(Box::pin(run_cargo(cmd, package, mode, handle.clone())));
            }
            Some(Message::Cancel) => {
                run_cargo_future = None;
            }
        }
    }
}

async fn run_cargo(
    cmd: SharedString,
    _package: SharedString,
    mode: SharedString,
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
    cargo_command.arg(cmd.as_str());
    if mode == "release" {
        cargo_command.arg("--release");
    }
    let mut res = cargo_command
        .args(&["--message-format", "json"])
        //.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    //let mut stdout = BufReader::new(res.stdout.take().unwrap()).lines();
    let mut stderr = BufReader::new(res.stderr.take().unwrap()).lines();
    while let Some(line) = stderr.next_line().await? {
        let line = SharedString::from(line.as_str());
        println!("Line: {}", line);

        let h = handle.clone();
        sixtyfps::invoke_from_event_loop(move || {
            if let Some(h) = h.upgrade() {
                DIAG_MODEL.with(|model| {
                    model.borrow().push(Diag {
                        message: line.clone(),
                    })
                });
                h.set_status(line);
            }
        });
    }

    sixtyfps::invoke_from_event_loop(move || {
        if let Some(h) = handle.upgrade() {
            h.set_status("Finished".into());
        }
    });

    Ok(())
}

fn read_metadata(handle: sixtyfps::Weak<CargoUI>) {
    let mut cmd = cargo_metadata::MetadataCommand::new();
    let mut args = std::env::args().skip_while(|val| !val.starts_with("--manifest-path"));
    match args.next() {
        Some(ref p) if p == "--manifest-path" => {
            cmd.manifest_path(args.next().unwrap());
        }
        Some(p) => {
            cmd.manifest_path(p.trim_start_matches("--manifest-path="));
        }
        None => {}
    };
    let metadata = cmd.exec().unwrap();
    let targets = metadata
        .packages
        .iter()
        .map(|p| p.name.as_str().into())
        .collect::<Vec<SharedString>>();

    sixtyfps::invoke_from_event_loop(move || {
        if let Some(h) = handle.upgrade() {
            h.set_packages(ModelHandle::from(
                Rc::new(VecModel::from(targets)) as Rc<dyn Model<Data = SharedString>>
            ))
        }
    });
}
