/* Copyright © 2021 SixtyFPS GmbH <info@sixtyfps.info>
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */

use super::CargoUI;
use sixtyfps::ComponentHandle;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

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

async fn rustup_worker_loop(
    mut r: UnboundedReceiver<RustupMessage>,
    _handle: sixtyfps::Weak<CargoUI>,
) {
    loop {
        let m = r.recv().await;

        match m {
            None => return,
            Some(RustupMessage::Quit) => return,
        }
    }
}
