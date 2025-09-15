#![feature(once_cell_try, string_from_utf8_lossy_owned)]

use itertools::Itertools;
use std::{
    alloc::{GlobalAlloc, System},
    borrow::Cow,
    cell::Cell,
    error::Error,
    sync::{Arc, LazyLock, Mutex, OnceLock, RwLock, TryLockError},
};

use macrosia::{regex, Executor, Macro, MacroError, TextMacro, VariableRegistry};
use pyo3::{exceptions::PyAssertionError, prelude::*, types::{PyDict, PyList, PyString}};
use rusqlite::{params_from_iter, Connection};

mod gil;
use gil::AllowThreads;

struct LimitAlloc;

static MEMORY_LIMIT: usize = 8 * 1024 * 1024; // 8 MiB

thread_local! {
    static LIMIT_ALLOCATIONS: Cell<bool> = Cell::new(false);
    static ALLOCATED_MEMORY: Cell<usize> = Cell::new(0);
}

unsafe impl GlobalAlloc for LimitAlloc {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        if LIMIT_ALLOCATIONS.get() {
            let current = ALLOCATED_MEMORY.get();
            let new = current.checked_add(layout.size());
            if new.is_none_or(|v| v > MEMORY_LIMIT) {
                LIMIT_ALLOCATIONS.set(false);
                panic!("memory limit exhausted");
            }
            ALLOCATED_MEMORY.set(new.unwrap());
        } else {
            ALLOCATED_MEMORY.set(ALLOCATED_MEMORY.get() + layout.size());
        }
        let ptr = System.alloc(layout);
        if ptr.is_null() {
            ALLOCATED_MEMORY.set(ALLOCATED_MEMORY.get() - layout.size());
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        ALLOCATED_MEMORY.set(ALLOCATED_MEMORY.get() - layout.size());
        System.dealloc(ptr, layout);
    }
}

#[global_allocator]
static ALLOC: LimitAlloc = LimitAlloc;

static DB_CONN: OnceLock<Mutex<Connection>> = OnceLock::new();

struct TilesMacro;

impl Macro for TilesMacro {
    fn name(&self) -> &[u8] {
        b"tiles"
    }
    fn source(&self) -> &[u8] {
        b"<yea i can't be assed to figure out how to make this show up here sorry>"
    }
    fn eval<'arg, 'reg: 'arg, 'exec: 'reg>(
        &self,
        _x: &'exec macrosia::Executor,
        _v: &'reg mut VariableRegistry,
        _r: &mut macrosia::rand::rngs::SmallRng,
        args: &mut dyn Iterator<Item = &'arg [u8]>,
    ) -> Result<Cow<'static, [u8]>, MacroError> {
        let mut queries = args.map(str::from_utf8).process_results(|iter| {
            iter.map(|v| {
                let Some((query, value)) = v.split_once(':') else {
                    return Err(format!("invalid query: {v}"));
                };
                Ok((query, value))
            })
            .collect::<Result<Vec<_>, _>>()
        })??;
        #[allow(suspicious_double_ref_op)]
        // somehow cloning the &str resolves lifetime issues. i'm not questioning it
        queries.dedup_by_key(|(query, _)| query.clone());
        let conn = DB_CONN
            .get()
            .ok_or("database not initialized - this is a bug in the bot, please report!")?;

        let mut query_string = String::from("SELECT DISTINCT name FROM tiles WHERE 1");
        let mut args: Vec<rusqlite::types::Value> = vec![];

        for query_res in queries {
            let (query, value) = query_res;
            match query {
                "name" => {
                    query_string.push_str(" AND name REGEXP ?");
                    args.push(rusqlite::types::Value::Text(format!("^{value}$")))
                }
                "tiling" => {
                    let tiling_int = match value {
                        "icon" => -3,
                        "custom" => -2,
                        "none" => -1,
                        "directional" => 0,
                        "tiling" => 1,
                        "character" => 2,
                        "animated_directional" => 3,
                        "animated" => 4,
                        "static_character" => 5,
                        "diagonal_tiling" => 6,
                        _ => return Err(format!("invalid tiling mode: {value}"))?,
                    };
                    query_string.push_str(" AND tiling == ?");
                    args.push(rusqlite::types::Value::Integer(tiling_int))
                }
                "source" => {
                    query_string.push_str(" AND source == ?");
                    args.push(rusqlite::types::Value::Text(value.to_string()))
                }
                "tag" => {
                    query_string.push_str(" AND INSTR(tags, ?)");
                    args.push(rusqlite::types::Value::Text(value.to_string()))
                }
                v => return Err(format!("invalid query: {v}"))?,
            }
        }

        conn.clear_poison();
        let tiles: Vec<String> = {
            let conn = conn
                .lock()
                .map_err(|_| "database connection poisoned - this is a bug, please report!")?;
            let mut query = conn
                .prepare_cached(&query_string)
                .map_err(|_| "invalid query - was there a null byte in one of the query values?")?;
            let res = query
                .query_map(params_from_iter(args), |row| row.get::<_, String>(0))
                .map_err(|_| "failed to execute SQL query - this is a bug, please report!")?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| "failed to fetch database row - this is a bug, please report!")?;
            res
        };

        Ok(Cow::Owned(
            tiles
                .into_iter()
                .sorted()
                .map(|tilename| {
                    tilename
                        .replace("\\", "\\\\")
                        .replace("[", "\\[")
                        .replace("/", "\\/")
                        .replace("]", "\\]")
                        .replace(" ", "\\ ")
                        .replace("$", "\\$")
                })
                .join("/")
                .into_bytes(),
        ))
    }
    fn clone(&self) -> Box<dyn macrosia::Macro> {
        Box::new(Self)
    }
    fn description(&self) -> &str {
        ""
    }
}

