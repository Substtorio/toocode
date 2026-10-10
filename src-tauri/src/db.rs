//! SQLite 可视化视图的后端。
//!
//! ★ 它和 `searchdb.rs` 是**两件事**，别混：
//!   · `searchdb` 管的是「**我们**建的那个索引库」—— 连接常驻在 `IndexStore` 里，
//!     有增量同步、有字节上限
//!   · 这里管的是「**用户**想看的任意一个 SQLite 文件」—— 一次一个连接、
//!     用完就放，不缓存（用户可能随手翻好几个库，缓存反而是负担；而且那些
//!     文件可能正被别的程序写着 —— 常驻连接会让它更难被别的程序改）
//!
//! ★ 命令都很薄：开库 → 做一件事 → 交出结果。界面的事全在前端
//!
//! ⚠ 值一律转成**字符串**（`Option<String>`，`None` = NULL）再交出去：
//!   SQLite 是**动态类型**（同一列不同行可能是 int / text / blob），
//!   而前端只是显示。blob 不把字节流塞过去，只说它有多大

use rusqlite::Connection;
use serde::Serialize;
use std::path::Path;

/// 扫描数据库文件时要跳过的目录（和文件树 / 搜索那条规则同一个意思）
const IGNORED_DIRS: [&str; 8] = [
    "node_modules",
    ".git",
    "target",
    "dist",
    ".venv",
    "__pycache__",
    ".next",
    "build",
];

/// 扫描最多往下几层 —— 数据库文件不会埋得很深，而无限递归在大仓库上很贵
const MAX_SCAN_DEPTH: usize = 4;

/// 认得的数据库扩展名
const DB_EXTENSIONS: [&str; 3] = ["db", "sqlite", "sqlite3"];

/// 数行数时最多数到多少。
///
/// ★ 为什么不直接 `SELECT count(*)`：一张千万行的表会把界面卡死，
///   而我们只需要「大概多少」来算分页 —— 数到上限就停（见 `count_capped`）
const COUNT_CAP: i64 = 100_000;

// ============================ 交出去的类型 ============================

/// 一个数据库文件的概况
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DbInfo {
    /// 绝对路径（前端拿它去调别的命令）
    path: String,
    /// 文件名（显示用）
    name: String,
    /// 文件字节数
    size: u64,
    /// SQLite 自己的版本号 —— 顺带证明「它真的是个库」
    version: String,
    /// 里面有哪些表 / 视图
    tables: Vec<DbTable>,
}

/// 一个表 / 视图
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DbTable {
    name: String,
    /// `table` / `view` / `virtual`（FTS5 那种虚表）
    kind: String,
    /// 建表语句（关系图里能直接看到，不用再查一次）
    sql: String,
}

