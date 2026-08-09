// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

mod dir;
mod entries;
mod file;
mod glob;
mod grep;

pub use entries::init_mod_fs;
pub(crate) use file::{TempFileGuard, write_file_atomic};
