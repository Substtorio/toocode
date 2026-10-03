//! 搜索索引（SQLite）。
//!
//! ★ 为什么要一个索引：见文档里「搜索」那一节的实测 —— 原来的实现是
//!   「把每个文件完整读一遍再逐行 `find`」，所以**每次敲键的成本和查什么词无关**。
//!   110 个文件要 ~100ms；5850 个文件 / 906MB 要 ~2.8s（查一个根本不存在的词也一样慢）。
//!   索引把「查询」从 O(全仓库字节数) 变成 O(命中数)，代价是首次建索引要全扫一遍。
//!
//! ★ 分工（这条是整个设计的地基）：
//!   **索引只回答「哪些文件命中」，行号 / 列号 / 高亮仍然由 `search_in_text` 去算。**
//!   为什么这么分：位置口径那一套（UTF-16 单位、前导空白偏移、超长行截断、emoji
//!   补充平面）已经在 `search_in_text` 里调好并测过了；索引只做**粗筛**，
//!   精算沿用旧代码，就少一处将来会不一致的地方。
//!
//! ★★ 为什么用 FTS5 的 **trigram** 分词器，而不是默认的 `unicode61`：
//!   默认分词器按**词**索引 —— `foo` 匹配不到 `foobar`。而这里的搜索是「包含」语义
//!   （和 `str::find` 一致，用户也是这么期待的）。trigram 把每 3 个连续字符都当索引项，
//!   于是 `LIKE '%oob%'` 这种「子串 + 前后都有通配符」的查询也能走索引。
//!   代价两条，都已实测确认：
//!     · 索引体积可能是原文的 1~2 倍
//!     · 查询词短于 3 个字符时提取不出三元组，只能退化成扫全表
//!       ⇒ 那种查询**直接交给旧路径**（见 `is_indexable`），行为完全一致
//!
//! ★★ 结果为什么永远是对的（哪怕索引还没建完 / 根本建不出来）：
//!   大仓库的索引是**渐进**建起来的（每次同步只花 `SYNC_BUDGET_MS` 毫秒），
//!   没建好的那些文件仍然走旧的现场扫描。所以：
//!     · 索引没建好 → 走得慢，但结果和以前一模一样
//!     · 索引建好了 → 只读命中的那几个文件，快
//!   两条路的文件并进同一个结果集，不存在「索引不全所以搜不到」这种状态。

use rusqlite::{params, Connection};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::collect_search_files;

/// 查询词短于这么多个**连续**字符就别用索引了。
/// ★ trigram 的一个「字」是 3 个连续字符 —— 短于它时 FTS5 提取不出任何三元组，
///   只能扫整张 fts 表（那比走旧路径还贵）。所以那种查询直接交给旧路径
const MIN_INDEXED_CHARS: usize = 3;

/// 一次同步最多花这么多毫秒去建内容索引。
/// ★ 有预算才不会「第一次搜索卡住好几秒」：索引是渐进建起来的，
///   期间没建好的文件走旧路径，结果始终是对的
const SYNC_BUDGET_MS: u128 = 250;

/// 单次同步最多处理多少个待索引文件（时间预算之外的第二个保险）
const SYNC_BATCH_FILES: usize = 512;

/// 距上次 stat 扫描不到这么久就跳过扫描。
/// ★ 打字时每敲一下都全盘 stat 一遍太浪费；这个延迟用户感觉不到
///   （保存文件之后前端会调 `index_invalidate`，那次一定是重新扫的）
const SYNC_TTL_MS: u128 = 800;

/// 内容索引的总字节上限（算的是**源码字节**，不是库文件大小）。
///
/// ★★ 为什么要封顶：本机实测索引体积大约是**原文的 3~7 倍** ——
///   FTS5 的 trigram 索引自身就比原文大（它把每个三连字符都当一项）。
///   不封顶的话一个大仓库能长出好几 GB 的库文件。
///   超出去的文件标记成 `indexed = 2`（**决定不索引**），
///   它们仍然走旧的现场扫描 —— 结果不会少，只是那部分慢
/// ★ 「到顶」和「还没建到」必须分开：混在一起的话 `pending` 永远不为 0，
///   前端的进度会一直转、后台线程也停不下来
const MAX_INDEX_BYTES: i64 = 128 * 1024 * 1024;

/// 符号搜索一次最多返回多少条
const DEFAULT_SYMBOL_LIMIT: usize = 300;

/// 文件名搜索一次最多返回多少条。
/// ★ 它只是**候选集**：真正的排序在前端（`fuzzy.ts` 那套打分）。
///   截断要有个上限，不然一个十万文件的大仓库光序列化就能把前端卡住
const DEFAULT_FILE_LIMIT: usize = 2000;

/// `files.indexed` 的取值
const PENDING: i64 = 0; // 待索引
const DONE: i64 = 1; // 已经进索引了
const SKIPPED: i64 = 2; // 超出上限，决定不索引（每次搜索现场扫）

// ============================ 索引本体 ============================

pub struct Index {
    conn: Connection,
    /// LIKE 到底有没有被下推进 FTS5 虚表（开库时探一次）。
    /// 探不到就把整个索引标成不可用，让搜索安静地退回旧路径
    usable: bool,
    last_sync_ms: u128,
    /// 磁盘上的东西变过了（保存文件 / 切 git 分支），下次搜索必须重新扫
    dirty: bool,
    /// 内容索引的字节上限。放成字段而不是直接用常量 —— 用例要把它调得很小
    max_bytes: i64,
    /// 上次同步的结果，TTL 内直接复用
    last: IndexInfo,
}

/// 一条符号命中（「转到符号」用）
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolRow {
    pub path: String,
    pub name: String,
    /// LSP 的 SymbolKind 编号
    pub kind: i64,
    pub line: usize,
    pub column: usize,
}

/// 索引的状态 / 一次同步的统计。
/// ★ 和 `SearchResponse` 一样用 camelCase —— 搜索这一族的返回值都是这个口径，
///   别在同一族类型里换一套（后端要自己 `rename_all`，Tauri 不会帮你转返回值）
#[derive(Serialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct IndexInfo {
    /// 索引里的文件总数
    pub files: usize,
    /// 其中内容已经建好的
    pub indexed: usize,
    /// 还没建好内容的（这些每次搜索都要现场扫）
    pub pending: usize,
    /// 超出上限、决定不索引的（同样每次搜索现场扫，但**不会**再减少）
    pub skipped: usize,
    /// 这次同步里新建 / 重建了几个
    pub updated: usize,
    /// 这次同步里删掉了几个
    pub removed: usize,
    /// 这次同步花了多少毫秒
    pub took_ms: u64,
    /// 库文件多大（字节）
    pub db_bytes: u64,
    /// 已进索引的源码字节数。拿它和 `db_bytes` 一比就知道索引膨胀了多少
    pub indexed_bytes: i64,
    /// 索引是不是真的能用了（false = 一直在走旧路径）
    pub built: bool,
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// 特征串（去尾斜杠 + 转小写）。会话表拿它当 key，哈希也拿它当输入
pub fn root_key(root: &str) -> String {
    root.trim_end_matches(['\\', '/']).to_lowercase()
}

