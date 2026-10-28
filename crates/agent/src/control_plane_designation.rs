/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: Apache-2.0
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 * http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Persistent record of the host's control-plane designation.
//!
//! The designation reaches the DPU in `ManagedHostNetworkConfigResponse`. The
//! agent keeps the last value Core sent beside the HBN files the designation
//! protects.

use std::fs;
use std::path::{Path, PathBuf};

use eyre::Context;
use serde::{Deserialize, Serialize};

use crate::ethernet_virtualization::{self, FPath};

/// Beside `nvue_startup.yaml` and the DPU-local DHCP files, on the same
/// persistent DPU storage they rely on.
const PATH: &str = "var/support/nico-control-plane.json";

/// A JSON object rather than a bare boolean so later fields, such as the
/// retention bound this does not yet implement, do not change the format.
#[derive(Debug, Deserialize, Serialize)]
struct Record {
    control_plane: bool,
}

fn path(hbn_root: &Path) -> PathBuf {
    hbn_root.join(PATH)
}

/// The last designation Core sent, or `false` when none was ever recorded.
///
/// Every failure reads as undesignated. An agent that cannot tell whether it is
/// designated keeps the unconditional reset behavior, so a damaged file cannot
/// grant retention. The next successful fetch rewrites the file.
pub(super) fn load(hbn_root: &Path) -> bool {
    let path = path(hbn_root);
    match read(&path) {
        Ok(control_plane) => control_plane,
        Err(err) => {
            tracing::warn!(
                error = format!("{err:#}"),
                path = %path.display(),
                "Cannot read control-plane designation; treating this DPU as undesignated"
            );
            false
        }
    }
}

fn read(path: &Path) -> eyre::Result<bool> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(serde_json::from_str::<Record>(&contents)?.control_plane),
        // Never recorded, which is the state of a DPU that has not yet reached
        // a Core that sends the designation.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

/// Records the designation Core sent. Called on every successful fetch; the
/// shared file writer skips the disk when the contents already match. A failure
/// here only costs the next cold start its designation, so it is logged rather
/// than propagated into the main loop.
pub(super) fn store(hbn_root: &Path, control_plane: bool) {
    if let Err(err) = write(hbn_root, control_plane) {
        tracing::error!(
            error = format!("{err:#}"),
            "Failed to record control-plane designation"
        );
    }
}

fn write(hbn_root: &Path, control_plane: bool) -> eyre::Result<()> {
    let path = FPath(path(hbn_root));
    let parent = path
        .0
        .parent()
        .ok_or_else(|| eyre::eyre!("{path} has no parent directory"))?;
    fs::create_dir_all(parent)
        .wrap_err_with(|| format!("create directory {}", parent.display()))?;

    let contents = serde_json::to_string(&Record { control_plane })?;
    ethernet_virtualization::write(contents, &path, "control-plane designation", false)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_without_a_record_is_undesignated() {
        let root = tempfile::tempdir().unwrap();
        assert!(!load(root.path()));
    }

    #[test]
    fn store_then_load_round_trips() {
        let root = tempfile::tempdir().unwrap();
        for designated in [true, false, true] {
            store(root.path(), designated);
            assert_eq!(load(root.path()), designated, "stored {designated}");
        }
    }

    #[test]
    fn an_unparsable_record_is_undesignated_and_is_repaired() {
        let root = tempfile::tempdir().unwrap();
        store(root.path(), true);
        fs::write(path(root.path()), "{not json").unwrap();
        assert!(!load(root.path()));

        // Storing on every successful fetch is what repairs the file.
        store(root.path(), true);
        assert!(load(root.path()));
    }
}
