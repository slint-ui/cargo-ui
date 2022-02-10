/* Copyright © SixtyFPS GmbH <info@slint-ui.com>
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */

use std::rc::Rc;

use super::{CargoUI, Toolchain};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

#[derive(Debug)]
pub enum RustupMessage {
    Quit,
}

pub struct RustupWorker {
    pub channel: UnboundedSender<RustupMessage>,
    worker_thread: std::thread::JoinHandle<()>,
}

impl RustupWorker {
    pub fn new(cargo_ui: &CargoUI) -> Self {
        let (channel, r) = tokio::sync::mpsc::unbounded_channel();
        let worker_thread = std::thread::spawn({
            let handle_weak = cargo_ui.as_weak();
            move || {
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(rustup_worker_loop(r, handle_weak))
            }
        });
        Self {
            channel,
            worker_thread,
        }
    }

    pub fn join(self) -> std::thread::Result<()> {
        let _ = self.channel.send(RustupMessage::Quit);
        self.worker_thread.join()
    }
}

async fn rustup_worker_loop(mut r: UnboundedReceiver<RustupMessage>, handle: slint::Weak<CargoUI>) {
    let refresh_handle = tokio::task::spawn(refresh_toolchains(handle.clone()));

    loop {
        let m = r.recv().await;

        match m {
            None => return,
            Some(RustupMessage::Quit) => {
                refresh_handle.abort();
                return;
            }
        }
    }
}

async fn refresh_toolchains(handle: slint::Weak<CargoUI>) -> tokio::io::Result<()> {
    handle.clone().upgrade_in_event_loop(|ui| {
        ui.set_toolchains_available(false);
    });

    let mut rustup_command = tokio::process::Command::new("rustup");
    rustup_command.arg("toolchain").arg("list");
    let mut spawn_result = rustup_command
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let mut stdout = BufReader::new(spawn_result.stdout.take().unwrap()).lines();

    let mut toolchains = Vec::new();

    while let Some(line) = stdout.next_line().await? {
        let name: SharedString = line.into();
        toolchains.push({
            let default = name.contains("(default)");
            Toolchain { name, default }
        });
    }

    handle.upgrade_in_event_loop(|ui| {
        ui.set_toolchains(ModelRc::from(
            Rc::new(VecModel::from(toolchains)) as Rc<dyn Model<Data = Toolchain>>
        ));
        ui.set_toolchains_available(true);
    });

    Ok(())
}