/// 一张表的结构
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DbSchema {
    table: String,
    /// 建表语句原文
    sql: String,
    columns: Vec<DbColumn>,
    indexes: Vec<DbIndex>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DbColumn {
    name: String,
    /// 声明类型（SQLite 里只是个「亲和性」，可能是空的）
    r#type: String,
    /// 1 = 非空、0 = 可空、None = 没写
    not_null: Option<i64>,
    /// 主键的**次序**（复合主键时 > 1）—— 0 表示不是主键
    primary_key: i64,
    /// 默认值表达式
    default_value: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DbIndex {
    name: String,
    /// 1 = 唯一索引
    unique: bool,
    /// 它索引了哪些列
    columns: Vec<String>,
}

/// 一页数据
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DbRows {
    columns: Vec<String>,
    /// 每个格子：`None` = SQL，其它是已经转成字符串的值
    rows: Vec<Vec<Option<String>>>,
    offset: i64,
    /// 总行数（**被 `COUNT_CAP` 截过**）
    total: i64,
    /// 是不是「数到上限就停了」—— 是的话界面上显示成「100000+」
    total_capped: bool,
}

/// 任意 SQL 的结果
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DbQueryResult {
    /// 返回了结果集的语句（SELECT / PRAGMA）才有列
    columns: Vec<String>,
    rows: Vec<Vec<Option<String>>>,
    /// 写操作影响了几行（读操作恒为 0）
    changes: i64,
    /// 结果集被 `limit` 截断了没有
    truncated: bool,
    /// 花了多少毫秒 —— 敲 SQL 的人关心这个
    elapsed_ms: u64,
}

/// 扫到的数据库文件
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DbFileEntry {
    path: String,
    name: String,
    size: u64,
    /// 最后修改时间（毫秒时间戳，0 = 取不到）
    modified_ms: u64,
}

// ============================ 内部工具 ============================

/// 打开一个库并确认它**真的是** SQLite。
///
/// ★ 为什么要探一下：用户可能随手点一个同名的二进制文件（`.db` 并不专属于
///   SQLite）。不探的话会在后面某一步报出一句莫名其妙的「file is not a database」，
///   而那时已经不知道是哪一步的问题了
///
/// ⚠ ★★ **探测语句不能用 `SELECT sqlite_version()`**：SQLite 是惰性打开的，
///   而那个查询不需要读任何数据页 —— 対一个纯文本文件它**也会成功**
///   （把那个文件当成一个空库）。必须去碰一下目录表才会真的读文件头
fn open(path: &str) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|err| format!("打开失败：{err}"))?;
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |row| {
        row.get::<_, i64>(0)
    })
    .map_err(|_| format!("{path} 看起来不是 SQLite 数据库"))?;
    Ok(conn)
}

/// 把标识符（表名 / 列名）安全地塞进 SQL。
///
/// ⚠ 表名**不能参数化**（SQL 不允许），只能拼 —— 所以里面的双引号必须转义成
///   两个双引号。名字虽然来自 `sqlite_master`（算是可信），但「拼 SQL 的字符串
///   一定要过转义」这条规矩不该有例外：今天可信的来源，明天就可能变成用户输入
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// 把一格的值转成字符串
fn cell_to_string(value: rusqlite::types::ValueRef<'_>) -> Option<String> {
    use rusqlite::types::ValueRef;
    match value {
        ValueRef::Null => None,
        ValueRef::Integer(number) => Some(number.to_string()),
        ValueRef::Real(number) => Some(number.to_string()),
        ValueRef::Text(bytes) => Some(String::from_utf8_lossy(bytes).to_string()),
        // ★ blob 不往下传字节流（可能是图片、可能是几 MB 的二进制）——
        //   只说它多大，用户知道「这里有个二进制」就够了
        ValueRef::Blob(bytes) => Some(format!("<blob {} 字节>", bytes.len())),
    }
}

/// 数一张表有多少行，**数到上限就停**。
///
/// ★ 写法要点：`COUNT_CAP + 1` 是为了能分辨「刚好等于上限」和「超过上限」。
///   `LIMIT` 在外面那层是没用的（`count(*)` 只返回一行），必须套在子查询里
fn count_capped(conn: &Connection, table: &str) -> (i64, bool) {
    let sql = format!(
        "SELECT count(*) FROM (SELECT 1 FROM {} LIMIT {})",
        quote_ident(table),
        COUNT_CAP + 1
    );
    match conn.query_row(&sql, [], |row| row.get::<_, i64>(0)) {
        Ok(total) if total > COUNT_CAP => (COUNT_CAP, true),
        Ok(total) => (total, false),
        // 数不出来（视图可能报错、虚表可能不支持）就当 0，不影响看数据
        Err(_) => (0, false),
    }
}

/// 按位置读一页，每行都转成字符串
fn read_rows(
    conn: &Connection,
    sql: &str,
    limit: i64,
) -> Result<(Vec<String>, Vec<Vec<Option<String>>>), String> {
    let mut statement = conn.prepare(sql).map_err(|err| err.to_string())?;

    let columns: Vec<String> = statement
        .column_names()
        .iter()
        .map(|name| name.to_string())
        .collect();

    let column_count = columns.len();
    let mut rows = Vec::new();

    let mut result = statement.query([]).map_err(|err| err.to_string())?;
    while let Some(row) = result.next().map_err(|err| err.to_string())? {
        let mut line = Vec::with_capacity(column_count);
        for index in 0..column_count {
            let value = row.get_ref(index).map_err(|err| err.to_string())?;
            line.push(cell_to_string(value));
        }
        rows.push(line);
        if rows.len() as i64 >= limit {
            break;
        }
    }

    Ok((columns, rows))
}

