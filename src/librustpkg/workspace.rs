// Copyright 2013 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// rustpkg utilities having to do with workspaces

use std::{os,util};
use std::path::Path;
use context::Context;
use path_util::{workspace_contains_package_id, find_dir_using_rust_path_hack, default_workspace};
use path_util::rust_path;
use util::option_to_vec;
use package_id::PkgId;

pub fn each_pkg_parent_workspace(cx: &Context, pkgid: &PkgId, action: &fn(&Path) -> bool) -> bool {
    // Using the RUST_PATH, find workspaces that contain
    // this package ID
    let workspaces = pkg_parent_workspaces(cx, pkgid);
    if workspaces.is_empty() {
        // tjc: make this a condition
        fail2!("Package {} not found in any of \
                    the following workspaces: {}",
                   pkgid.path.to_str(),
                   rust_path().to_str());
    }
    for ws in workspaces.iter() {
        if action(ws) {
            break;
        }
    }
    return true;
}

/// Given a package ID, return a vector of all of the workspaces in
/// the RUST_PATH that contain it
pub fn pkg_parent_workspaces(cx: &Context, pkgid: &PkgId) -> ~[Path] {
    let rs: ~[Path] = rust_path().move_iter()
        .filter(|ws| workspace_contains_package_id(pkgid, ws))
        .collect();
    if cx.use_rust_path_hack {
        rs + option_to_vec(find_dir_using_rust_path_hack(pkgid))
    }
    else {
        rs
    }
}

pub fn is_workspace(p: &Path) -> bool {
    os::path_is_dir(&p.push("src"))
}

/// Construct a workspace and package-ID name based on the current directory.
/// This gets used when rustpkg gets invoked without a package-ID argument.
pub fn cwd_to_workspace() -> Option<(Path, PkgId)> {
    let cwd = os::getcwd();
    for path in rust_path().move_iter() {
        let srcpath = path.push("src");
        if srcpath.is_ancestor_of(&cwd) {
            // I'd love to use srcpath.get_relative_to(cwd) but it behaves wrong
            // I'd say broken, but it has tests enforcing the wrong behavior.
            // instead, just hack up the components vec
            let mut pkgid = cwd;
            pkgid.is_absolute = false;
            let comps = util::replace(&mut pkgid.components, ~[]);
            pkgid.components = comps.move_iter().skip(srcpath.components.len()).collect();
            return Some((path, PkgId::new(pkgid.components.connect("/"))))
        }
    }
    None
}

/// If `workspace` is the same as `cwd`, and use_rust_path_hack is false,
/// return `workspace`; otherwise, return the first workspace in the RUST_PATH.
pub fn determine_destination(cwd: Path, use_rust_path_hack: bool, workspace: &Path) -> Path {
    if workspace == &cwd && !use_rust_path_hack {
        workspace.clone()
    }
    else {
        default_workspace()
    }
}