/// 工作区路径 → 库文件名。
/// ★ 用哈希而不是把路径编码进文件名：路径里的 `:` `\` 中文都不适合当文件名，
///   而且很容易超过文件名长度上限。转小写是因为 Windows 路径不区分大小写 ——
///   同一个文件夹用两种写法打开时应该落到同一个库
/// ★ 先过一遍 `root_key`：不然 `D:\proj` 和 `D:\proj\` 会得到两个哈希、
///   两个库，同一个文件夹白建两遍索引（这个坑是用例捅出来的）
fn hash_root(root: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in root_key(root).as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// 索引库放哪儿：`appDataDir/index/<工作区哈希>.db`
///
/// ★ 为什么不放在工作区里（比如 `.toocode/index.db`）：那会往用户仓库里塞东西，
///   还得让每个仓库都记得写 `.gitignore`。放应用数据目录没有这些麻烦
/// ★ 代价：文件夹搬走 / 改名之后要重建索引（哈希变了，等于新库）。可接受
pub fn db_path(app: &tauri::AppHandle, root: &str) -> Result<PathBuf, String> {
    use tauri::Manager;
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|err| format!("拿不到应用数据目录：{err}"))?;
    let dir = dir.join("index");
    std::fs::create_dir_all(&dir).map_err(|err| format!("建索引目录失败：{err}"))?;
    Ok(dir.join(format!("{}.db", hash_root(root))))
}

/// 建表。以后加表 / 加列都从这里加（全是 `IF NOT EXISTS`，老库直接升级）
fn migrate(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        r#"
        -- 文件清单：stat 差集用。不存内容，所以它很小
        CREATE TABLE IF NOT EXISTS files(
            id      INTEGER PRIMARY KEY,   -- 同时就是 fts 的 rowid
            path    TEXT    NOT NULL UNIQUE,
            mtime   INTEGER NOT NULL,
            size    INTEGER NOT NULL,
            -- 内容是否已经进过索引。
            -- ★ 「读不出来 / 不是 UTF-8」也算 1：那种文件本来就不该被搜到，
            --   不标记的话每次同步都会再读一遍，白费力气
            indexed INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS files_pending ON files(indexed) WHERE indexed = 0;

        -- 内容索引。path 是 UNINDEXED 的 —— 它只是跟着行一起存回来，不该进索引，
        -- 那样既浪费空间又会让词表里混进一堆路径片段
        CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5(
            path UNINDEXED,
            body,
            tokenize = 'trigram'
        );

        -- 符号。「转到符号」（Ctrl+T）用
        -- ★ kind 用的是 **LSP 的 SymbolKind 编号**：将来打开过的文件改用
        --   语言服务器的 `documentSymbol` 覆盖掉这里的粗糙结果时，编号不用换算
        CREATE TABLE IF NOT EXISTS symbols(
            path TEXT    NOT NULL,
            name TEXT    NOT NULL,
            kind INTEGER NOT NULL,
            line INTEGER NOT NULL,
            col  INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS symbols_name ON symbols(name);
        CREATE INDEX IF NOT EXISTS symbols_path ON symbols(path);
        "#,
    )
    .map_err(|err| format!("建索引表失败：{err}"))
}

/// 探一次：`LIKE` 到底有没有被下推进 FTS5 虚表。
///
/// ★★ 这是**优化器的行为**，不是语法保证的 —— 所以要用一个探针把它变成事实。
///   探不到就把索引标成不可用、安静地退回旧路径；
///   不探的话，升级 SQLite 之后可能悄悄退化成逐行扫，而搜索「结果还对，只是慢几百倍」
///
/// ⚠ 探针必须**和真查询长得一模一样**（字面量 + ESCAPE），否则探了个寂寞：
///   实测把模式写成绑定参数 `?1` 时，计划里就没有 `VIRTUAL TABLE INDEX` 了
fn like_uses_index(conn: &Connection) -> bool {
    let sql = format!(
        "EXPLAIN QUERY PLAN SELECT path FROM fts WHERE body LIKE {} ESCAPE '\\'",
        sql_literal("%toocode%")
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return false;
    };
    let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(3)) else {
        return false;
    };
    let found = rows
        .flatten()
        .any(|line| line.contains("VIRTUAL TABLE INDEX"));
    found
}

impl Index {
    pub fn open(db: &Path) -> Result<Self, String> {
        let conn = Connection::open(db).map_err(|err| format!("打开索引库失败：{err}"))?;
        // WAL：批量写入快得多，而且读不会被写挡住
        let _ = conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;");
        migrate(&conn)?;
        let usable = like_uses_index(&conn);
        if !usable {
            println!("[搜索索引] LIKE 没有走 FTS5 索引，这次不启用索引（搜索走旧的现场扫描）");
        }
        Ok(Self {
            conn,
            usable,
            last_sync_ms: 0,
            dirty: true,
            max_bytes: MAX_INDEX_BYTES,
            last: IndexInfo::default(),
        })
    }

    #[cfg(test)]
    fn memory() -> Self {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let usable = like_uses_index(&conn);
        Self {
            conn,
            usable,
            last_sync_ms: 0,
            dirty: true,
            max_bytes: MAX_INDEX_BYTES,
            last: IndexInfo::default(),
        }
    }

    pub fn is_usable(&self) -> bool {
        self.usable
    }