/// 判断一个文件的扩展名算不算数据库
fn looks_like_db(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            let lower = ext.to_ascii_lowercase();
            DB_EXTENSIONS.contains(&lower.as_str())
        })
}

/// 拿文件的最后修改时间（毫秒）。取不到就当 0 —— 排序用，不值得为它报错
fn modified_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|delta| delta.as_millis() as u64)
        .unwrap_or(0)
}

// ============================ 命令 ============================

/// 在工作区里找数据库文件。
///
/// ★ 为什么要有它：用户不会记得自己的 `.db` 放在哪一层 ——
///   树里一路点开去找，比自己列出来麻烦得多
/// ⚠ 只扫到 `MAX_SCAN_DEPTH` 层，而且跳过 node_modules / target 那些 ——
///   不然一个大仓库能扫出几万个文件
#[tauri::command]
pub fn db_find_files(root: String) -> Result<Vec<DbFileEntry>, String> {
    let mut found: Vec<DbFileEntry> = Vec::new();
    let mut stack = vec![(std::path::PathBuf::from(&root), 0usize)];

    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            if path.is_dir() {
                if depth < MAX_SCAN_DEPTH && !IGNORED_DIRS.contains(&name.as_str()) {
                    stack.push((path, depth + 1));
                }
                continue;
            }

            if !looks_like_db(&path) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            found.push(DbFileEntry {
                path: path.to_string_lossy().replace('\\', "/"),
                name,
                size: meta.len(),
                modified_ms: modified_ms(&meta),
            });
        }
    }

    // 大的排前面 —— 通常「内容多的那个」才是用户想看的
    found.sort_by(|a, b| b.size.cmp(&a.size));
    Ok(found)
}

/// 列出**我们自己建的**索引库（`appDataDir/index/*.db`）。
///
/// ★ 为什么单开一个命令：那些库不在工作区里，`db_find_files` 扫不到它们 ——
///   而它们恰恰是「手边就有真数据」的那一份（实测这个项目的工作区里
///   **一个 `.db` 都没有**）。少了这个入口，数据库视图就是一片空白
///
/// ⚠ 只认主库文件：`x.db` 旁边的 `x.db-wal` / `x.db-shm` 不会被列出来 ——
///   它们的 `extension()` 是 `db-wal` 而不是 `db`，`looks_like_db` 天然挡住了
///   （这一步不能省：`-wal` 有时候比主库还大，列出来会很莫名其妙）
#[tauri::command]
pub fn db_index_files(app: tauri::AppHandle) -> Result<Vec<DbFileEntry>, String> {
    use tauri::Manager;
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|err| err.to_string())?
        .join("index");

    let mut found: Vec<DbFileEntry> = Vec::new();
    // ★ 目录不存在 = 还没建过索引，这是**正常状态**不是错误
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(found);
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !looks_like_db(&path) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        found.push(DbFileEntry {
            path: path.to_string_lossy().replace('\\', "/"),
            name: entry.file_name().to_string_lossy().to_string(),
            size: meta.len(),
            modified_ms: modified_ms(&meta),
        });
    }

    // 最近改动的排前面（活动的工作区就是最近改过的那一个）
    found.sort_by(|a, b| b.modified_ms.cmp(&a.modified_ms));
    Ok(found)
}

