use sixtyfps::{Model, ModelHandle, SharedString, VecModel};
use std::rc::Rc;
use tokio::sync::mpsc::UnboundedReceiver;

sixtyfps::include_modules!();

#[derive(Debug)]
enum Message {
    Quit,
    Action {
        cmd: SharedString,
        package: SharedString,
        mode: SharedString,
    },
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
    handle.run();
    let _ = s.send(Message::Quit);
    metadata_thread.join().unwrap();
    worker_thread.join().unwrap();
}

async fn worker_loop(mut r: UnboundedReceiver<Message>, handle: sixtyfps::Weak<CargoUI>) {
    while let Some(m) = r.recv().await {
        match m {
            Message::Quit => return,
            Message::Action { cmd, package, mode } => {
                run_cargo(cmd, package, mode, handle.clone());
            }
        }
    }
}

fn run_cargo(
    cmd: SharedString,
    _package: SharedString,
    mode: SharedString,
    handle: sixtyfps::Weak<CargoUI>,
) {
    // FIXME! we want to do that asynchronously
    // Also, we should not just launch cargo.
    let mut cargo_command = std::process::Command::new("cargo");
    cargo_command.arg(cmd.as_str());
    if mode == "release" {
        cargo_command.arg("--release");
    }
    let res = cargo_command
        .args(&["--message-format", "json"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap()
        .wait_with_output(Erro)
        .unwrap();

    let message = SharedString::from(format!(
        "{}\n{}",
        std::str::from_utf8(&res.stdout).unwrap(),
        std::str::from_utf8(&res.stderr).unwrap()
    ));

    sixtyfps::invoke_from_event_loop(move || {
        if let Some(h) = handle.upgrade() {
            h.set_status(message)
        }
    });
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