    /// 标记「磁盘变过了」，下一次搜索一定会重新扫一遍。
    /// 保存文件之后前端会调这个 —— 否则刚存的内容要等 TTL 过期才搜得到
    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    /// 只读状态，不碰磁盘
    pub fn info(&self) -> IndexInfo {
        let count = |sql: &str| -> i64 {
            self.conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0)
        };
        let files = count("SELECT count(*) FROM files").max(0) as usize;
        let pending = count("SELECT count(*) FROM files WHERE indexed = 0").max(0) as usize;
        let skipped = count("SELECT count(*) FROM files WHERE indexed = 2").max(0) as usize;
        let indexed_bytes =
            count("SELECT coalesce(sum(size), 0) FROM files WHERE indexed = 1");
        let db_bytes = self
            .conn
            .query_row("PRAGMA page_count", [], |r| r.get::<_, i64>(0))
            .ok()
            .zip(
                self.conn
                    .query_row("PRAGMA page_size", [], |r| r.get::<_, i64>(0))
                    .ok(),
            )
            .map(|(pages, size)| (pages * size) as u64)
            .unwrap_or(0);
        IndexInfo {
            files,
            indexed: files.saturating_sub(pending).saturating_sub(skipped),
            pending,
            skipped,
            // ★ 这两个取**上次同步**的结果，而不是写 0：
            //   不然调 index_info 永远看不到「上一轮识别出几个变化」，
            //   排查「为什么索引一直在重建」时就没抓手了
            updated: self.last.updated,
            removed: self.last.removed,
            took_ms: self.last.took_ms,
            db_bytes,
            indexed_bytes,
            built: self.usable,
        }
    }

    /// 把磁盘现状同步进索引，并在预算内往前推一批内容索引。
    ///
    /// 四步：① stat 扫描全树 ② 差集出「新增 / 变了 / 没了」 ③ 预算内读文件建内容 ④ 汇总
    pub fn sync(&mut self, root: &Path, force: bool) -> Result<IndexInfo, String> {
        let now = now_ms();
        if !force && !self.dirty && now.saturating_sub(self.last_sync_ms) < SYNC_TTL_MS {
            // TTL 内：省掉整棵树的 stat 扫描，只把 pending 现算一遍
            let mut cached = self.info();
            cached.updated = 0;
            cached.removed = 0;
            cached.took_ms = 0;
            return Ok(cached);
        }

        let started = now_ms();
        let mut entries = Vec::new();
        collect_search_files(root, &mut entries, 0);

        // 库里的现状：path → (id, mtime, size)
        let mut known: HashMap<String, (i64, i64, i64)> = HashMap::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT id, path, mtime, size FROM files")
                .map_err(|err| err.to_string())?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(1)?,
                        (
                            r.get::<_, i64>(0)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, i64>(3)?,
                        ),
                    ))
                })
                .map_err(|err| err.to_string())?;
            for row in rows.flatten() {
                known.insert(row.0, row.1);
            }
        }

        let mut updated = 0usize;
        let mut removed = 0usize;
        let mut seen: HashSet<String> = HashSet::with_capacity(entries.len());

        // ② 新增 / 变化 → 打回「待索引」
        {
            let tx = self.conn.transaction().map_err(|err| err.to_string())?;
            {
                let mut insert = tx
                    .prepare(
                        "INSERT OR IGNORE INTO files(path, mtime, size, indexed) VALUES (?1, ?2, ?3, ?4)",
                    )
                    .map_err(|err| err.to_string())?;
                let mut touch = tx
                    .prepare("UPDATE files SET mtime = ?2, size = ?3, indexed = 0 WHERE id = ?1")
                    .map_err(|err| err.to_string())?;
                for entry in &entries {
                    let path = entry.path.to_string_lossy().to_string();
                    match known.get(&path) {
                        None => {
                            insert
                                .execute(params![
                                    path,
                                    entry.mtime_ms,
                                    entry.size as i64,
                                    PENDING
                                ])
                                .map_err(|err| err.to_string())?;
                            updated += 1;
                        }
                        Some((id, mtime, size)) => {
                            if *mtime != entry.mtime_ms || *size != entry.size as i64 {
                                touch
                                    .execute(params![id, entry.mtime_ms, entry.size as i64])
                                    .map_err(|err| err.to_string())?;
                                updated += 1;
                            }
                        }
                    }
                    seen.insert(path);
                }
            }
            tx.commit().map_err(|err| err.to_string())?;
        }

        // ③ 没了的 → 删掉（连带 fts 里那行）
        {
            let gone: Vec<(i64, String)> = known
                .iter()
                .filter(|(path, _)| !seen.contains(path.as_str()))
                .map(|(path, (id, _, _))| (*id, path.clone()))
                .collect();
            if !gone.is_empty() {
                let tx = self.conn.transaction().map_err(|err| err.to_string())?;
                {
                    let mut del_fts = tx
                        .prepare("DELETE FROM fts WHERE rowid = ?1")
                        .map_err(|err| err.to_string())?;
                    let mut del_file = tx
                        .prepare("DELETE FROM files WHERE id = ?1")
                        .map_err(|err| err.to_string())?;
                    let mut del_symbols = tx
                        .prepare("DELETE FROM symbols WHERE path = ?1")
                        .map_err(|err| err.to_string())?;
                    for (id, path) in &gone {
                        del_fts.execute([id]).map_err(|err| err.to_string())?;
                        del_file.execute([id]).map_err(|err| err.to_string())?;
                        // ★ 文件没了，它的符号也得跟着走 —— 不然 Ctrl+T 会搜到一个
                        //   打不开的位置（而那种失败是「点了没反应」，不报错）
                        del_symbols
                            .execute(params![path])
                            .map_err(|err| err.to_string())?;
                        removed += 1;
                    }
                }
                tx.commit().map_err(|err| err.to_string())?;
            }
        }

        // ④ 预算内往前推一批内容索引
        self.build_pending(started)?;

        self.last_sync_ms = now_ms();
        self.dirty = false;
        let mut info = self.info();
        info.updated = updated;
        info.removed = removed;
        info.took_ms = now_ms().saturating_sub(started) as u64;
        self.last = info.clone();
        Ok(info)
    }

    /// 把「待索引」里的文件按预算往前推一批。
    ///
    /// ★ 正文存的是**小写化之后**的版本，这是刻意的：
    ///   SQLite 的 `LIKE` 只对 ASCII 做大小写折叠，而 Rust 的 `to_lowercase()` 是全
    ///   Unicode 的。两边都折一遍，索引和查询就落在同一个口径上。
    ///   顺带还照顾了 `case_sensitive = true` —— 小写匹配是大小写敏感匹配的**超集**，
    ///   索引只做粗筛，真正的大小写判断仍然由 `search_in_text` 做
    fn build_pending(&mut self, started: u128) -> Result<(), String> {
        let tx = self.conn.transaction().map_err(|err| err.to_string())?;
        {
            let pending: Vec<(i64, String, i64)> = {
                let mut stmt = tx
                    .prepare("SELECT id, path, size FROM files WHERE indexed = 0 LIMIT ?1")
                    .map_err(|err| err.to_string())?;
                let rows = stmt
                    .query_map([SYNC_BATCH_FILES as i64], |r| {
                        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
                    })
                    .map_err(|err| err.to_string())?;
                rows.flatten().collect()
            };

            // 已经装进去多少字节 —— 决定还能不能再装
            let mut used: i64 = tx
                .query_row(
                    "SELECT coalesce(sum(size), 0) FROM files WHERE indexed = 1",
                    [],
                    |r| r.get(0),
                )
                .map_err(|err| err.to_string())?;

            let mut del = tx
                .prepare("DELETE FROM fts WHERE rowid = ?1")
                .map_err(|err| err.to_string())?;
            let mut insert = tx
                .prepare("INSERT INTO fts(rowid, path, body) VALUES (?1, ?2, ?3)")
                .map_err(|err| err.to_string())?;
            let mut mark = tx
                .prepare("UPDATE files SET indexed = ?2 WHERE id = ?1")
                .map_err(|err| err.to_string())?;
            let mut del_symbols = tx
                .prepare("DELETE FROM symbols WHERE path = ?1")
                .map_err(|err| err.to_string())?;
            let mut insert_symbol = tx
                .prepare(
                    "INSERT INTO symbols(path, name, kind, line, col) VALUES (?1, ?2, ?3, ?4, ?5)",
                )
                .map_err(|err| err.to_string())?;

            for (id, path, size) in pending {
                if now_ms().saturating_sub(started) > SYNC_BUDGET_MS {
                    break;
                }

                // 读一次，符号和正文共用
                let text = std::fs::read(&path)
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok());

                // ★ 符号**不受内容上限管**：它只存「名字 + 行号」，几百字节一个，
                //   而 Ctrl+T 的价值就在于整个仓库都能找到符号。
                //   上限管的是正文索引的体积（那个会膨到源码的好几倍）
                del_symbols
                    .execute(params![path])
                    .map_err(|err| err.to_string())?;
                if let Some(text) = &text {
                    for symbol in scan_symbols(&path, text) {
                        insert_symbol
                            .execute(params![
                                path,
                                symbol.name,
                                symbol.kind,
                                symbol.line as i64,
                                symbol.column as i64
                            ])
                            .map_err(|err| err.to_string())?;
                    }
                }

                // 正文索引：超出上限就不进去（上面符号已经存好了）
                let over_cap = used + size > self.max_bytes;
                del.execute([id]).map_err(|err| err.to_string())?;
                if let Some(text) = text {
                    if !over_cap {
                        insert
                            .execute(params![id, path, text.to_lowercase()])
                            .map_err(|err| err.to_string())?;
                        used += size;
                    }
                }
                mark.execute(params![id, if over_cap { SKIPPED } else { DONE }])
                    .map_err(|err| err.to_string())?;
            }
        }
        tx.commit().map_err(|err| err.to_string())?;
        Ok(())
    }

    /// 把索引往前推一批（不扫磁盘，只管「待索引」那部分）。
    /// 后台补齐线程拿它当一轮（见 `warm`）
    pub fn build_more(&mut self) -> Result<usize, String> {
        self.build_pending(now_ms())?;
        Ok(self.info().pending)
    }

    /// 粗筛：哪些文件的内容里**可能**有这个词。
    /// ★ 返回的一定是超集（LIKE 只比小写的「包含」，不管大小写是否真的对得上），
    ///   精算交给 `search_in_text`
    ///
    /// ★★ 模式是**内联成字面量**的，不是绑定参数 —— 实测（`like_query_uses_the_trigram_index_not_a_scan`）
    ///   带 `?1` 时执行计划里没有 `VIRTUAL TABLE INDEX`，也就是**用不上 trigram 索引**。
    ///   原因很硬：三元组必须在**准备语句时**就提取出来，而参数的值要到执行时才知道。
    ///   代价是每次查询都得重新 prepare（几十微秒），和它换来的加速比不值一提
    /// ⚠ 所以 `sql_literal` 里的转义是**安全边界**，不能省
    pub fn candidate_paths(&self, query: &str) -> Result<Vec<String>, String> {
        let needle = like_pattern(&query.to_lowercase());
        let sql = format!(
            "SELECT path FROM fts WHERE body LIKE {} ESCAPE '\\' ORDER BY path",
            sql_literal(&needle)
        );
        let mut stmt = self.conn.prepare(&sql).map_err(|err| err.to_string())?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|err| err.to_string())?;
        Ok(rows.flatten().collect())
    }

    /// 按**文件名 / 路径**找文件（快速打开用的候选集）。
    ///
    /// ★ 这里刻意不用 FTS5：路径一共就几万条，一次 `LIKE` 全表扫也就几毫秒 ——
    ///   为它再养一张 trigram 索引不值得（那张索引会比路径本身大得多）。
    ///   内容搜索那边不同：正文是几百 MB，扫不起
    /// ★ 排序只用「路径短的在前」这个粗规则。真正该排的是「哪个更像用户要找的」，
    ///   而那是前端的模糊打分该干的事 —— 在 SQL 里再实现一遍只会两套结果不一致
    pub fn find_files(&self, query: &str, limit: usize) -> Result<Vec<String>, String> {
        let needle = like_pattern(&query.to_lowercase());
        let mut stmt = self
            .conn
            .prepare(
                "SELECT path FROM files WHERE lower(path) LIKE ?1 ESCAPE '\\' \
                 ORDER BY length(path), path LIMIT ?2",
            )
            .map_err(|err| err.to_string())?;
        let rows = stmt
            .query_map(params![needle, limit as i64], |r| r.get::<_, String>(0))
            .map_err(|err| err.to_string())?;
        Ok(rows.flatten().collect())
    }

    /// 按名字找符号（「转到符号」用）。
    ///
    /// ★ 前缀命中排在子串命中前面：敲 `scan` 时 `scan_symbols` 应该比
    ///   `rescan_all` 靠前 —— VS Code 也是这个行为
    pub fn find_symbols(&self, query: &str, limit: usize) -> Result<Vec<SymbolRow>, String> {
        let lower = query.to_lowercase();
        let needle = like_pattern(&lower);
        let prefix = format!("{}%", escape_like(&lower));
        let mut stmt = self
            .conn
            .prepare(
                "SELECT path, name, kind, line, col FROM symbols \
                 WHERE lower(name) LIKE ?1 ESCAPE '\\' \
                 ORDER BY (CASE WHEN lower(name) LIKE ?2 ESCAPE '\\' THEN 0 ELSE 1 END), \
                          length(name), name \
                 LIMIT ?3",
            )
            .map_err(|err| err.to_string())?;
        let rows = stmt
            .query_map(params![needle, prefix, limit as i64], |r| {
                Ok(SymbolRow {
                    path: r.get(0)?,
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    line: r.get::<_, i64>(3)? as usize,
                    column: r.get::<_, i64>(4)? as usize,
                })
            })
            .map_err(|err| err.to_string())?;
        Ok(rows.flatten().collect())
    }

    /// 还没进内容索引的文件 —— 这些得走旧的现场扫描，结果才不会漏。
    /// ★ 包含「待索引」（0）和「决定不索引」（2）两种：后者的内容是**永远**不会进索引的，
    ///   不把它们算进来的话，那些文件就永远搜不到了
    pub fn unindexed_paths(&self) -> Result<Vec<String>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM files WHERE indexed != 1 ORDER BY path")
            .map_err(|err| err.to_string())?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|err| err.to_string())?;
        Ok(rows.flatten().collect())
    }

    /// 清掉重建（给 UI 上那个「重建索引」按钮用）。
    /// 索引是**派生数据**，随时可以重建，不怕丢
    pub fn rebuild(&mut self, root: &Path) -> Result<IndexInfo, String> {
        self.conn
            .execute_batch("DELETE FROM fts; DELETE FROM files;")
            .map_err(|err| format!("清空索引失败：{err}"))?;
        self.last_sync_ms = 0;
        self.dirty = true;
        self.sync(root, true)
    }
}