static EXECUTOR: LazyLock<RwLock<Executor>> = LazyLock::new(|| RwLock::new(Executor::new(0)));

#[pyfunction]
fn connect_to_db(py: Python, path: String) -> PyResult<()> {
    py.detach(|| {
        DB_CONN
            .get_or_try_init(|| {
                Ok(Mutex::new({
                    let conn = Connection::open(path)?;
                    add_regexp_function(&conn)?;
                    conn
                }))
            })
            .map(|_| ())
            .map_err(|err: rusqlite::Error| -> PyErr {
                PyAssertionError::new_err(format!("{err}")).into()
            })
    })
}

macro_rules! sql_py_err {
    ($expr: expr) => {
        ($expr).map_err(|err: rusqlite::Error| PyAssertionError::new_err(format!("{err}")))
    };
}

#[pyfunction]
fn update_macros(py: Python) -> PyResult<bool> {
    py.detach(|| {
        LIMIT_ALLOCATIONS.set(false);
        let Some(db) = DB_CONN.get() else {
            Err(PyAssertionError::new_err(
                "The SQLite database has not been connected yet! Try again in a few moments.",
            ))?;
            unreachable!()
        };
        EXECUTOR.clear_poison();
        let mut exec = {
            let e = EXECUTOR.try_write();
            match e {
                Ok(v) => v,
                Err(TryLockError::Poisoned(err)) => unreachable!("executor was poisoned: {err}"),
                Err(TryLockError::WouldBlock) => {
                    return Ok(false);
                }
            }
        };

        db.clear_poison();
        {
            let db = match db.try_lock() {
                Ok(v) => v,
                Err(TryLockError::Poisoned(err)) => unreachable!("database was poisoned: {err}"),
                Err(_) => return Ok(false),
            };
            let mut stmt = sql_py_err!(db.prepare("SELECT name, value FROM macros"))?;
            let mut rows = sql_py_err!(stmt.query([]))?;
            exec.clear_macros();
            while let Some(row) = sql_py_err!(rows.next())? {
                exec.add_macro(TextMacro {
                    name: Arc::new(row.get::<_, String>(0).unwrap().into_bytes()),
                    source: Arc::new(row.get::<_, String>(1).unwrap().into_bytes()),
                    description: Arc::new(String::new()),
                });
            }
        }
        exec.add_stdlib();
        exec.add_macro(TilesMacro);

        Ok(true)
    })
}

