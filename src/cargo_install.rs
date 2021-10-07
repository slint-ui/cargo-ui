/* Copyright © 2021 SixtyFPS GmbH <info@sixtyfps.info>
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */

use std::rc::Rc;

use super::{CargoInstallData, CargoUI, InstalledCrate};
use sixtyfps::{ComponentHandle, Global, Model, ModelHandle, VecModel};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

#[derive(Debug)]
pub enum CargoInstallMessage {
    Refresh,
    Quit,
}

pub struct CargoInstallWorker {
    pub channel: UnboundedSender<CargoInstallMessage>,
    worker_thread: std::thread::JoinHandle<()>,
}

impl CargoInstallWorker {
    pub fn new(cargo_ui: &CargoUI) -> Self {
        let (channel, r) = tokio::sync::mpsc::unbounded_channel();
        let worker_thread = std::thread::spawn({
            let handle_weak = cargo_ui.as_weak();
            move || {
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(cargo_install_worker_loop(r, handle_weak))
            }
        });
        Self {
            channel,
            worker_thread,
        }
    }

    pub fn join(self) -> std::thread::Result<()> {
        let _ = self.channel.send(CargoInstallMessage::Quit);
        self.worker_thread.join()
    }
}

async fn cargo_install_worker_loop(
    mut r: UnboundedReceiver<CargoInstallMessage>,
    handle: sixtyfps::Weak<CargoUI>,
) {
    let mut refresh_handle = tokio::task::spawn(refresh(handle.clone()));

    loop {
        let m = r.recv().await;

        match m {
            None => return,
            Some(CargoInstallMessage::Quit) => {
                refresh_handle.abort();
                return;
            }
            Some(CargoInstallMessage::Refresh) => {
                refresh_handle.abort();
                refresh_handle = tokio::task::spawn(refresh(handle.clone()));
            }
        }
    }
}

async fn refresh(handle: sixtyfps::Weak<CargoUI>) -> tokio::io::Result<()> {
    let cargo_path = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cargo_install_command = tokio::process::Command::new(cargo_path);
    cargo_install_command.arg("install").arg("--list");
    let mut spawn_result = cargo_install_command
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let mut stdout = BufReader::new(spawn_result.stdout.take().unwrap()).lines();
    let mut result = Vec::new();

    while let Some(line) = stdout.next_line().await? {
        if line.starts_with(' ') || !line.ends_with(':') {
            return Ok(report_error(
                handle,
                &format!("Parse error: expected crate description: '{}'", line),
            ));
        }
        let line = line.trim_end_matches(':');
        let (name, rest) = line.split_once(' ').unwrap_or((line, ""));
        if !rest.starts_with('v') {
            return Ok(report_error(
                handle,
                &format!("Parse error: expected version number in : '{}'", line),
            ));
        }
        let (version, _rest) = rest.split_once(' ').unwrap_or((rest, ""));
        result.push(InstalledCrate {
            name: name.into(),
            version: version.into(),
        });
        let next_line = stdout.next_line().await?;
        if let Some(next_line) = next_line {
            if !next_line.starts_with(' ') {
                return Ok(report_error(
                    handle,
                    &format!("Parse error: expected crate name: '{}'", next_line),
                ));
            }
        } else {
            break;
        }
    }

    handle.upgrade_in_event_loop(move |ui| {
        CargoInstallData::get(&ui).set_crates(ModelHandle::from(
            Rc::new(VecModel::from(result)) as Rc<dyn Model<Data = InstalledCrate>>
        ));
    });

    Ok(())
}

fn report_error(_handle: sixtyfps::Weak<CargoUI>, arg: &str) {
    /* TODO
    handle.clone().upgrade_in_event_loop(|ui| {

    });*/
    eprintln!("{}", arg);
}