/// 查询词能不能用索引。
///
/// ★ 判据是「有**连续** 3 个能当三元组用的字符」，不是「总长度 3」：
///   `a%b` 长度也是 3，但被通配符劈开后就提取不出三元组
/// ★ 这里是**故意保守**的：带 `%` / `_` 的查询虽然理论上也能走索引，
///   但「SQLite 怎么看待 "还转义过的通配符"」是个实现细节 ——
///   拿不准就当不能用。退回去扫一遍只是慢，不会错
pub fn is_indexable(query: &str) -> bool {
    let mut run = 0usize;
    for ch in query.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            run = 0;
        } else {
            run += 1;
            if run >= MIN_INDEXED_CHARS {
                return true;
            }
        }
    }
    false
}

/// 把一个字符串里的 LIKE 元字符转义掉（`%` `_` `\`）
fn escape_like(needle: &str) -> String {
    let mut out = String::with_capacity(needle.len());
    for ch in needle.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// 把查询词包成 LIKE 模式，顺手把 LIKE 的元字符转义掉。
/// ⚠ 不转义的话，用户搜 `100%` 里的 `%` 会变成通配符 —— 候选集变大。
///   结果依然是对的（真正命不命中由 `search_in_text` 决定），但会白读一堆文件
fn like_pattern(needle: &str) -> String {
    format!("%{}%", escape_like(needle))
}

/// 把字符串包成 SQL 字面量（单引号里再把单引号翻倍）。
/// ★ 这是**安全边界**：模式必须内联成字面量才走得了 trigram 索引（见 `candidate_paths`），
///   而内联就意味着拼接 SQL —— 用户搜 `it's` 时那个单引号只能靠这里処理
fn sql_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push('\'');
        }
        out.push(ch);
    }
    out.push('\'');
    out
}

// ============================ 符号扫描 ============================
//
// ★ 为什么用行级启发式，而不是语言服务器：`Ctrl+T` 要在**整个仓库**里找符号，
//   而语言服务器只在你打开过的文件上有数据。扫描虽然粗糙，但「覆盖面全」
//   和「不用等服务器起来」是它的价值。
//
// ⚠ 它失败的方式是「少几个符号 / 名字取歪了」，**不会报错** ——
//   所以这里的自标是「常见写法别漏」，不追求精确。
//   将来打开过的文件可以用 LSP 的 `documentSymbol` 覆盖掉这些粗糙结果，
//   表结构不用动（`kind` 用的就是 LSP 的编号）

/// 一个挖出来的符号
#[derive(Clone)]
struct SymbolHit {
    name: String,
    kind: i64,
    /// 1 起的行号
    line: usize,
    /// 1 起的列号（符号名在这一行的第几个**字符**）
    column: usize,
}

/// 声明前面可能出现的修饰词。剥掉它们才能看到真正的关键字
/// ⚠ `const` 不在表里：它既是修饰词也是关键字，当成关键字处理才对
const MODIFIERS: &[&str] = &[
    "pub", "export", "default", "async", "unsafe", "abstract", "final", "public", "private",
    "protected", "internal", "declare", "readonly", "open", "override", "extern",
];

