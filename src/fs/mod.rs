// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

mod dir;
mod entries;
mod file;
mod glob;

pub use entries::init_mod_fs;
pub(crate) use file::write_file_atomic;