/// 打开一个库，列出里面的表和视图
#[tauri::command]
pub fn db_open(path: String) -> Result<DbInfo, String> {
    let conn = open(&path)?;

    let version = conn
        .query_row("SELECT sqlite_version()", [], |row| row.get::<_, String>(0))
        .unwrap_or_else(|_| "?".to_string());

    // ★ `sqlite_master` 是 SQLite 的「目录表」—— 表、索引、视图、触发器都在里面。
    //   按 `name` 排序（而不是按 create 顺序）：名字稳定，列表才不会每次打开都换位置
    let mut statement = conn
        .prepare("SELECT name, type, ifnull(sql, '') FROM sqlite_master WHERE type IN ('table', 'view') ORDER BY name")
        .map_err(|err| err.to_string())?;

    let tables = statement
        .query_map([], |row| {
            let name: String = row.get(0)?;
            let kind: String = row.get(1)?;
            let sql: String = row.get(2)?;
            Ok(DbTable {
                // FTS5 的表在 sqlite_master 里也是 type='table'，
                // 但建表语句里有 `USING fts5` —— 单独标出来，界面上好看一些
                kind: if sql.to_ascii_lowercase().contains("using fts5") {
                    "virtual".to_string()
                } else {
                    kind
                },
                name,
                sql,
            })
        })
        .map_err(|err| err.to_string())?
        .flatten()
        .collect::<Vec<_>>();

    let size = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    let name = Path::new(&path)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.clone());

    Ok(DbInfo {
        path,
        name,
        size,
        version,
        tables,
    })
}

/// 一张表的结构：列、索引、建表语句
#[tauri::command]
pub fn db_schema(path: String, table: String) -> Result<DbSchema, String> {
    let conn = open(&path)?;

    let sql: String = conn
        .query_row(
            "SELECT ifnull(sql, '') FROM sqlite_master WHERE name = ?1",
            [&table],
            |row| row.get(0),
        )
        .unwrap_or_default();

    // ★ `PRAGMA table_info` 是**唯一**能拿到列信息的方式（列不是 sqlite_master 里的行）。
    //   ⚠ PRAGMA 的参数同样不能参数化 ⇒ 只能拼表名，所以过了 quote_ident
    let mut statement = conn
        .prepare(&format!("PRAGMA table_info({})", quote_ident(&table)))
        .map_err(|err| err.to_string())?;
    let columns = statement
        .query_map([], |row| {
            Ok(DbColumn {
                name: row.get(1)?,
                r#type: row.get(2)?,
                not_null: row.get(3).ok(),
                default_value: row.get(4).ok(),
                primary_key: row.get(5).unwrap_or(0),
            })
        })
        .map_err(|err| err.to_string())?
        .flatten()
        .collect::<Vec<_>>();

    // ⚠ 索引那一步失败不该让整张表看不了（虚表就不一定有常规索引）
    let mut indexes: Vec<DbIndex> = Vec::new();
    if let Ok(mut stmt) = conn.prepare(&format!("PRAGMA index_list({})", quote_ident(&table))) {
        let listed = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, i64>(2).unwrap_or(0) != 0))
            })
            .map(|rows| rows.flatten().collect::<Vec<_>>())
            .unwrap_or_default();

        for (index_name, unique) in listed {
            let mut columns_of_index: Vec<String> = Vec::new();
            if let Ok(mut info) = conn.prepare(&format!(
                "PRAGMA index_info({})",
                quote_ident(&index_name)
            )) {
                columns_of_index = info
                    .query_map([], |row| row.get::<_, String>(2))
                    .map(|rows| rows.flatten().collect::<Vec<_>>())
                    .unwrap_or_default();
            }
            indexes.push(DbIndex {
                name: index_name,
                unique,
                columns: columns_of_index,
            });
        }
    }

    Ok(DbSchema {
        table,
        sql,
        columns,
        indexes,
    })
}

/// 读一页数据
///
/// ★ 分页靠 `LIMIT / OFFSET` 而不是把整张表读进来 ——
///   一张百万行的表读进来会直接把前端撑爆
#[tauri::command]
pub fn db_rows(path: String, table: String, offset: i64, limit: i64) -> Result<DbRows, String> {
    let conn = open(&path)?;
    let limit = limit.clamp(1, 500);

    let sql = format!(
        "SELECT * FROM {} LIMIT {} OFFSET {}",
        quote_ident(&table),
        limit,
        offset.max(0)
    );
    let (columns, rows) = read_rows(&conn, &sql, limit)?;
    let (total, total_capped) = count_capped(&conn, &table);

    Ok(DbRows {
        columns,
        rows,
        offset: offset.max(0),
        total,
        total_capped,
    })
}