/// 「关键字 → 符号类型」的表。
/// ★ 表放在一处，加一种语言就是加一行 —— 不用去改扫描逻辑
fn keywords_for(ext: &str) -> &'static [(&'static str, i64)] {
    match ext {
        "rs" => &[
            ("fn", 12),
            ("struct", 23),
            ("enum", 10),
            ("trait", 11),
            ("impl", 2),
            ("mod", 2),
            ("const", 14),
            ("static", 14),
            ("type", 26),
        ],
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "vue" => &[
            ("function", 12),
            ("class", 5),
            ("interface", 11),
            ("type", 26),
            ("enum", 10),
            ("const", 13),
            ("let", 13),
            ("var", 13),
        ],
        "py" => &[("def", 12), ("class", 5)],
        _ => &[
            ("function", 12),
            ("func", 12),
            ("fn", 12),
            ("def", 12),
            ("class", 5),
            ("interface", 11),
            ("struct", 23),
            ("enum", 10),
            ("const", 14),
        ],
    }
}

/// 值得扫符号的扩展名。不在表里的（`.txt` / `.log` / `.lock` …）直接跳过 ——
/// 不跳过的话，一篇说明文里的 “class Foo” 也会变成符号
fn is_code_ext(ext: &str) -> bool {
    matches!(
        ext,
        "rs" | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "vue"
            | "py"
            | "java"
            | "kt"
            | "kts"
            | "cs"
            | "go"
            | "rb"
            | "php"
            | "swift"
            | "dart"
            | "scala"
            | "c"
            | "cc"
            | "cpp"
            | "h"
            | "hpp"
            | "sh"
            | "ps1"
            | "sql"
            | "lua"
    )
}

fn extension_of(path: &str) -> String {
    // `rsplit_once` 而不是 `split('.').last()`：后者会把目录名里的点也算进来
    path.rsplit_once('.')
        .map(|(_, ext)| ext.to_lowercase())
        .unwrap_or_default()
}

/// 把一个标识符字符当一个词。其余字符都是分隔符
fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_' || ch == '$'
}

/// 把一行切成「词 + 它在这一行的字符下标」。
/// ★ 不用 `split_whitespace`：它不给位置，而我们要存符号名的列号
fn words_of(line: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut start = 0usize;
    for (index, ch) in line.chars().enumerate() {
        if is_word_char(ch) {
            if current.is_empty() {
                start = index;
            }
            current.push(ch);
        } else if !current.is_empty() {
            out.push((std::mem::take(&mut current), start));
        }
    }
    if !current.is_empty() {
        out.push((current, start));
    }
    out
}

fn is_identifier(word: &str) -> bool {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) if first.is_alphabetic() || first == '_' || first == '$' => {}
        _ => return false,
    }
    word.chars().count() <= 120
}

/// 从一份源码里挖符号
fn scan_symbols(path: &str, text: &str) -> Vec<SymbolHit> {
    let ext = extension_of(path);
    match ext.as_str() {
        "md" | "markdown" => return scan_headings(text),
        "css" | "scss" | "less" => return scan_selectors(text),
        "json" | "jsonc" => return scan_json_keys(text),
        _ => {}
    }
    if !is_code_ext(&ext) {
        return Vec::new();
    }

    let keywords = keywords_for(&ext);
    let mut out = Vec::new();

    for (index, raw) in text.lines().enumerate() {
        if out.len() >= 2000 {
            break; // 病态文件（一行一个声明写几千行）不追
        }
        let trimmed = raw.trim_start();
        // 注释行跳过：注释里的 “fn foo” 不是符号
        if trimmed.starts_with("//")
            || trimmed.starts_with('*')
            || trimmed.starts_with("/*")
            || trimmed.starts_with('#')
        {
            continue;
        }
        let indent = raw.chars().count() - trimmed.chars().count();
        let words = words_of(trimmed);

        // ★ 在前几个词里找**第一个关键字**，名字取它后面第一个标识符。
        //   为什么不是「只看第一个词」：`pub(crate) fn foo` 、
        //   `export default async function foo` 都有好几层前缀
        //   为什么只找前 5 个词就停：再往后就不是声明头了
        for (position, (word, _)) in words.iter().take(5).enumerate() {
            let Some((_, kind)) = keywords.iter().find(|(keyword, _)| keyword == word) else {
                continue;
            };
            let Some((name, name_column)) = words
                .iter()
                .skip(position + 1)
                .find(|(candidate, _)| is_identifier(candidate))
            else {
                break;
            };
            out.push(SymbolHit {
                name: name.clone(),
                kind: *kind,
                line: index + 1,
                column: indent + name_column + 1,
            });
            break;
        }
    }

    let _ = MODIFIERS; // 修饰词已经在「找第一个关键字」里自然跳过了，这里留个提示
    out
}

/// Markdown 标题（`#` ~ `######`）。
/// ★ 要跳过围栏代码块 —— 代码块里的 `# 注释` 不是标题
fn scan_headings(text: &str) -> Vec<SymbolHit> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for (index, raw) in text.lines().enumerate() {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let hashes = trimmed.chars().take_while(|ch| *ch == '#').count();
        if hashes == 0 || hashes > 6 {
            continue;
        }
        let name = trimmed.chars().skip(hashes).collect::<String>();
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        out.push(SymbolHit {
            name: name.to_string(),
            kind: 15, // String —— 标题在 VS Code 里也是这一类
            line: index + 1,
            column: hashes + 2,
        });
    }
    out
}

/// CSS 的类 / id 选择器。元素选择器不收 —— 那太多了，列表会被淹掉
fn scan_selectors(text: &str) -> Vec<SymbolHit> {
    let mut out = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let trimmed = raw.trim_start();
        let Some(brace) = trimmed.find('{') else { continue };
        let head = trimmed[..brace].trim();
        // 一行里既有声明又有 `{`（比如 `a { color: red }`）就不当选择器
        if head.is_empty() || trimmed.contains(';') {
            continue;
        }
        let name = head
            .split([',', ' '])
            .next()
            .unwrap_or("")
            .trim();
        if !(name.starts_with('.') || name.starts_with('#')) {
            continue;
        }
        let indent = raw.chars().count() - trimmed.chars().count();
        out.push(SymbolHit {
            name: name.to_string(),
            kind: 5,
            line: index + 1,
            column: indent + 1,
        });
    }
    out
}

/// JSON 的**顶层**键。
/// ★ 判「顶层」靠跟踪括号深度，不靠缩进 —— 缩进几格是格式化工具的自由，
///   而深度是语法事实
fn scan_json_keys(text: &str) -> Vec<SymbolHit> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;

    for (index, raw) in text.lines().enumerate() {
        let indent = raw.chars().count() - raw.trim_start().chars().count();
        let trimmed = raw.trim_start();
        if depth == 1 && trimmed.starts_with('"') {
            if let Some(end) = trimmed[1..].find('"') {
                let name = &trimmed[1..1 + end];
                let rest = trimmed[1 + end + 1..].trim_start();
                if rest.starts_with(':') && !name.is_empty() {
                    out.push(SymbolHit {
                        name: name.to_string(),
                        kind: 20, // Key
                        line: index + 1,
                        column: indent + 2,
                    });
                }
            }
        }
        for ch in raw.chars() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_string = false;
                }
                continue;
            }
            match ch {
                '"' => in_string = true,
                '{' | '[' => depth += 1,
                '}' | ']' => depth -= 1,
                _ => {}
            }
        }
    }
    out
}

// ============================ 会话表 + 命令 ============================

