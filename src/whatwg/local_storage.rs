// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! localStorage implementation backed by redb (WHATWG HTML §12).

use std::cell::RefCell;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use zuqe::{CallContext, FieldDef, JSResult, JSValue, Object, ScopedContext};

use crate::helpers::aike_home;

const LS_TABLE: TableDefinition<&str, &str> = TableDefinition::new("localStorage");

thread_local! {
    static LOCAL_STORAGE: RefCell<Option<Database>> = const { RefCell::new(None) };
}

/// Return the db path: CLI arg > env > default.
fn storage_db_path(cli_path: Option<&str>) -> std::path::PathBuf {
    if let Some(p) = cli_path {
        return std::path::PathBuf::from(p);
    }
    aike_home().join("localStorage.redb")
}

/// Eager init — called once from `install_local_storage`.
/// Returns false when the database is already held by another process
/// (spawned child), in which case localStorage is silently disabled.
fn init_storage(db_path: Option<&str>) -> bool {
    LOCAL_STORAGE.with_borrow_mut(|opt| {
        if opt.is_some() {
            return true;
        }
        let path = storage_db_path(db_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        match Database::create(&path) {
            Ok(db) => {
                *opt = Some(db);
                true
            }
            Err(e) => {
                eprintln!(
                    "[WARN] failed to open localStorage database at '{}': {e}, \
                     this environment has no localStorage",
                    path.display()
                );
                false
            }
        }
    })
}

fn with_db<R>(f: impl FnOnce(&Database) -> R) -> R {
    LOCAL_STORAGE.with_borrow(|opt| {
        f(opt
            .as_ref()
            .expect("localStorage not initialized — call init_storage() first"))
    })
}

fn js_get_item(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let key = cc.arg(0).to_string(ctx)?.to_string();
    let result = with_db(|db| -> Option<String> {
        let txn = db.begin_read().unwrap();
        txn.open_table(LS_TABLE)
            .ok()
            .and_then(|t| t.get(key.as_str()).ok().flatten())
            .map(|val| val.value().to_string())
    });
    match result {
        Some(s) => Ok(JSValue::make_string(
            ctx.create_string_from_utf8(s.into_bytes())?,
        )),
        None => Ok(JSValue::NULL),
    }
}

fn js_set_item(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let key = cc.arg(0).to_string(ctx)?.to_string();
    let value = cc.arg(1).to_string(ctx)?.to_string();
    with_db(|db| {
        let txn = db.begin_write().unwrap();
        {
            let mut table = txn.open_table(LS_TABLE).unwrap();
            table.insert(key.as_str(), value.as_str()).unwrap();
        }
        txn.commit().unwrap();
    });
    Ok(JSValue::UNDEFINED)
}

fn js_remove_item(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let key = cc.arg(0).to_string(ctx)?.to_string();
    with_db(|db| {
        let txn = db.begin_write().unwrap();
        {
            let mut table = txn.open_table(LS_TABLE).unwrap();
            let _ = table.remove(key.as_str());
        }
        txn.commit().unwrap();
    });
    Ok(JSValue::UNDEFINED)
}

fn js_clear(_ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    with_db(|db| {
        let txn = db.begin_write().unwrap();
        {
            let _ = txn.delete_table(LS_TABLE);
            let _ = txn.open_table(LS_TABLE);
        }
        txn.commit().unwrap();
    });
    Ok(JSValue::UNDEFINED)
}

fn js_key(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let index = cc.arg(0).to_number(ctx)?.to_i32() as u64;
    let result = with_db(|db| -> Option<String> {
        let txn = db.begin_read().unwrap();
        txn.open_table(LS_TABLE).ok().and_then(|table| {
            let mut iter = table.iter().ok()?;
            iter.nth(index as usize)
                .and_then(|r| r.ok())
                .map(|item| item.0.value().to_string())
        })
    });
    match result {
        Some(s) => Ok(JSValue::make_string(
            ctx.create_string_from_utf8(s.into_bytes())?,
        )),
        None => Ok(JSValue::NULL),
    }
}

fn js_length(_ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    let len = with_db(|db| -> usize {
        let txn = db.begin_read().unwrap();
        match txn.open_table(LS_TABLE) {
            Ok(table) => {
                let mut count = 0usize;
                let mut iter = table.iter().unwrap();
                while iter.next().is_some() {
                    count += 1;
                }
                count
            }
            Err(_) => 0,
        }
    });
    Ok(JSValue::from(len as i32))
}

/// Install `localStorage` on the global object.
/// Returns Ok(()) even when the database is already held by another process;
/// in that case localStorage is simply not installed (fire-and-forget spawns).
pub(crate) fn install_local_storage(
    ctx: &mut ScopedContext,
    global: &mut Object,
    db_path: Option<&str>,
) -> Result<(), i32> {
    if !init_storage(db_path) {
        return Ok(());
    }

    let mut storage = ctx.create_base_object()?;

    storage.install_fields(
        ctx,
        &[
            FieldDef::GETSET("length", Some(js_length), None, 0),
            FieldDef::FUNC("getItem", 1, js_get_item, 0),
            FieldDef::FUNC("setItem", 2, js_set_item, 0),
            FieldDef::FUNC("removeItem", 1, js_remove_item, 0),
            FieldDef::FUNC("clear", 0, js_clear, 0),
            FieldDef::FUNC("key", 1, js_key, 0),
            FieldDef::STR(
                "[Symbol.toStringTag]",
                "Storage",
                zuqe::JS_PROP_CONFIGURABLE,
            ),
        ],
    )?;

    global.define_property_value_str(
        ctx,
        "localStorage",
        storage.into(),
        zuqe::JS_PROP_CONFIGURABLE,
    )?;

    Ok(())
}
