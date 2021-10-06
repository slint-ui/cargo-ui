/* Copyright © 2021 SixtyFPS GmbH <info@sixtyfps.info>
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */

// FIXME: Re-enable clippy when sixtyfps generated code is clippy-clean.
#[allow(clippy::all)]
mod generated_code {
    sixtyfps::include_modules!();
}
pub use generated_code::*;

mod cargo;
use cargo::*;
mod rustup;
use rustup::*;

fn main() {
    let cargo_ui = CargoUI::new();

    let cargo_worker = CargoWorker::new(&cargo_ui);
    let rustup_worker = RustupWorker::new(&cargo_ui);

    cargo_ui.on_open_url(|url| {
        open::that_in_background(url.as_str());
    });
    cargo_ui.set_cargo_ui_version(env!("CARGO_PKG_VERSION").into());

    cargo_ui.set_workspace_valid(false);

    cargo_ui.on_action({
        let cargo_channel = cargo_worker.channel.clone();
        let ui_handle = cargo_ui.as_weak();
        move |action| {
            cargo_channel
                .send(CargoMessage::Action {
                    action,
                    feature_settings: FeatureSettings::new(&ui_handle.upgrade().unwrap()),
                })
                .unwrap()
        }
    });
    cargo_ui.on_cancel({
        let cargo_channel = cargo_worker.channel.clone();
        move || cargo_channel.send(CargoMessage::Cancel).unwrap()
    });
    cargo_ui.on_show_open_dialog({
        let cargo_channel = cargo_worker.channel.clone();
        move || cargo_channel.send(CargoMessage::ShowOpenDialog).unwrap()
    });
    cargo_ui.on_reload_manifest({
        let cargo_channel = cargo_worker.channel.clone();
        move |m| cargo_channel.send(CargoMessage::ReloadManifest(m)).unwrap()
    });
    cargo_ui.on_package_selected({
        let cargo_channel = cargo_worker.channel.clone();
        move |pkg| {
            cargo_channel
                .send(CargoMessage::PackageSelected(pkg))
                .unwrap()
        }
    });

    cargo_ui.run();

    cargo_worker.join().unwrap();
    rustup_worker.join().unwrap();
}