/// 每个工作区一个索引。放 State 里而不是全局变量 —— Tauri 管它的生命周期。
/// ★ 用 `Arc<Mutex<..>>` 是为了能把它**克隆进 `spawn_blocking` 的闭包**：
///   搜索是阻塞 IO，必须挪到阻塞线程池里跑
#[derive(Default)]
pub struct IndexStore(pub Arc<Mutex<HashMap<String, Index>>>);

/// 拿到（必要时打开）某个工作区的索引，借出去用一下。
///
/// ★ 参数直接要 `Arc<Mutex<..>>` 而不是 `&IndexStore`：
///   `search_in_folder` 要把索引放进 `spawn_blocking` 的闭包里，手里只有克隆出来的 Arc。
///   两边走同一个入口，真机上只有一份逻辑
/// ★ 索引打不开**不是错误**：搜索照常能搜，只是走旧的现场扫描。
///   所以调用方拿到的 `Err` 要当成「没有索引」处理
pub fn with_index<T>(
    app: &tauri::AppHandle,
    store: &Arc<Mutex<HashMap<String, Index>>>,
    root: &str,
    use_it: impl FnOnce(&mut Index) -> Result<T, String>,
) -> Result<T, String> {
    let key = root_key(root);
    let mut map = store.lock().map_err(|err| err.to_string())?;
    if !map.contains_key(&key) {
        let index = Index::open(&db_path(app, root)?)?;
        map.insert(key.clone(), index);
    }
    let index = map.get_mut(&key).ok_or("索引没建起来")?;
    use_it(index)
}

#[tauri::command]
pub fn index_info(
    app: tauri::AppHandle,
    store: tauri::State<'_, IndexStore>,
    root: String,
) -> Result<IndexInfo, String> {
    with_index(&app, &store.0, &root, |index| Ok(index.info()))
}

