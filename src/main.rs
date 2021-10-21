/* Copyright © 2021 SixtyFPS GmbH <info@sixtyfps.info>
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */

// FIXME: Re-enable clippy when sixtyfps generated code is clippy-clean.
#[allow(clippy::all)]
mod generated_code {
    sixtyfps::include_modules!();
}
use cargo_metadata::DependencyKind;
pub use generated_code::*;

mod cargo;
mod install;
mod rustup;

use install::InstallJob;
use sixtyfps::Model;

use crate::cargo::{CargoMessage, FeatureSettings};

fn main() {
    let cargo_ui = CargoUI::new();

    let cargo_worker = cargo::CargoWorker::new(&cargo_ui);
    let rustup_worker = rustup::RustupWorker::new(&cargo_ui);

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
        move |current_manifest| {
            let selected_manifest =
                cargo::show_open_dialog(std::path::PathBuf::from(current_manifest.as_str()).into());

            cargo_channel
                .send(CargoMessage::ReloadManifest(selected_manifest))
                .unwrap();
        }
    });
    cargo_ui.on_reload_manifest({
        let cargo_channel = cargo_worker.channel.clone();
        move |m| {
            cargo_channel
                .send(CargoMessage::ReloadManifest(
                    std::path::PathBuf::from(m.as_str()).into(),
                ))
                .unwrap()
        }
    });
    cargo_ui.on_package_selected({
        let cargo_channel = cargo_worker.channel.clone();
        move |pkg| {
            cargo_channel
                .send(CargoMessage::PackageSelected(pkg))
                .unwrap()
        }
    });

    cargo_ui.global::<DependencyData>().on_remove({
        let cargo_channel = cargo_worker.channel.clone();
        move |pkg, dep, dep_kind| {
            cargo_channel
                .send(CargoMessage::DependencyRemove {
                    parent_package: pkg,
                    crate_name: dep,
                    dep_kind: dep_kind_from_str(dep_kind),
                })
                .unwrap()
        }
    });
    cargo_ui.global::<DependencyData>().on_request_upgrade({
        let cargo_channel = cargo_worker.channel.clone();
        move |pkg, dep, dep_kind| {
            cargo_channel
                .send(CargoMessage::DependencyUpgrade {
                    parent_package: pkg,
                    crate_name: dep,
                    dep_kind: dep_kind_from_str(dep_kind),
                })
                .unwrap()
        }
    });
    cargo_ui.global::<DependencyData>().on_add_dependency({
        let cargo_channel = cargo_worker.channel.clone();
        move |dep, dep_kind| {
            cargo_channel
                .send(CargoMessage::DependencyAdd {
                    crate_name: dep,
                    dep_kind: dep_kind_from_str(dep_kind),
                })
                .unwrap()
        }
    });
    cargo_ui.global::<CargoInstallData>().on_upgrade({
        let cargo_channel = cargo_worker.channel.clone();
        move |c| {
            cargo_channel
                .send(CargoMessage::Install(InstallJob::Install(c)))
                .unwrap()
        }
    });
    cargo_ui.global::<CargoInstallData>().on_uninstall({
        let cargo_channel = cargo_worker.channel.clone();
        move |c| {
            cargo_channel
                .send(CargoMessage::Install(InstallJob::Uninstall(c)))
                .unwrap()
        }
    });
    cargo_ui.global::<CargoInstallData>().on_upgrade_all({
        let cargo_channel = cargo_worker.channel.clone();
        let cargo_ui = cargo_ui.as_weak();
        move || {
            let installed = cargo_ui.unwrap().global::<CargoInstallData>().get_crates();
            for i in 0..installed.row_count() {
                let mut c = installed.row_data(i);
                if !c.queued && !c.new_version.is_empty() {
                    c.queued = true;
                    cargo_channel
                        .send(CargoMessage::Install(InstallJob::Install(c.name.clone())))
                        .unwrap();
                    // as_any() to workaround that set_row_data was not implemented in ModelHandle in SixtyFPS 0.1.3
                    installed
                        .as_any()
                        .downcast_ref::<sixtyfps::VecModel<InstalledCrate>>()
                        .unwrap()
                        .set_row_data(i, c);
                }
            }
        }
    });
    cargo_ui.global::<CargoInstallData>().on_install({
        let cargo_channel = cargo_worker.channel.clone();
        let cargo_ui = cargo_ui.as_weak();
        move |c| {
            let installed = cargo_ui.unwrap().global::<CargoInstallData>().get_crates();
            if let Some(installed) = installed
                .as_any()
                .downcast_ref::<sixtyfps::VecModel<InstalledCrate>>()
            {
                installed.push(InstalledCrate {
                    name: c.clone(),
                    queued: true,
                    ..Default::default()
                });
            }
            cargo_channel
                .send(CargoMessage::Install(InstallJob::Install(c)))
                .unwrap()
        }
    });
    cargo_ui
        .global::<CratesCompletionData>()
        .on_update_completion({
            let cargo_channel = cargo_worker.channel.clone();
            move |cpl| {
                cargo_channel
                    .send(CargoMessage::UpdateCompletion(cpl))
                    .unwrap()
            }
        });

    cargo_ui.run();

    cargo_worker.join().unwrap();
    rustup_worker.join().unwrap();
}

fn dep_kind_from_str(dep_kind: sixtyfps::SharedString) -> DependencyKind {
    match dep_kind.as_str() {
        "dev" => DependencyKind::Development,
        "build" => DependencyKind::Build,
        _ => DependencyKind::Normal,
    }
}