/// 执行任意 SQL。
///
/// ⚠ **这是唯一一个有副作用的命令** —— 用户可以在里面 `DROP TABLE`。
///   界面那边有提示，但这里不拦：要的就是「能跑任意 SQL」这个能力，
///   拦一半（比如只禁 DROP）反而让人以为「不能写」，然后去别处找开关
/// ★ 返回什么取决于语句类型：`SELECT` / `PRAGMA` 有结果集，
///   `INSERT` / `UPDATE` / `CREATE` 只有影响行数 —— 两个都填上，让前端自己判断
#[tauri::command]
pub fn db_query(path: String, sql: String, limit: i64) -> Result<DbQueryResult, String> {
    let conn = open(&path)?;
    let limit = limit.clamp(1, 500);
    let started = std::time::Instant::now();

    // ★ 先试「有结果集」那条路。`prepare` 对任何语句都成功，
    //   所以判断「有没有结果集」只能看 `column_count()` —— 它是 0 就说明
    //   这是个写操作（或者不返回行的 DDL）
    let mut statement = conn.prepare(&sql).map_err(|err| err.to_string())?;
    let column_count = statement.column_count();

    if column_count == 0 {
        // 写操作：直接 execute（它内部会开事务并提交）
        let changes = statement.execute([]).map_err(|err| err.to_string())?;
        return Ok(DbQueryResult {
            columns: Vec::new(),
            rows: Vec::new(),
            changes: changes as i64,
            truncated: false,
            elapsed_ms: started.elapsed().as_millis() as u64,
        });
    }

    let (columns, rows) = read_rows(&conn, &sql, limit)?;
    Ok(DbQueryResult {
        truncated: rows.len() as i64 >= limit,
        columns,
        rows,
        changes: 0,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个临时库文件的路径。
    /// ★ 名字里带上纳秒时间戳 —— 测试是并行跑的，固定名字会互相撞
    fn temp_path(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|delta| delta.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("toocode-db-test-{tag}-{nanos}.db"))
    }

    fn as_string(path: &std::path::Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }

    #[test]
    fn quote_ident_escapes_double_quotes() {
        // ⚠ 表名进不了绑定参数，只能拼进 SQL —— 而这个名字里可能有双引号。
        //   不转义的话拼出来的 SQL 直接坏掉（索引库那些名字当然没问题，
        //   但「拼 SQL 一定要过转义」这条规矩不该对任何来源开例外）
        assert_eq!(quote_ident("files"), "\"files\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn looks_like_db_ignores_wal_and_shm() {
        assert!(looks_like_db(Path::new("D:/x/a.db")));
        assert!(looks_like_db(Path::new("D:/x/a.SQLITE")));
        assert!(looks_like_db(Path::new("D:/x/a.sqlite3")));
        // ★ 这条是重点：`x.db-wal` 的扩展名是 `db-wal` 而不是 `db`。
        //   不挡的话侧栏里会多出两个看不懂的条目 ——
        //   而且 `-wal` 有时候比主库还大
        assert!(!looks_like_db(Path::new("D:/x/a.db-wal")));
        assert!(!looks_like_db(Path::new("D:/x/a.db-shm")));
        assert!(!looks_like_db(Path::new("D:/x/notes.txt")));
    }

    #[test]
    fn rejects_a_file_that_is_not_sqlite() {
        let path = temp_path("nope");
        std::fs::write(&path, b"this is definitely not a database").unwrap();

        // ★ 为什么值得测：SQLite **惰性打开** —— `Connection::open` 成功、
        //   连 `SELECT sqlite_version()` 都成功。探测一旦写成那个，
        //   一个纯文本文件会显示成「0 张表的空库」，而用户以为它坏了
        let err = match db_open(as_string(&path)) {
            Ok(_) => panic!("一个纯文本文件不该被当成数据库"),
            Err(err) => err,
        };
        assert!(err.contains("不是 SQLite"), "错误信息不对：{err}");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn count_capped_stops_at_the_cap() {
        let small = temp_path("cap-small");
        let conn = Connection::open(&small).unwrap();
        conn.execute_batch("CREATE TABLE t(x); INSERT INTO t(x) VALUES (1), (2), (3);")
            .unwrap();
        assert_eq!(count_capped(&conn, "t"), (3, false));

        // ★ 超过上限时必须返回「上限值 + capped 标记」。数出 100000 却不带标记的话，
        //   分页会算成「一共 100000 行」，而后面其实还有数据 ——
        //   用户翻不到最后几页，而且看不出来是为什么
        let big = temp_path("cap-big");
        let conn2 = Connection::open(&big).unwrap();
        conn2
            .execute_batch(
                "CREATE TABLE t(x);
                 WITH RECURSIVE seq(n) AS (
                     SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 100005
                 )
                 INSERT INTO t(x) SELECT n FROM seq;",
            )
            .unwrap();
        assert_eq!(count_capped(&conn2, "t"), (COUNT_CAP, true));

        drop(conn);
        drop(conn2);
        let _ = std::fs::remove_file(&small);
        let _ = std::fs::remove_file(&big);
    }

    #[test]
    fn reads_schema_rows_and_runs_arbitrary_sql() {
        let path = temp_path("e2e");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT, extra BLOB);
                 INSERT INTO notes(body) VALUES ('hello'), ('world'), (NULL);
                 CREATE INDEX notes_body ON notes(body);",
            )
            .unwrap();
        }
        let file = as_string(&path);

        let info = db_open(file.clone()).unwrap();
        assert_eq!(info.tables.len(), 1);
        assert_eq!(info.tables[0].name, "notes");
        assert_eq!(info.tables[0].kind, "table");
        assert!(!info.version.is_empty());

        let schema = db_schema(file.clone(), "notes".to_string()).unwrap();
        assert_eq!(schema.columns.len(), 3);
        // 界面靠 `primary_key > 0` 打 PK 标记，靠 `indexes` 画关系图的右列
        assert!(schema.columns[0].primary_key > 0);
        assert_eq!(schema.indexes.len(), 1);
        assert_eq!(schema.indexes[0].columns, vec!["body"]);

        // 分页：limit=2 只要 2 行，但 total 要说全（3 行）
        let page = db_rows(file.clone(), "notes".to_string(), 0, 2).unwrap();
        assert_eq!(page.columns, vec!["id", "body", "extra"]);
        assert_eq!(page.rows.len(), 2);
        assert_eq!(page.total, 3);
        assert!(!page.total_capped);

        // ⚠ 不能断言「第 3 行是哪个」—— 没写 ORDER BY 时 SQLite 给的顺序不定（
        //   正好是没写 ORDER BY 时该有的行为）。只数 NULL 有几个
        let all = db_rows(file.clone(), "notes".to_string(), 0, 10).unwrap();
        assert_eq!(all.rows.len(), 3);
        assert_eq!(all.rows.iter().filter(|row| row[1].is_none()).count(), 1);

        // 写操作：没有结果集，但要报「影响了几行」
        let changed = db_query(file.clone(), "UPDATE notes SET body = 'x'".to_string(), 10).unwrap();
        assert!(changed.columns.is_empty());
        assert_eq!(changed.changes, 3);

        // 读操作：有结果集，changes 恒为 0
        let selected = db_query(file.clone(), "SELECT body FROM notes".to_string(), 10).unwrap();
        assert_eq!(selected.columns, vec!["body"]);
        assert_eq!(selected.rows.len(), 3);
        assert_eq!(selected.changes, 0);

        // 语法错要让调用方拿到错误（而不是静默返回空结果）
        assert!(db_query(file, "SELECT FROM".to_string(), 10).is_err());

        let _ = std::fs::remove_file(&path);
    }
}