/// 后台把索引补齐。
///
/// ★★ 为什么要有它：索引是渐进建的（每轮只花 `SYNC_BUDGET_MS`），
///   光靠「用户搜索」推动的话，一个大仓库要搜很多次才建得完 ——
///   而在那期间每次搜索还得现场扫那些没建好的文件（= 和以前一样慢）。
///   后台慢慢补，用户第二次搜索就已经快很多了。
///
/// ★ 这里复用的是同一批代码（`build_more`），循环里每轮还睡一下 ——
///   让出 CPU，也顺便让搜索有机会拿到锁
fn warm(store: Arc<Mutex<HashMap<String, Index>>>, root: String) {
    // 同一个工作区只允许一个补齐线程（前端可能多次调它）
    static WARMING: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let warming = WARMING.get_or_init(|| Mutex::new(HashSet::new()));
    let key = root_key(&root);
    {
        let Ok(mut set) = warming.lock() else { return };
        if !set.insert(key.clone()) {
            return;
        }
    }

    std::thread::spawn(move || {
        loop {
            let pending = {
                let Ok(mut map) = store.lock() else { break };
                match map.get_mut(&key) {
                    Some(index) => match index.build_more() {
                        Ok(pending) => pending,
                        Err(err) => {
                            println!("[搜索索引] 后台补齐出错，剩下的走现场扫描：{err}");
                            break;
                        }
                    },
                    // 库已经不在了（关工作区时被清掉），没什么可补的
                    None => break,
                }
            };
            if pending == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        if let Ok(mut set) = warming.lock() {
            set.remove(&key);
        }
    });
}

/// 按文件名找文件（「转到文件」用）。
/// ★ 和内容搜索一样，索引没建好也有结果 —— `files` 表是**磁盘清单**，
///   不需要内容建好；搜索前先同步一下就能拿到当前的文件列表
#[tauri::command]
pub fn index_files(
    app: tauri::AppHandle,
    store: tauri::State<'_, IndexStore>,
    root: String,
    query: String,
    limit: Option<usize>,
) -> Result<Vec<String>, String> {
    with_index(&app, &store.0, &root, |index| {
        index.sync(Path::new(&root), false)?;
        index.find_files(&query, limit.unwrap_or(DEFAULT_FILE_LIMIT))
    })
}

/// 按名字找符号（「转到符号」用）。
#[tauri::command]
pub fn index_symbols(
    app: tauri::AppHandle,
    store: tauri::State<'_, IndexStore>,
    root: String,
    query: String,
    limit: Option<usize>,
) -> Result<Vec<SymbolRow>, String> {
    with_index(&app, &store.0, &root, |index| {
        index.sync(Path::new(&root), false)?;
        index.find_symbols(&query, limit.unwrap_or(DEFAULT_SYMBOL_LIMIT))
    })
}

/// 开始后台补齐（前端在工作区打开时调一次）。
/// 马上把当前状态返回去，不等建完 —— 建完要多久取决于仓库大小
#[tauri::command]
pub fn index_warm(
    app: tauri::AppHandle,
    store: tauri::State<'_, IndexStore>,
    root: String,
) -> Result<IndexInfo, String> {
    // 先把库打开：后台线程只负责「往前推」，不负责开库 / 建表
    let info = with_index(&app, &store.0, &root, |index| Ok(index.info()))?;
    if info.built && info.pending > 0 {
        warm(store.0.clone(), root);
    }
    Ok(info)
}

/// 重建索引（清空重来）
#[tauri::command]
pub fn index_rebuild(
    app: tauri::AppHandle,
    store: tauri::State<'_, IndexStore>,
    root: String,
) -> Result<IndexInfo, String> {
    with_index(&app, &store.0, &root, |index| {
        index.rebuild(Path::new(&root))
    })
}

/// 告诉索引「磁盘变过了」（保存文件之后调），下次搜索一定重新扫
#[tauri::command]
pub fn index_invalidate(
    app: tauri::AppHandle,
    store: tauri::State<'_, IndexStore>,
    root: String,
) -> Result<(), String> {
    with_index(&app, &store.0, &root, |index| {
        index.invalidate();
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(conn: &Connection, pattern: &str) -> Vec<i64> {
        let mut stmt = conn
            .prepare("SELECT rowid FROM fts WHERE body LIKE ?1 ESCAPE '\\' ORDER BY rowid")
            .unwrap();
        stmt.query_map([pattern], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn plan(conn: &Connection, sql: &str) -> Vec<String> {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        stmt.query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    /// 搭一个临时工作区，返回它的路径
    fn temp_workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bundled_sqlite_has_fts5_and_trigram() {
        // `bundled` 到底把哪些编译开关打开了，只有真跑一遍才知道。
        // 少了 FTS5 或少了 trigram 分词器，这里直接红，而不是等用户搜索时才炸
        let index = Index::memory();
        index
            .conn
            .execute(
                "INSERT INTO fts(rowid, path, body) VALUES (1, 'a.txt', 'hello world')",
                [],
            )
            .unwrap();
        assert_eq!(hit(&index.conn, "%lo wo%"), vec![1]);
        assert!(index.is_usable(), "LIKE 没走索引，索引不可用");
    }

    #[test]
    fn trigram_tokenizer_gives_substring_semantics() {
        // ★ 「为什么不用默认分词器」的证据：默认的 unicode61 按**词**索引，
        //   `foo` 匹配不到 `foobar`；而我们的搜索是「包含」语义
        let index = Index::memory();
        index
            .conn
            .execute_batch(
                "INSERT INTO fts(rowid, path, body) VALUES
                   (1, 'a.ts',  'let foobar = 1;'),
                   (2, 'b.ts',  'function handleclick() {}'),
                   (3, 'c.txt', 'const 中文变量 = 42;');",
            )
            .unwrap();

        assert_eq!(
            hit(&index.conn, "%oob%"),
            vec![1],
            "词中命中（子串语义的核心）"
        );
        assert_eq!(hit(&index.conn, "%nction h%"), vec![2], "跨空格也要能命中");
        assert_eq!(hit(&index.conn, "%中文%"), vec![3], "非 ASCII 也要能子串命中");
        assert_eq!(hit(&index.conn, "%zzzz%"), Vec::<i64>::new());
    }

    #[test]
    fn like_query_uses_the_trigram_index_not_a_scan() {
        // ★★ 真正要钉住的东西：trigram 允许 LIKE 走 FTS5 索引，但那是**优化器的行为**，
        //    不是语法保证的。退化成逐行扫时搜索结果还是对的，只是慢几百倍 ——
        //    所以必须把执行计划钉下来
        let index = Index::memory();
        index
            .conn
            .execute_batch("INSERT INTO fts(rowid, path, body) VALUES (1, 'a', 'let foobar = 1;');")
            .unwrap();

        let lines = plan(
            &index.conn,
            "SELECT path FROM fts WHERE body LIKE '%oob%' ESCAPE '\\'",
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("VIRTUAL TABLE INDEX")),
            "LIKE 没走 FTS5 索引，计划是：{lines:?}"
        );
        assert!(index.is_usable());
    }

    #[test]
    fn short_queries_are_not_worth_the_index() {
        // 短于 3 个字符提取不出三元组 —— 那时 FTS5 只能扫全表，比旧路径还贵。
        // 所以这个判断是「该不该用索引」的开关，不是随手写的长度检查
        assert!(!is_indexable("a"));
        assert!(!is_indexable("ab"));
        assert!(is_indexable("abc"));
        assert!(is_indexable("中文变"));
        // 带通配符的查询是**故意保守**的：`foo_bar` 里前三个字符就够了，
        // 但 `a%b` 这种「被劈成两段 1~2 字符」的就不敢用（宁可退回扫一遍）
        assert!(!is_indexable("a%b"));
        assert!(!is_indexable("ab%cd"));
        assert!(is_indexable("foo_bar"), "下划线不要把它整条废掉");
    }

    #[test]
    fn like_pattern_escapes_wildcards() {
        // 不转义的话，搜 `100%` 里的 % 会变成通配符 —— 候选集会变大
        assert_eq!(like_pattern("100%"), "%100\\%%");
        assert_eq!(like_pattern("a_b"), "%a\\_b%");
        assert_eq!(like_pattern("a\\b"), "%a\\\\b%");
        assert_eq!(like_pattern("普通"), "%普通%");
    }

    #[test]
    fn sql_literal_escapes_single_quotes() {
        // ★ 模式是内联成字面量的（绑定参数用不上 trigram 索引），
        //   所以这里的转义就是**安全边界** —— 少了它，搜 `it's` 会拼出一句坏 SQL
        assert_eq!(sql_literal("abc"), "'abc'");
        assert_eq!(sql_literal("it's"), "'it''s'");
    }

    #[test]
    fn queries_with_quotes_still_find_files() {
        // 端到端验证上面那个转义：真的拿带单引号的词去搜一遍
        let dir = temp_workspace("toocode-index-quote");
        std::fs::write(dir.join("a.txt"), "let name = 'toocode';\n").unwrap();

        let mut index = Index::memory();
        index.sync(&dir, true).unwrap();
        assert_eq!(index.candidate_paths("'toocode'").unwrap().len(), 1);
        assert_eq!(index.candidate_paths("it's").unwrap().len(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_indexes_content_and_survives_changes() {
        let dir = temp_workspace("toocode-index-sync");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.ts"), "let alpha = 1;\n").unwrap();
        std::fs::write(dir.join("sub/b.ts"), "let beta = 2;\n").unwrap();
        // 被忽略的目录不该进索引
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join("node_modules/c.ts"), "let gamma = 3;\n").unwrap();

        let mut index = Index::memory();
        let info = index.sync(&dir, true).unwrap();
        assert_eq!(info.files, 2, "node_modules 不该被索引");
        assert_eq!(info.indexed, 2, "两个小文件一次同步就该建完");
        assert_eq!(info.pending, 0);

        let found = |index: &Index, needle: &str| index.candidate_paths(needle).unwrap();
        assert_eq!(found(&index, "alpha").len(), 1);
        assert_eq!(found(&index, "beta").len(), 1);
        assert_eq!(found(&index, "gamma").len(), 0, "被忽略的目录不该搜得到");

        // 改了内容 → 下一次同步要重建它。
        // ⚠ 内容长度也变一下：mtime 的精度可能只到秒，size 变了差集才一定认得出来
        std::fs::write(dir.join("a.ts"), "let alpha = 42; // changed\n").unwrap();
        let info = index.sync(&dir, true).unwrap();
        assert_eq!(info.updated, 1, "变过的文件要重新索引");
        assert_eq!(found(&index, "42").len(), 1);
        assert_eq!(found(&index, "= 1").len(), 0, "旧内容不该还搜得到");

        // 删掉一个 → fts 里那行也要跟着走
        std::fs::remove_file(dir.join("sub/b.ts")).unwrap();
        let info = index.sync(&dir, true).unwrap();
        assert_eq!(info.removed, 1);
        assert_eq!(info.files, 1);
        assert_eq!(found(&index, "beta").len(), 0, "删掉的文件不该还搜得到");

        // 清空重建
        let info = index.rebuild(&dir).unwrap();
        assert_eq!(info.files, 1);
        assert_eq!(info.indexed, 1);
        assert_eq!(found(&index, "alpha").len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_content_is_marked_done_but_never_matches() {
        // 二进制 / 非 UTF-8 的文件：不能一直躺在「待索引」里每次重复读，
        // 但也不该被搜到
        let dir = temp_workspace("toocode-index-binary");
        std::fs::write(dir.join("good.txt"), "findme\n").unwrap();
        std::fs::write(dir.join("bad.bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();

        let mut index = Index::memory();
        let info = index.sync(&dir, true).unwrap();
        assert_eq!(info.files, 2);
        assert_eq!(
            info.pending, 0,
            "读不出来的也要标记成已处理，否则每次同步都重读"
        );
        assert_eq!(index.candidate_paths("findme").unwrap().len(), 1);
        assert_eq!(index.candidate_paths("fffe").unwrap().len(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn index_is_progressive_and_never_loses_the_rest() {
        // ★ 大仓库的核心保证：一次同步建不完时，剩下的文件必须能被列出来，
        //   让调用方走现场扫描 —— 否则「索引不全」就等于「搜不到」
        let dir = temp_workspace("toocode-index-progressive");
        for i in 0..(SYNC_BATCH_FILES + 5) {
            std::fs::write(dir.join(format!("f{i}.txt")), format!("content {i}\n")).unwrap();
        }

        let mut index = Index::memory();
        let info = index.sync(&dir, true).unwrap();
        assert_eq!(info.files, SYNC_BATCH_FILES + 5);
        // 受批次上限保护，这一轮不可能全都建完
        assert!(info.pending > 0, "批次上限没起作用");
        assert_eq!(
            info.pending + info.indexed,
            info.files,
            "建好的 + 待建的必须正好等于总数"
        );
        // 没建好的那些必须能列出来（列表非空，否则那些文件就永远搜不到了）
        let rest = index.unindexed_paths().unwrap();
        assert_eq!(rest.len(), info.pending);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn content_index_stops_at_the_byte_cap_and_keeps_the_rest_on_the_old_path() {
        // ★ 上限存在的意义：trigram 索引可能是原文的 1~2 倍，
        //   不封顶的话一个大仓库能长出好几 GB 的库文件
        let dir = temp_workspace("toocode-index-cap");
        for i in 0..6 {
            std::fs::write(dir.join(format!("f{i}.txt")), "x".repeat(1000)).unwrap();
        }

        let mut index = Index::memory();
        index.max_bytes = 2500; // 只装得下两个
        let info = index.sync(&dir, true).unwrap();
        assert_eq!(info.files, 6);
        assert_eq!(info.indexed, 2, "刚好装两个（2×1000 ≤ 2500 < 3×1000）");
        assert_eq!(info.skipped, 4);
        assert_eq!(
            info.pending, 0,
            "到顶之后不该还有「待索引」—— 否则后台线程永远停不下来"
        );
        // ★ 被跳过的仍然要被列出来：它们的内容**永远**不会进索引，
        //   不列的话那些文件就永远搜不到了
        assert_eq!(index.unindexed_paths().unwrap().len(), 4);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ttl_skips_the_scan_unless_dirtied() {
        let dir = temp_workspace("toocode-index-ttl");
        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();

        let mut index = Index::memory();
        index.sync(&dir, true).unwrap();

        // TTL 内再同步一次：应该跳过（没花时间）
        let cached = index.sync(&dir, false).unwrap();
        assert_eq!(cached.updated, 0);
        assert_eq!(cached.removed, 0);
        assert_eq!(cached.took_ms, 0);

        // 但被标脏之后，即便在 TTL 内也要真扫一遍
        index.invalidate();
        std::fs::write(dir.join("b.txt"), "world\n").unwrap();
        let info = index.sync(&dir, false).unwrap();
        assert_eq!(info.updated, 1, "标脏之后必须重新扫");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_files_matches_any_part_of_the_path_case_insensitively() {
        let dir = temp_workspace("toocode-index-names");
        std::fs::create_dir_all(dir.join("src/components")).unwrap();
        std::fs::write(dir.join("src/App.vue"), "x").unwrap();
        std::fs::write(dir.join("src/components/ChatPanel.vue"), "x").unwrap();
        std::fs::write(dir.join("README.md"), "x").unwrap();

        let mut index = Index::memory();
        index.sync(&dir, true).unwrap();
        let names = |q: &str| -> Vec<String> {
            index
                .find_files(q, 50)
                .unwrap()
                .into_iter()
                .map(|p| p.replace('\\', "/"))
                .collect()
        };

        // 大小写不敏感（用户敲的都是小写）
        assert_eq!(names("app.vue").len(), 1);
        // 命中的是**整条路径**，所以目录名也能搜
        assert!(names("components")[0].ends_with("components/ChatPanel.vue"));
        assert_eq!(names("zzzz").len(), 0);
        // 粗排序：短的路径排前面（真正的排序在前端的模糊打分）
        let vues = names(".vue");
        assert_eq!(vues.len(), 2);
        assert!(vues[0].ends_with("App.vue"), "短的应该在前：{vues:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scans_common_declarations_and_skips_noise() {
        let hits = |path: &str, text: &str| -> Vec<String> {
            scan_symbols(path, text)
                .into_iter()
                .map(|symbol| format!("{}:{}", symbol.name, symbol.line))
                .collect()
        };

        // Rust：修饰词要剥掉，`pub(crate)` 这种带括号的也算修饰
        assert_eq!(
            hits(
                "a.rs",
                "pub fn alpha() {}\nimpl Beta {\n    async fn gamma(&self) {}\n}\n"
            ),
            vec!["alpha:1", "Beta:2", "gamma:3"]
        );
        assert_eq!(hits("a.rs", "pub(crate) fn hidden() {}\n"), vec!["hidden:1"]);
        // TS：修饰词有好几层
        assert_eq!(
            hits("a.ts", "export default async function delta() {}\n"),
            vec!["delta:1"]
        );
        assert_eq!(
            hits("a.ts", "export const EPSILON = 1;\nclass Zeta {}\n"),
            vec!["EPSILON:1", "Zeta:2"]
        );
        assert_eq!(hits("a.py", "class Eta:\n    def theta(self):\n"), vec![
            "Eta:1",
            "theta:2"
        ]);
        // 注释里的「声明」不是声明
        assert_eq!(hits("a.rs", "// fn ghost() {}\n"), Vec::<String>::new());
        // 不认识的行不硬编：`if let` / `for` 不是声明
        assert_eq!(hits("a.rs", "if let Some(x) = y {}\nfor i in 0..3 {}\n"), Vec::<
            String,
        >::new());
        // 不该扫的扩展名（一篇说明文里的 class Foo 不是符号）
        assert_eq!(hits("a.txt", "function whatever() {}\n"), Vec::<String>::new());
    }

    #[test]
    fn scans_markdown_headings_css_selectors_and_json_keys() {
        let names = |path: &str, text: &str| -> Vec<String> {
            scan_symbols(path, text)
                .into_iter()
                .map(|symbol| symbol.name)
                .collect()
        };

        // ★ markdown 要跳过围栏代码块 —— 代码块里的 `# 注释` 不是标题
        assert_eq!(
            names("a.md", "# Title\n\n```\n# 这不是标题\n```\n\n## Sub\n"),
            vec!["Title", "Sub"]
        );
        // CSS 只收类 / id 选择器
        assert_eq!(
            names("a.css", ".foo {\n  color: red;\n}\n#bar:hover {\n}\ndiv {\n}\n"),
            vec![".foo", "#bar:hover"]
        );
        // JSON 只收顶层的键（靠括号深度判断，不靠缩进）
        assert_eq!(
            names(
                "a.json",
                "{\n  \"alpha\": 1,\n  \"nested\": {\n    \"beta\": 2\n  }\n}\n"
            ),
            vec!["alpha", "nested"]
        );
    }

    #[test]
    fn symbols_follow_the_file_through_change_and_delete() {
        let dir = temp_workspace("toocode-index-symbols");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/a.ts"),
            "export function alphaThing() {}\nexport const betaThing = 1;\nexport function thingOne() {}\n",
        )
        .unwrap();
        std::fs::write(dir.join("README.md"), "# Gamma Heading\n").unwrap();

        let mut index = Index::memory();
        index.sync(&dir, true).unwrap();
        let names = |index: &Index, query: &str| -> Vec<String> {
            index
                .find_symbols(query, 50)
                .unwrap()
                .into_iter()
                .map(|row| row.name)
                .collect()
        };

        assert_eq!(names(&index, "alphathing"), vec!["alphaThing"], "大小写不敏感");
        assert_eq!(names(&index, "gamma"), vec!["Gamma Heading"], "markdown 标题也算符号");
        // 前缀命中排在子串命中前面：thing 开头的那条应该第一
        let thing = names(&index, "thing");
        assert_eq!(thing[0], "thingOne", "前缀命中要排前面：{thing:?}");
        assert!(thing.contains(&"alphaThing".to_string()));

        // 文件改了 → 旧符号不能还留着（否则点进去是空的）
        std::fs::write(dir.join("src/a.ts"), "export function renamedThing() {}\n").unwrap();
        index.sync(&dir, true).unwrap();
        assert_eq!(names(&index, "alphathing"), Vec::<String>::new());
        assert_eq!(names(&index, "renamedthing"), vec!["renamedThing"]);

        // 文件删了 → 符号也得跟着走
        std::fs::remove_file(dir.join("README.md")).unwrap();
        index.sync(&dir, true).unwrap();
        assert_eq!(names(&index, "gamma"), Vec::<String>::new());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn root_key_and_hash_are_stable_across_path_spellings() {
        // Windows 路径不区分大小写，尾部斜杠也不该造成两个库
        assert_eq!(root_key("D:\\Code\\Proj\\"), root_key("d:\\code\\proj"));
        assert_eq!(hash_root("D:\\Code\\Proj"), hash_root("d:\\code\\proj\\"));
        assert_ne!(hash_root("D:\\a"), hash_root("D:\\b"));
    }
}
