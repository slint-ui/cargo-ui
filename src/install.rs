/* Copyright © 2021 SixtyFPS GmbH <info@sixtyfps.info>
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */

use std::{
    collections::{HashSet, VecDeque},
    rc::Rc,
    str::FromStr,
};

use super::*;
use cargo_metadata::Version;
use slint::{ModelRc, SharedString, VecModel};
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Debug, Clone)]
pub enum InstallJob {
    Install(SharedString),
    Uninstall(SharedString),
}

impl InstallJob {
    pub fn crate_name(&self) -> &SharedString {
        match self {
            InstallJob::Install(a) => a,
            InstallJob::Uninstall(a) => a,
        }
    }
}

pub async fn refresh_install_list(
    handle: slint::Weak<CargoUI>,
) -> tokio::io::Result<Vec<InstalledCrate>> {
    let mut cargo_install_command = cargo::cargo_command();
    cargo_install_command.arg("install").arg("--list");
    let mut spawn_result = cargo_install_command
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let mut stdout = BufReader::new(spawn_result.stdout.take().unwrap()).lines();
    let mut result = Vec::new();

    while let Some(line) = stdout.next_line().await? {
        if line.starts_with(' ') {
            // Just skip the binary names
            continue;
        }
        if !line.ends_with(':') {
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
            ..Default::default()
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
    Ok(result)
}

fn report_error<T: Default>(_handle: slint::Weak<CargoUI>, arg: &str) -> T {
    /* TODO
    handle.clone().upgrade_in_event_loop(|ui| {

    });*/
    eprintln!("{}", arg);
    Default::default()
}

pub async fn process_install(job: InstallJob, handle: slint::Weak<CargoUI>) -> std::io::Result<()> {
    let mut cmd = cargo::cargo_command();
    match &job {
        InstallJob::Install(cr) => cmd.arg("install").arg("--force").arg(cr.as_str()),
        InstallJob::Uninstall(cr) => cmd.arg("uninstall").arg(cr.as_str()),
    };
    let mut res = cmd
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let mut stdout = BufReader::new(res.stdout.take().unwrap()).lines();
    let mut stderr = BufReader::new(res.stderr.take().unwrap()).lines();
    loop {
        let status = tokio::select! {
            line = stderr.next_line() => {
                if let Some(line) = line? { line } else { break }

            }
            line = stdout.next_line() => {
                if let Some(line) = line? { line } else { break }
            }
        };
        let crate_name = job.crate_name().clone();
        handle.clone().upgrade_in_event_loop(move |cargo_ui| {
            let installed = cargo_ui.global::<CargoInstallData>().get_crates();
            for i in 0..installed.row_count() {
                if let Some(mut c) = installed.row_data(i) {
                    if c.name == crate_name {
                        c.progress = true;
                        c.status = status.into();
                        installed
                            .as_any()
                            .downcast_ref::<VecModel<InstalledCrate>>()
                            .unwrap()
                            .set_row_data(i, c);
                        return;
                    }
                }
            }
        });
    }
    Ok(())
}

pub fn apply_install_list(
    mut list: Vec<InstalledCrate>,
    crates_index: Option<&crates_index::Index>,
    install_queue: &VecDeque<InstallJob>,
    currently_installing: &SharedString,
    handle: slint::Weak<CargoUI>,
) {
    let mut set: HashSet<_> = install_queue.iter().map(|ci| ci.crate_name()).collect();
    if !currently_installing.is_empty() {
        set.insert(currently_installing);
    }
    for cr in list.iter_mut() {
        cr.queued = set.remove(&cr.name);
        cr.new_version = crates_index
            .and_then(|idx| idx.crate_(&cr.name))
            .and_then(|from_idx| {
                let new_version = from_idx.highest_stable_version()?.version();
                (Version::from_str(new_version).ok()?
                    > Version::from_str(cr.version.strip_prefix("v")?).ok()?)
                .then(|| new_version)
                .map(|x| x.into())
            })
            .unwrap_or_default();
    }
    for cr in set {
        list.push(InstalledCrate {
            name: cr.clone(),
            queued: true,
            ..Default::default()
        })
    }

    handle.upgrade_in_event_loop(move |ui| {
        ui.global::<CargoInstallData>().set_crates(ModelRc::from(
            Rc::new(VecModel::from(list)) as Rc<dyn Model<Data = InstalledCrate>>
        ));
    });
}