#[pyfunction]
fn evaluate<'py>(
    py: Python<'py>,
    program: String,
    ctx: u8,
    step_limit: Option<usize>,
    debug_log: Option<Py<PyList>>,
) -> PyResult<Bound<'py, PyAny>> {
    let pin = Box::pin(async move {
        LIMIT_ALLOCATIONS.set(true);
        let thread = std::thread::spawn(move || -> Result<Option<String>, MacroError> {
            LIMIT_ALLOCATIONS.set(true);
            EXECUTOR.clear_poison();
            let exec = match EXECUTOR.try_read() {
                Ok(exec) => exec,
                Err(TryLockError::WouldBlock) => return Ok(None),
                Err(_) => return Err("executor is poisoned - this is a bug, please report!")?,
            };
            exec.set_context(ctx);
            let mut var_reg = VariableRegistry::new();
            let mut debug_vec = vec![];
            let readout = debug_log.as_ref().map(|_| &mut debug_vec);
            let mut generator = exec.evaluate(program.as_bytes(), &mut var_reg, step_limit, readout);
            loop {
                let Some(res) = generator() else { continue };
                drop(generator);
                if let Some(log) = debug_log {
                    Python::attach(|py| {
                        for str in debug_vec {
                            log.call_method1(py, "append", (str as String, )).expect("failed to append to debug log");
                        }
                    })
                }
                break res.map(|v| Some(String::from_utf8_lossy_owned(v.into_owned())));
            }
        });
        let res = async move { thread.join() }.await;
        LIMIT_ALLOCATIONS.set(true);

        match res {
            Err(panic_payload) => {
                if let Some(&"memory limit exhausted") =
                    panic_payload.downcast_ref::<&'static str>()
                {
                    return Ok(Some((false, "memory limit exhausted during macro execution, but not during expansion".to_string(), None)));
                }
                std::panic::resume_unwind(panic_payload)
            }
            Ok(Err(macro_error)) => Ok(Some((false,
                macro_error.message().to_string(),
                Some(macro_error.trace().iter().map(|v| String::from_utf8_lossy(v).into_owned()).collect::<Vec<String>>())
            ))),
            Ok(Ok(res)) => Ok(res.map(|r| (true, r, None))),
        }
    });
    pyo3_async_runtimes::async_std::future_into_py(py, pin)
}

#[pyfunction]
fn get_builtins(py: Python) -> PyResult<Vec<Py<PyAny>>> {
    let mut exec = Executor::new(0);
    exec.add_stdlib();
    let mut vec = vec![];
    for mac in exec.macros().values() {
        let dict = PyDict::new(py);
        dict.set_item("name", String::from_utf8_lossy(mac.name()).into_owned())?;
        dict.set_item("description", mac.description().to_string())?;
        dict.set_item("source", String::from_utf8_lossy(mac.source()).into_owned())?;
        vec.push(dict.into());
    }
    Ok(vec)
}

#[pymodule]
fn macrosia_glue(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(evaluate, m)?)?;
    m.add_function(wrap_pyfunction!(update_macros, m)?)?;
    m.add_function(wrap_pyfunction!(connect_to_db, m)?)?;
    m.add_function(wrap_pyfunction!(get_builtins, m)?)?;
    m.add(
        "PanicException",
        <pyo3::panic::PanicException as pyo3::PyTypeInfo>::type_object(m.py()),
    )?;
        m.add(
        "RustPanic",
        <pyo3_async_runtimes::err::RustPanic as pyo3::PyTypeInfo>::type_object(m.py()),
    )?;
    Ok(())
}

fn add_regexp_function(db: &Connection) -> rusqlite::Result<()> {
    db.create_scalar_function(
        "regexp",
        2,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        move |ctx| {
            if ctx.len() != 2 {
                return Err(rusqlite::Error::InvalidParameterCount(ctx.len(), 2));
            }
            let regexp: Arc<regex::Regex> = ctx.get_or_create_aux(
                0,
                |vr| -> Result<_, Box<dyn Error + Send + Sync + 'static>> {
                    Ok(regex::Regex::new(vr.as_str()?)?)
                },
            )?;
            let is_match = {
                let text = ctx
                    .get_raw(1)
                    .as_str()
                    .map_err(|e| rusqlite::Error::UserFunctionError(e.into()))?;

                regexp.is_match(text)
            };

            Ok(is_match)
        },
    )
}
