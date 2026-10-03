use base64::Engine as _;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
// ⚠ `Child` 这个名字已经被 portable_pty 占了（终端那块的），
//   所以 std 的这个显式改名成 StdChild —— 两边混用会编不过，但报错信息不好懂
// ★ 这里**不需要** ChildStdout：读线程已经泛型化成 `impl Read` 了
use std::process::{Child as StdChild, ChildStderr, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use tauri::{AppHandle, Emitter, State};

// 搜索索引（SQLite）。单独一个文件 —— 它自己有一整套「建库 / 同步 / 查询」的逻辑，
// 塞进 lib.rs 只会让这个已经三千多行的文件更难找东西
mod searchdb;

// ============================ 数据结构 ============================

/// 文件树节点。
/// serde 负责把它序列化成 JSON —— 前端的 FileNode 接口就是照着它对齐的。
#[derive(Serialize)]
struct FileNode {
    name: String,
    path: String,

    // `type` 是 Rust 关键字，不能直接当字段名。
    // 所以内部叫 kind，序列化时用 rename 改成前端期望的 "type"
    #[serde(rename = "type")]
    kind: NodeKind,

    // 文件没有 children。
    // skip_serializing_if 让 None 字段干脆不出现在 JSON 里，
    // 而不是输出 "children": null
    #[serde(skip_serializing_if = "Option::is_none")]
    children: Option<Vec<FileNode>>,
}

/// 对应前端 TS 里的 `"file" | "folder"` 联合类型。
/// rename_all = "lowercase" 会把变体名转成小写字符串（File -> "file"）。
/// 用枚举而不是 String，是为了让编译器帮我们挡住拼写错误。
#[derive(Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum NodeKind {
    File,
    Folder,
}

// ============================ 配置常量 ============================

/// 这些目录一律跳过。
/// 不跳的话，光是一个 node_modules 就足以把编辑器卡死。
const IGNORED_DIRS: &[&str] = &[".git", "node_modules", "target", "dist", ".vite", ".idea"];

/// 递归深度上限，防止超深目录把一次性读取拖垮
const MAX_DEPTH: usize = 6;

/// 单个文件最大可读字节数 —— 防止把几百 MB 的文件塞进编辑器
const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;

// ============================ 目录遍历 ============================

/// 递归遍历目录。
///
/// 这和我们在前端写的 `findNodeByPath`、以及 `FileTreeNode` 的渲染递归，
/// 是同一个形态：处理当前项 → 对每个子项做同样的事 → 有终止条件。
/// 这里的终止条件是「深度超限」或「它不是目录」。
fn walk(dir: &Path, depth: usize) -> Vec<FileNode> {
    let mut nodes = Vec::new();

    if depth > MAX_DEPTH {
        return nodes;
    }

    // 读不了（权限不足、路径失效）就当空目录处理。
    // 这里的取舍：单个子目录读失败，不该让整个命令失败
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return nodes,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = path.is_dir();

        if is_dir && IGNORED_DIRS.contains(&name.as_str()) {
            continue;
        }

        nodes.push(FileNode {
            name,
            // 统一用 / 当分隔符。Windows 原生是 \，但前端那边（fileNameOf、languageFromPath）
            // 都是按 / 切的。在「跨语言边界」上统一格式，前端就不用到处做兼容判断
            path: path.to_string_lossy().replace('\\', "/"),
            kind: if is_dir { NodeKind::Folder } else { NodeKind::File },
            children: if is_dir { Some(walk(&path, depth + 1)) } else { None },
        });
    }

    // 文件夹排前面，再按名字不区分大小写排序 —— 和常见文件管理器一致
    nodes.sort_by(|a, b| {
        let a_is_dir = a.kind == NodeKind::Folder;
        let b_is_dir = b.kind == NodeKind::Folder;
        b_is_dir
            .cmp(&a_is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    nodes
}

// ============================ Tauri 命令 ============================
//
// #[tauri::command] 会把一个普通函数注册成前端可调用的命令。
// 参数和返回值都必须能过 serde —— 它在中间做 Rust <-> JSON 的转换。

/// 递归读取一个目录，返回整棵树。
/// 前端调用方式：invoke("read_dir", { path: "..." })
#[tauri::command]
fn read_dir(path: String) -> Result<Vec<FileNode>, String> {
    let root = Path::new(&path);

    if !root.is_dir() {
        return Err(format!("不是一个目录：{path}"));
    }

    Ok(walk(root, 0))
}

/// 读取一个文本文件。
/// 注意返回类型是 Result —— 前端拿到的不是返回值就是错误信息，不会静默失败。
#[tauri::command]
fn read_file(path: String) -> Result<String, String> {
    let meta = fs::metadata(&path).map_err(|e| format!("无法读取文件信息 {path}：{e}"))?;

    if meta.len() > MAX_FILE_BYTES {
        return Err(format!(
            "文件太大（{} KB），超过上限 {} KB",
            meta.len() / 1024,
            MAX_FILE_BYTES / 1024
        ));
    }

    let bytes = fs::read(&path).map_err(|e| format!("读取失败 {path}：{e}"))?;

    // 这里不用 fs::read_to_string：它会把「不是合法 UTF-8」和「IO 出错」
    // 混成同一条报错（"stream did not contain valid UTF-8"）——
    // 那是给开发者看的，不是给用户看的。
    // 自己走一遍 String::from_utf8，就能把这种情况翻译成人话。
    String::from_utf8(bytes).map_err(|_| {
        "这不是一个文本文件（内容不是合法的 UTF-8 文本），编辑器没办法当文本打开。\n\
         如果它是图片，点它就会显示预览。\n\
         其它类型可以走菜单「文件 → 用系统默认程序打开」。"
            .to_string()
    })
}

// ============================ ICNS 容器 ============================

/// PNG 文件的魔数（开头这 8 个固定字节）。
/// 「文件开头的几个固定字节」就是它的身份证 —— 靠它认类型比靠扩展名可靠得多
const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// 从 .icns 里把 PNG 抠出来。
///
/// icns 是 macOS 的图标容器格式，**它本身不是图片，而是一个盒子**：
///   [ "icns" | 总长度 ] 后面跟着若干条目，每条是 [ 类型 4B | 长度 4B | 数据 ]
///
/// 关键在于：现代 icns 条目的「数据」**本来就是 PNG**
///（苹果为省空间直接塞 PNG 进去，而不是未压缩位图）。
/// 所以根本不需要什么解码库 —— 走一遍容器、把数据以 PNG 魔数开头的那个挑出来就行了。
///
/// 返回数据最长的那一条（也就是尺寸最大的那个图标）。
fn extract_icns_png(bytes: &[u8]) -> Option<&[u8]> {
    if !bytes.starts_with(b"icns") {
        return None;
    }

    let mut cursor = 8usize;
    let mut best: Option<&[u8]> = None;

    // 这是典型的「长度前缀」式结构：读到一条长度就跳一条，直到走完。
    // 和链表不同，它不需要指针 —— 长度本身就告诉你下一条在哪
    while cursor + 8 <= bytes.len() {
        let size = u32::from_be_bytes([
            bytes[cursor + 4],
            bytes[cursor + 5],
            bytes[cursor + 6],
            bytes[cursor + 7],
        ]) as usize;

        // 长度非法就停 —— 文件坏了，硬着头皮往下读只会越界 panic
        if size < 8 || cursor + size > bytes.len() {
            break;
        }

        let data = &bytes[cursor + 8..cursor + size];
        if data.starts_with(&PNG_MAGIC) && best.map_or(true, |b| data.len() > b.len()) {
            best = Some(data);
        }

        cursor += size;
    }

    best
}

/// 读取一个「能直接显示」的二进制文件。
///
/// 和 read_file 的区别：
///   那个返回 String，遇到非 UTF-8 字节直接报错；
///   这个把字节原样递出去，压根不过 UTF-8 这道关，并且必要时会做格式转换。
///
/// 返回值用 tauri::ipc::Response 而不是 Vec<u8>：
///   Vec<u8> 会被 serde 序列化成 JSON 数组（[137, 80, 78, 71, ...]），
///   一个 50 KB 的图标会膨胀成几十万个数字的文本。
///   Response 走的是原始字节通道，一个字节就是一个字节。
#[tauri::command]
fn read_preview_bytes(path: String) -> Result<tauri::ipc::Response, String> {
    let meta = fs::metadata(&path).map_err(|e| format!("无法读取文件信息 {path}：{e}"))?;

    if meta.len() > MAX_FILE_BYTES {
        return Err(format!(
            "文件太大（{} KB），超过上限 {} KB",
            meta.len() / 1024,
            MAX_FILE_BYTES / 1024
        ));
    }

    let bytes = fs::read(&path).map_err(|e| format!("读取失败 {path}：{e}"))?;

    // to_lowercase：Windows 上扩展名大小写不敏感（.ICNS 也得认）
    if path.to_lowercase().ends_with(".icns") {
        return match extract_icns_png(&bytes) {
            Some(png) => Ok(tauri::ipc::Response::new(png.to_vec())),
            None => Err(
                "这个 .icns 里没有内嵌的 PNG，无法显示。\n\
                 （很老式的 icns 用未压缩位图，那才真的需要专门解码器）"
                    .to_string(),
            ),
        };
    }

    Ok(tauri::ipc::Response::new(bytes))
}

/// 把内容写回文件。
///
/// 返回 Result<(), String> —— 成功时没有值可取（() 就是 Rust 的「无返回值」），
/// 失败时把原因带回前端。
#[tauri::command]
fn write_file(path: String, content: String) -> Result<(), String> {
    // 注意 fs::write 是「整体覆盖」而不是追加：
    // 它会把文件截断成 0 字节再写入新内容
    fs::write(&path, content).map_err(|e| format!("写入失败 {path}：{e}"))
}

// ============================ 语法扩展（插件机制的第一块）============================
//
// 目标：让编辑器能直接吃 **VS Code 扩展里的 TextMate 语法**。
// 这比自己去集成一个语法引擎有意思得多 ——
// 语法引擎只是个库，而「能加载 VS Code 的扩展」是一个机制。
//
// ★ 为什么读扩展的 package.json，而不是递归去找 *.tmLanguage.json：
//   1. 扩展目录里往往还塞着 node_modules，全量递归会慢得多
//   2. **语法文件自己不知道自己是给哪种文件用的** ——
//      「.vue 文件该用 source.vue 这个语法」这条信息只写在
//      package.json 的 contributes 里。VS Code 也是这么读的
//
// 返回给前端的是**路径**而不是语法内容：一个扩展可能几百 KB，
// 几十个全塞进 IPC 不划算 —— 前端可以按需再去读

#[derive(Deserialize)]
struct ExtensionManifest {
    contributes: Option<Contributes>,
}

#[derive(Deserialize)]
struct Contributes {
    #[serde(default)]
    languages: Vec<LanguageContribution>,
    #[serde(default)]
    grammars: Vec<GrammarContribution>,
    #[serde(default)]
    themes: Vec<ThemeContribution>,
    #[serde(default)]
    snippets: Vec<SnippetContribution>,
}

/// 一条主题贡献（`contributes.themes`）。
///
/// ★ 字段全部给了 `#[serde(default)]`：serde 是**全有或全无**的 ——
///   一个字段类型不对，整个 `package.json` 都会被跳过，这个扩展的语法也一起没了。
///   主题的个别字段缺了不值当付出这种代价
#[derive(Deserialize)]
struct ThemeContribution {
    #[serde(default)]
    id: String,
    #[serde(default)]
    label: String,
    #[serde(rename = "uiTheme", default)]
    ui_theme: String,
    #[serde(default)]
    path: String,
}

#[derive(Deserialize)]
struct LanguageContribution {
    id: String,
    /// 形如 [".vue"]，带点
    #[serde(default)]
    extensions: Vec<String>,
}

#[derive(Deserialize)]
struct GrammarContribution {
    /// 这个语法服务于哪个语言 id。没写就**不挂到任何语言上**
    /// （但不是说就可以丢掉它 —— 见下面 inject_to 的说明）
    language: Option<String>,
    #[serde(rename = "scopeName")]
    scope_name: String,
    path: String,
    /// 这个语法要**注入到**哪些 scope 里去。
    ///
    /// ★ 这就是「注入语法」的声明：声明了 injectTo、又没写 language 的那些
    ///   （Volar 有 6 个），平时根本不会被人加载 ——
    ///   而是等某个父语法分词到某个位置时，TextMate 按当前 scope 把它们找出来。
    /// ⚠ 值可能带 `L:` 前缀（如 `L:source.vue`），表示注入到那一层；
    ///   前端要负责剥掉前缀，并且把方向**反转**（清单写的是「我注入到谁」，
    ///   而 TextMate 问的是「谁注入到我这儿」）
    #[serde(rename = "injectTo", default)]
    inject_to: Vec<String>,
}

/// 一条可用的语法
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GrammarEntry {
    /// 这条语法服务于哪个 Monaco 语言 id。
    ///
    /// ★ 为 None 表示它是「注入语法」：清单里没写 `language`，
    ///   它不挂到任何语言上，但别的语法会 include 它。
    ///   所以路径照样得交出去 —— 少了它们，Vue 之类的语法会缺一块
    id: Option<String>,
    /// 语法文件声明的 scopeName，TextMate 内部用它互相 include
    scope_name: String,
    /// 语法文件的绝对路径（前端再自己去读内容）
    path: String,
    /// 对应哪些文件扩展名，带点，如 [".vue"]
    extensions: Vec<String>,
    /// 要注入到哪些 scope（原样带过来，前端负责剥 `L:` 前缀 + 反转方向）
    inject_to: Vec<String>,
}

/// 一条可用的主题
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ThemeEntry {
    id: String,
    /// 给用户看的名字
    label: String,
    /// vs-dark / vs / hc-black / hc-light
    ui_theme: String,
    /// 主题文件的绝对路径（前端再自己去读内容）
    path: String,
}

/// 用户主目录。
/// 不引第三方库：Windows 看 USERPROFILE，类 Unix 看 HOME，够用了
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// VS Code **程序本身**可能装在哪。
///
/// ★ 不能写死路径：绿色版、自定义位置、多版本并存都很常见（用户的就装在 D:\vscode），
///   而我们要找的「内置扩展」（html / css / ts / json 这些父语法的家）
///   就在程序目录下面的 `resources/app/extensions`。
///
/// 按可信度找两条线索：
///   1) PATH —— VS Code 会把自己的 `bin` 加进去（`<安装根>[/版本目录]/bin`）
///   2) 标准安装位置
/// 从注册表问「VS Code 装在哪」。
///
/// ★ 为什么非要有这条线索：绿色版 / 自定义安装位置的 VS Code 不在标准路径下，
///   而 PATH 里的线索只在「从 VS Code 自己的终端启动」时才在 ——
///   换个终端启动应用，PATH 就没有了。而「内置扩展」恰恰只在安装目录里，
///   断了这条线，Vue 这类语法就会缺一大块。
///
/// VS Code 注册 `vscode://` 协议时会写下一个键，值里就有 Code.exe 的完整路径 ——
/// 这是机器上最可靠的「VS Code 在哪」的线索
fn vscode_roots_from_registry() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    for key in [
        r"HKCU\Software\Classes\vscode\shell\open\command",
        r"HKLM\SOFTWARE\Classes\vscode\shell\open\command",
    ] {
        let Ok(output) = Command::new("reg").args(["query", key, "/ve"]).output() else {
            continue;
        };
        let text = String::from_utf8_lossy(&output.stdout);

        // 值长这样：  (Default)    REG_SZ    "D:\...\Code.exe" --open-url -- "%1"
        // 只要引号里的第一段 —— 那就是 exe 的完整路径
        let Some(open) = text.find('"') else { continue };
        let Some(close) = text[open + 1..].find('"') else {
            continue;
        };
        let exe = PathBuf::from(&text[open + 1..open + 1 + close]);

        // exe 所在目录 + 它的父目录都当候选：
        // 标准安装版 exe 就在安装根下；绿色版会多一层版本目录，两种都覆盖到
        if let Some(dir) = exe.parent() {
            roots.push(dir.to_path_buf());
            if let Some(parent) = dir.parent() {
                roots.push(parent.to_path_buf());
            }
        }
    }

    roots
}

fn vscode_install_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    // 1) 注册表 —— 最可信：它记的就是「用户实际在用的那个 VS Code」
    roots.extend(vscode_roots_from_registry());

    // 2) PATH —— VS Code 会把自己的 bin 加进去
    if let Some(path) = std::env::var_os("PATH") {
        for entry in std::env::split_paths(&path) {
            let lowered = entry.to_string_lossy().to_lowercase();
            if !lowered.contains("vs code") && !lowered.contains("vscode") {
                continue;
            }
            // PATH 里放的可能是安装根，也可能是它的 bin 子目录，两种都当作线索
            roots.push(entry.clone());
            if let Some(parent) = entry.parent() {
                roots.push(parent.to_path_buf());
            }
        }
    }

    // 3) 标准安装位置
    for var in ["LOCALAPPDATA", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(base) = std::env::var_os(var) {
            let base = PathBuf::from(base);
            roots.push(base.join("Programs").join("Microsoft VS Code"));
            roots.push(base.join("Microsoft VS Code"));
        }
    }

    roots
}

/// 所有可能放着扩展的目录，**按优先级排列**（前面的赢）。
fn extension_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();

    // 1) 用户自己装的扩展 —— 用户装的东西该说话
    if let Some(home) = home_dir() {
        roots.push(home.join(".vscode").join("extensions"));
        roots.push(home.join(".vscode-insiders").join("extensions"));
    }

    // 2) VS Code **自带**的扩展。
    //   ★ 这一步不能省：html / css / javascript / typescript / json / markdown
    //     这些「父语法」只在程序目录里，不在用户扩展目录里。
    //     而 Vue、Svelte、Astro 这类语法几乎全靠 include 它们 ——
    //     少了它们，.vue 里 html / 表达式那部分一个 token 都分不出来
    for install in vscode_install_roots() {
        // 绿色版会把真正的程序放在一层版本目录下，两种结构都试
        roots.push(install.join("resources").join("app").join("extensions"));
        if let Ok(children) = fs::read_dir(&install) {
            for child in children.flatten() {
                roots.push(child.path().join("resources").join("app").join("extensions"));
            }
        }
    }

    // 只留真实存在的目录，并去重（保持上面的优先级顺序）
    let mut seen = HashSet::new();
    roots.retain(|root| root.is_dir() && seen.insert(root.clone()));
    roots
}

/// 遍历所有扩展目录里的 `package.json`，每一条都交给 `visit`。
///
/// ★ 单个扩展读失败 / 格式不对就跳过 —— 一百多个扩展里有一个坏的，
///   不能让整次扫描失败
fn for_each_manifest(roots: &[PathBuf], mut visit: impl FnMut(&Path, ExtensionManifest)) {
    for root in roots {
        // 目录不存在 / 读不了都不是错误，只是「这里没有扩展」
        let Ok(dirs) = fs::read_dir(root) else {
            continue;
        };

        for dir in dirs.flatten() {
            if !dir.path().is_dir() {
                continue;
            }

            let Ok(raw) = fs::read_to_string(dir.path().join("package.json")) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_str::<ExtensionManifest>(&raw) else {
                continue;
            };

            visit(&dir.path(), manifest);
        }
    }
}

/// 扫一遍本机所有放着 VS Code 扩展的地方，把里面贡献的 TextMate 语法列出来。
#[tauri::command]
fn scan_grammar_extensions() -> Result<Vec<GrammarEntry>, String> {
    let roots = extension_roots();

    let mut entries: Vec<GrammarEntry> = Vec::new();

    // ★ 先到先得：同名 scopeName / 同一个语言 id / 同一个扩展名，只认先扫到的那个。
    //   而扫描顺序 = 目录的优先级（用户扩展 > 内置），
    //   所以用户装的扩展能盖住内置的，往下不会反过来
    let mut seen_scopes = HashSet::new();
    let mut seen_languages = HashSet::new();
    let mut seen_extensions = HashSet::new();

    for_each_manifest(&roots, |dir, manifest| {
        let Some(contributes) = manifest.contributes else {
            return;
        };

        for grammar in contributes.grammars {
            // ★ 顺序要紧：**先确认文件真的存在，再占位**。
            //   反过来（先占位）的话，一条「声明了 scopeName 但文件不在」的语法
            //   会把那个 scopeName 白白吃掉，后面真正存在的那个反而进不来。
            //   坑很深：症状是「某个语言莫名没高亮」，而且一声不响
            let path = dir.join(&grammar.path);
            if !path.is_file() {
                continue;
            }

            if !seen_scopes.insert(grammar.scope_name.clone()) {
                continue;
            }

            // 同一个语言只认第一条 —— 不然 Vue 会被后面的扩展抢走
            let id = grammar
                .language
                .filter(|id| seen_languages.insert(id.clone()));

            // 这个语言管到哪些扩展名（同一个扩展名也只认第一次）
            let mut extensions = Vec::new();
            if let Some(id) = &id {
                if let Some(language) = contributes.languages.iter().find(|item| &item.id == id) {
                    for ext in &language.extensions {
                        if seen_extensions.insert(ext.clone()) {
                            extensions.push(ext.clone());
                        }
                    }
                }
            }

            entries.push(GrammarEntry {
                id,
                scope_name: grammar.scope_name,
                // 和文件树一样，跨语言边界统一用 /
                path: path.to_string_lossy().replace('\\', "/"),
                extensions,
                inject_to: grammar.inject_to,
            });
        }
    });

    // 终端里能看见，出问题好排查（Tauri dev 会把 stdout 打出来）
    println!("[语法扩展] 候选扩展根 {} 个：", roots.len());
    for root in &roots {
        println!("[语法扩展]   {}", root.display());
    }
    println!(
        "[语法扩展] 得到 {} 个语法，其中 {} 个挂了具体语言、{} 个是注入语法",
        entries.len(),
        entries.iter().filter(|entry| entry.id.is_some()).count(),
        entries.iter().filter(|entry| !entry.inject_to.is_empty()).count()
    );

    Ok(entries)
}

/// 扫一遍所有扩展，把它们贡献的主题（`contributes.themes`）列出来。
///
/// ★ 这一步不是锦上添花：Monaco 内置的 vs-dark / vs 只有 45 条规则，
///   而 TextMate 的 scope 命名空间大得多 —— `entity.name.tag`、
///   `entity.other.attribute-name` 这些在 Monaco 的主题里根本没有，
///   匹配不到的 token 一律渲染成默认色，看起来就是「颜色很单调」。
///   VS Code 自带的 Dark+ / Light+ 有 169 个规则，正好补上
#[tauri::command]
fn scan_theme_extensions() -> Result<Vec<ThemeEntry>, String> {
    let mut entries: Vec<ThemeEntry> = Vec::new();
    let mut seen_ids = HashSet::new();

    for_each_manifest(&extension_roots(), |dir, manifest| {
        let Some(contributes) = manifest.contributes else {
            return;
        };

        for theme in contributes.themes {
            // 同样：先确认文件真的存在，再占位
            if theme.id.is_empty() || theme.path.is_empty() {
                continue;
            }
            let path = dir.join(&theme.path);
            if !path.is_file() {
                continue;
            }
            if !seen_ids.insert(theme.id.clone()) {
                continue;
            }

            entries.push(ThemeEntry {
                label: if theme.label.is_empty() {
                    theme.id.clone()
                } else {
                    theme.label
                },
                id: theme.id,
                ui_theme: theme.ui_theme,
                path: path.to_string_lossy().replace('\\', "/"),
            });
        }
    });

    println!("[主题扩展] 扫到 {} 个可用主题", entries.len());
    Ok(entries)
}

// ---------- 源代码管理（git）----------
//
// ★ 为什么直接调 `git` 命令，而不是引一个 git 库（git2 / gix）：
//   · git2 要拖一个 libgit2 的 C 依赖 —— 编译慢、体积大，
//     而体积小正是 Tauri 相对 Electron 的主要卖点
//   · 我们只要「看一眼 status」和「拿 HEAD 版本」，命令行完全够用
//   · 用户机器上本来就装着 git（他一直在用它管这个项目）
//
// ⚠ 唯一要防的是 **PATH 里没有 git** —— GUI 应用启动时拿到的环境变量
//   可能和终端里不一样（找 VS Code 安装目录那次已经踩过同一个坑）。
//   所以找不到时要给一句人能看懂的话，而不是一个裸的 os error

#[derive(Serialize)]
struct GitChange {
    /// 绝对路径。前端拿它去读文件、开 diff
    path: String,
    /// 相对仓库根的路径。列表里显示这个 —— 同名文件只能靠它区分
    relative: String,
    /// 两列状态码，比如 "M " / " M" / "??" / "R "
    status: String,
    /// 翻译成人话（「已修改」「未跟踪」…）
    label: String,
}

#[derive(Serialize)]
struct GitStatus {
    /// 这个文件夹是不是 git 仓库。不是的话前端显示一句说明，而不是报错
    is_repo: bool,
    /// 仓库根的绝对路径
    root: String,
    changes: Vec<GitChange>,
}

/// 跑一条 git 命令，拿 stdout
fn run_git(dir: &str, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(args);

    // ★ 不让 Windows 给这个子进程单独开一个控制台窗口。
    //   Tauri 的 exe 是 GUI 子系统、自己**没有**控制台 ——
    //   这种进程启动「控制台程序」（git.exe 就是）时，Windows 会新分配一个，
    //   屏幕上就是黑框闪一下。git_status 每次开文件夹 / 点刷新都要跑一遍，
    //   不关的话会一直闪
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let output = command
        .output()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                "找不到 git 命令。确认一下 git 装在系统 PATH 里 —— \
                 Tauri 应用启动时的 PATH 可能和终端里不一样"
                    .to_string()
            } else {
                format!("执行 git 失败：{error}")
            }
        })?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// 解析 `git status --porcelain=v1 -z` 的输出
///
/// ★ 为什么用 `-z` 而不是默认格式：默认格式会把**含空格或中文的文件名**
///   用引号包起来、还要转义，解析时得处理一堆特殊情况。
///   `-z` 用 NUL 分隔、不做任何转义 —— 名字长什么样就长什么样
///
/// ⚠ 重命名（R）会**多一个字段**：格式是 `XY <新名>\0<旧名>\0`。
///   不把多出来那个吃掉，后面的条目就会**全部错位**
fn parse_git_status(raw: &str, root: &str) -> Vec<GitChange> {
    let mut out = Vec::new();
    let mut parts = raw.split('\0');

    while let Some(entry) = parts.next() {
        // "XY <路径>" 最短也要 4 个字节（两个状态码 + 一个空格 + 一个字符）
        if entry.len() < 4 {
            continue;
        }

        // ★ 按字节切在这里是安全的：前两个字节一定是 ASCII 状态码，
        //   第 3 个字节是空格，所以下标 3 一定落在「文件名的开头」这个字符边界上
        let status = &entry[..2];
        let relative = entry[3..].to_string();

        // 重命名 / 复制：后面还跟着一个「旧路径」字段
        if status.starts_with('R') || status.starts_with('C') {
            parts.next();
        }

        out.push(GitChange {
            path: format!("{root}/{relative}"),
            relative,
            status: status.to_string(),
            label: git_status_label(status),
        });
    }

    out
}

/// 把两列状态码翻译成一句人话
fn git_status_label(status: &str) -> String {
    if status == "??" {
        return "未跟踪".to_string();
    }

    let mut chars = status.chars();
    let staged = chars.next().unwrap_or(' ');
    let worktree = chars.next().unwrap_or(' ');

    let mut parts: Vec<&str> = Vec::new();

    // 第一列 = 暂存区（和 HEAD 比）
    match staged {
        'M' => parts.push("已暂存：修改"),
        'A' => parts.push("已暂存：新增"),
        'D' => parts.push("已暂存：删除"),
        'R' => parts.push("已暂存：重命名"),
        'C' => parts.push("已暂存：复制"),
        _ => {}
    }

    // 第二列 = 工作区（和暂存区比）
    match worktree {
        'M' => parts.push("已修改"),
        'D' => parts.push("已删除"),
        'T' => parts.push("类型变了"),
        _ => {}
    }

    if parts.is_empty() {
        status.to_string()
    } else {
        parts.join("，")
    }
}

/// 列出工作区里所有改动（含未跟踪文件）
#[tauri::command]
async fn git_status(path: String) -> Result<GitStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        // 先问仓库根在哪 —— status 输出里的路径都是**相对仓库根**的，
        // 而前端要的是绝对路径
        let root = match run_git(&path, &["rev-parse", "--show-toplevel"]) {
            Ok(text) => text.trim().replace('\\', "/"),
            // ★ 「不是 git 仓库」是一种**正常状态**，不是错误 ——
            //   用户完全可能随手打开一个普通文件夹
            Err(_) => {
                return Ok(GitStatus {
                    is_repo: false,
                    root: String::new(),
                    changes: Vec::new(),
                })
            }
        };

        let raw = run_git(&path, &["status", "--porcelain=v1", "-z"])?;

        // ★ 必须先把 changes 算出来，再建结构体 —— 上面那个字段会把 `root`
        //   **move** 进去，写在同一行里就成了「先 move、再借用」
        let changes = parse_git_status(&raw, &root);

        Ok(GitStatus {
            is_repo: true,
            root,
            changes,
        })
    })
    .await
    .map_err(|error| format!("查询 git 状态失败：{error}"))?
}

/// 拿一个文件在 HEAD（上一次提交）时的内容 —— 给 diff 当「左边那一份」
///
/// ⚠ 新文件（还没提交过）在 HEAD 里根本不存在。这不是错误：
///   返回空串就行，diff 左边空着，看起来正是「新加了一个文件」
#[tauri::command]
async fn git_show_head(path: String, relative: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let spec = format!("HEAD:{relative}");
        match run_git(&path, &["show", &spec]) {
            Ok(text) => Ok(text),
            Err(_) => Ok(String::new()),
        }
    })
    .await
    .map_err(|error| format!("读取 HEAD 版本失败：{error}"))?
}

// ============================ 语言服务器（LSP）============================
//
// 这一节只干一件事：**把「一个跑在管道上的服务器进程」变成前端能用的消息流**。
// 它不知道 LSP 有哪些方法、也不知道 diagnostics 长什么样 —— 那些在前端的
// `lsp.ts` 里。分界线就是「JSON-RPC 的帧」：Rust 负责帧，前端负责语义。
//
// ★★ 为什么值得这么分：**DAP 用的是同一套分帧**
//   （一样的 `Content-Length: N\r\n\r\n<body>`），只是消息类型不同。
//   所以这一节写完，DAP 那边的传输层是白送的 —— 换个 emit 事件名就能用。
//
// ★ 为什么必须放 Rust 而不是前端直接连：浏览器 / WebView 没法起子进程，
//   也没法读写它的 stdio。这是「桌面版才能做的事」里最典型的一件。

/// 读**一帧**。
///
/// LSP 的分帧格式是 HTTP 那套的简化版：
/// ```text
/// Content-Length: 42\r\n
/// \r\n
/// {"jsonrpc":"2.0",…}
/// ```
/// 可以有多余的头（比如 `Content-Type`），我们只认必需的 `Content-Length`。
///
/// ⚠ ★★ `Content-Length` 是**字节数**，不是字符数。
///   含中文的 JSON（比如一个中文错误消息）用 `.chars().count()` 算就会短一截，
///   于是下一次读会从半个字符中间开始 —— 后面**所有**消息全部错位，
///   而且报错信息完全指不到这里
///
/// 返回 `Ok(None)` 表示对端关掉了管道（进程退出），是正常结束不是错误
fn read_frame(reader: &mut impl BufRead) -> Result<Option<String>, String> {
    let mut length: Option<usize> = None;
    // 已经读到过至少一个头 —— 用来区分「头部结束的空行」和「帧之间多出来的空行」
    let mut saw_header = false;

    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| format!("读取 LSP 头部失败：{error}"))?;
        // 读到 EOF 且一个字节都没有 = 对端关了
        if read == 0 {
            return Ok(None);
        }

        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            // ★ 帧与帧之间**没有**分隔符（一个帧的正文后面直接就是下一个帧的头），
            //   所以正常情况下这里不应该出现空行。
            //   但真发出来了也不该把整个会话弄挂 —— 当成噪声跳过，等真正的头
            if saw_header {
                break; // 头部结束，下面是正文
            }
            continue;
        }
        saw_header = true;

        // 头名字大小写不敏感（规范就是这么写的），其余的头一律忽略
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = Some(
                    value
                        .trim()
                        .parse()
                        .map_err(|_| format!("Content-Length 不是数字：{value}"))?,
                );
            }
        }
    }

    let Some(length) = length else {
        return Err("LSP 头部里没有 Content-Length".to_string());
    };

    let mut buffer = vec![0u8; length];
    reader
        .read_exact(&mut buffer)
        .map_err(|error| format!("读取 LSP 正文失败：{error}"))?;

    // ★ 到这一步 buffer 里**一定是**完整的 UTF-8 ——
    //   因为我们按字节数读满了整帧，不会截在字符中间。
    //   （这也是为什么正文要单独 read_exact，不能和头部一起 read_line）
    Ok(Some(String::from_utf8_lossy(&buffer).into_owned()))
}

/// 一个可用的语言服务器
#[derive(Serialize, Clone)]
struct LspServerInfo {
    /// 稳定标识（"json" / "rust-analyzer" …）。前端拿它当会话 id 的一部分
    id: String,
    /// 给人看的名字（菜单 / 状态栏里显示）
    label: String,
    /// 这个服务器管哪些语言 id。★ 要和语法扫描给出的语言 id 对得上
    languages: Vec<String>,
    /// 可执行文件（node.exe / rust-analyzer.exe …）
    program: String,
    args: Vec<String>,
}

/// 写端。★ stdio 和 TCP 两种形态——上面那层（`write_frame`）不必知道区别，
/// 把差异圈在这一个枚举里
enum BridgeWriter {
    /// 子进程的 stdin（LSP 和 debugpy 的默认形态）
    Stdio(ChildStdin),
    /// 一个 TCP 连接（适配器**自己监听端口**的那种，比如 debugpy 的 `--port` 模式）。
    /// ⚠ 选它是因为：有些适配器**只**提供端口模式（尤其 js-debug 那一系）
    Tcp(TcpStream),
}

impl Write for BridgeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Stdio(writer) => writer.write(buf),
            Self::Tcp(writer) => writer.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Stdio(writer) => writer.flush(),
            Self::Tcp(writer) => writer.flush(),
        }
    }
}

/// 跑起来的桥。**LSP 和 DAP 共用同一个形状** ——
/// 两边都是「一个子进程（或一个 socket）+ 一套 Content-Length 分帧」，没什么可分的
struct BridgeSession {
    /// 留着是为了 stop 时能 kill —— 不存的话进程就没人管了。
    /// ⚠ TCP 模式可能**没有**子进程（适配器早就跑在别处了），所以是 Option
    child: Option<StdChild>,
    /// ★ 包成 Arc<Mutex<…>> 是为了「拿得到就行」：
    ///   发消息时不必在整个写操作期间占着 sessions 那把大锁
    writer: Arc<Mutex<BridgeWriter>>,
}

/// 会话表
///
/// ★ 里面那层是 `Arc<Mutex<…>>` 而不是裸的 `Mutex<…>`：
///   连 TCP 端口是**阻塞**且要重试的，得放到 `spawn_blocking` 里干，
///   而 `State` 不让移出去 —— 所以能移的只有这个 Arc
#[derive(Default)]
struct BridgeState {
    sessions: Arc<Mutex<HashMap<String, BridgeSession>>>,
}

/// ★ 为什么不干脆 `manage(BridgeState::default())` 两次：
///   Tauri 的 State 是**按类型**存的，同一种类型只能有一份。
///   两个 newtype 各存一份，才能让「给 DAP 会话发 LSP 消息」这类串台
///   在类型上就不可能发生（就算两边的会话 id 撞名也不会互相踩）
#[derive(Default)]
struct LspState(BridgeState);

#[derive(Default)]
struct DapState(BridgeState);

/// 发给前端的每条消息
#[derive(Serialize, Clone)]
struct BridgeMessage {
    session: String,
    /// 原始 JSON 文本。★ Rust 不解析它 —— 解析留前端，
    /// 这样改协议版本 / 加方法都不用重编 Rust
    body: String,
}

/// 写一帧
fn write_frame(writer: &mut impl Write, body: &str) -> std::io::Result<()> {
    // ⚠ 同样：这里是**字节**长度
    write!(writer, "Content-Length: {}\r\n\r\n", body.as_bytes().len())?;
    writer.write_all(body.as_bytes())?;
    // ⚠ flush 不能省 —— 不刷的话消息会缓在缓冲区里，
    //   服务器那边就一直在等，表现为「发了初始化但永远没回应」
    writer.flush()
}

/// 枚举本机可用的语言服务器。
///
/// ★ 思路和语法 / 主题那两块一样：**不自己打包服务器，用机器上已有的**。
///   · VS Code 自带 json / html / css 三个服务器（是真 LSP，用 node 起）
///   · 用户 PATH 里的 rust-analyzer 之类也认
fn find_lsp_servers() -> Vec<LspServerInfo> {
    let mut servers: Vec<LspServerInfo> = Vec::new();

    // node 在哪儿 —— VS Code 自带的三个服务器都要靠它跑
    let node = which("node");

    // ---- 1) VS Code 自带的三个 ----
    // 路径是 <安装目录>/resources/app/extensions/<x>-language-features/server/dist/node/<x>ServerMain.js
    // ⚠ 绿色版会多一层版本哈希目录，所以不能直接拼 —— 得枚举候选
    if let Some(node) = &node {
        for (id, label, languages, dir) in [
            ("json", "JSON", &["json", "jsonc"][..], "json-language-features"),
            ("html", "HTML", &["html"][..], "html-language-features"),
            ("css", "CSS", &["css", "scss", "less"][..], "css-language-features"),
        ] {
            let Some(script) = find_bundled_server(dir, id) else {
                continue;
            };
            servers.push(LspServerInfo {
                id: id.to_string(),
                label: label.to_string(),
                languages: languages.iter().map(|s| s.to_string()).collect(),
                program: node.clone(),
                // `--stdio` 是 vscode-languageserver 认的开关：
                // 不加的话它默认也是 stdio，但显式写出来更清楚
                args: vec![script, "--stdio".to_string()],
            });
        }
    }

    // ---- 2) PATH 里的独立服务器 ----
    for (bin, id, label, languages) in [
        ("rust-analyzer", "rust", "Rust", &["rust"][..]),
        ("typescript-language-server", "typescript", "TypeScript", &["typescript", "javascript"][..]),
        ("pyright-langserver", "python", "Python", &["python"][..]),
    ] {
        let Some(program) = which(bin) else { continue };
        // pyright 要显式给 --stdio；其余的直接跑
        let args = if bin == "pyright-langserver" {
            vec!["--stdio".to_string()]
        } else {
            Vec::new()
        };
        servers.push(LspServerInfo {
            id: id.to_string(),
            label: label.to_string(),
            languages: languages.iter().map(|s| s.to_string()).collect(),
            program,
            args,
        });
    }

    servers
}

/// 在 VS Code 的安装目录里找 `<ext>/server/dist/node/<name>ServerMain.js`
fn find_bundled_server(extension: &str, name: &str) -> Option<String> {
    let relative = Path::new("resources")
        .join("app")
        .join("extensions")
        .join(extension)
        .join("server")
        .join("dist")
        .join("node")
        .join(format!("{name}ServerMain.js"));

    for root in vscode_install_roots() {
        // 标准安装：根目录下就是 resources/…
        let direct = root.join(&relative);
        if direct.is_file() {
            return Some(direct.to_string_lossy().into_owned());
        }

        // 绿色版：根目录下还有一层版本目录
        let Ok(children) = fs::read_dir(&root) else {
            continue;
        };
        for child in children.flatten() {
            let nested = child.path().join(&relative);
            if nested.is_file() {
                return Some(nested.to_string_lossy().into_owned());
            }
        }
    }

    None
}

/// 在 PATH 里找一个可执行文件。
///
/// ★ 为什么不用 `which` 那个 crate：就这么几行，引一个依赖不值当
/// ⚠ Windows 上要试 PATHEXT 里的后缀（.exe / .cmd / .bat …），
///   否则 `which("node")` 永远找不到 node
fn which(bin: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    let suffixes: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_lowercase())
            .collect()
    } else {
        vec![String::new()]
    };

    for dir in std::env::split_paths(&path) {
        for suffix in &suffixes {
            let candidate = dir.join(format!("{bin}{suffix}"));
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }

    None
}

/// 列出可用的语言服务器
#[tauri::command]
async fn lsp_servers() -> Vec<LspServerInfo> {
    // 要遍历目录、查 PATH，算阻塞 IO —— 别占住 async 的线程
    tauri::async_runtime::spawn_blocking(find_lsp_servers)
        .await
        .unwrap_or_default()
}

/// 起一个 stdio 桥，返回会话 id。
///
/// ★★ 这是 LSP 和 DAP **共用**的那一段：两边都是「起个子进程，
///   说话走 `Content-Length` 分帧的 JSON」。DAP 就是 LSP 的兄弟协议，
///   连分帧都一样 —— 所以只写一份。
///   区别只在**事件名**（`lsp-message` / `dap-message`）和程序参数。
fn start_bridge(
    app: AppHandle,
    sessions: &Mutex<HashMap<String, BridgeSession>>,
    session: String,
    program: &str,
    args: &[String],
    cwd: Option<&str>,
    channel: &'static str,
) -> Result<(), String> {
    let mut command = Command::new(program);
    command.args(args);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    // cwd 只在目录真的存在时才设 —— 设一个不存在的路径会让 spawn 整个失败
    //（终端那块踩过同一个坑）
    if let Some(dir) = cwd.filter(|d| Path::new(d).is_dir()) {
        command.current_dir(dir);
    }

    // ⚠ Windows 上不给这个的话会闪一个黑框（和 git 命令同一个坑）
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = command.spawn().map_err(|error| format!("启动子进程失败：{error}"))?;

    let stdin = child.stdin.take().ok_or("拿不到子进程的 stdin")?;
    let stdout = child.stdout.take().ok_or("拿不到子进程的 stdout")?;
    let stderr = child.stderr.take().ok_or("拿不到子进程的 stderr")?;

    spawn_bridge_reader(app.clone(), session.clone(), stdout, channel);
    spawn_bridge_stderr(app, session.clone(), stderr, channel);

    sessions.lock().unwrap().insert(
        session,
        BridgeSession {
            child: Some(child),
            writer: Arc::new(Mutex::new(BridgeWriter::Stdio(stdin))),
        },
    );

    Ok(())
}

/// 连到一个**已经在监听端口**的调试适配器。
///
/// ★★ 为什么要重试：适配器刚被拉起来时，端口可能还没进入监听状态。
///   只连一次的话会**偶发**失败，而且报错是「拒绝连接」，看起来像端口写错了
/// ⇒ 给一个总超时（默认 8 秒），里面每隔 100ms 试一次
///
/// ★ 这种模式下**没有子进程**：适配器是别人（用户自己 / 另一个工具）拉起来的，
///   我们只是接上去。所以 `stop` 时只关连接，不去 kill 谁
fn connect_bridge(
    app: AppHandle,
    sessions: &Mutex<HashMap<String, BridgeSession>>,
    session: String,
    host: &str,
    port: u16,
    channel: &'static str,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    // ★ 不初始化成空串：那样编译器会提示「赋了但没读过」
    let mut last_error;

    loop {
        match TcpStream::connect((host, port)) {
            Ok(stream) => {
                // ★ 读写要各用一份：try_clone 出来的是**同一个连接**的另一个句柄
                let reader = stream
                    .try_clone()
                    .map_err(|error| format!("复制 socket 句柄失败：{error}"))?;

                spawn_bridge_reader(app, session.clone(), reader, channel);

                sessions.lock().unwrap().insert(
                    session,
                    BridgeSession {
                        child: None,
                        writer: Arc::new(Mutex::new(BridgeWriter::Tcp(stream))),
                    },
                );
                return Ok(());
            }
            Err(error) => {
                last_error = error.to_string();
                if std::time::Instant::now() > deadline {
                    return Err(format!("连不上 {host}:{port}（重试了 8 秒）：{last_error}"));
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}

/// 读子进程 / socket 的输出，一帧一条事件发给前端。
///
/// ★ 参数写成 `impl Read`：stdio（`ChildStdout`）和 TCP（`TcpStream`）都是它 ——
///   于是这两种传输的**读循环只需要一份代码**
fn spawn_bridge_reader<R>(app: AppHandle, session: String, input: R, channel: &'static str)
where
    R: Read + Send + 'static,
{
    // ★ 必须开独立线程：`read` 是阻塞的，挂在命令里会把命令线程整个卡死
    //   （终端的 pty 那块踩过同一个坑）
    std::thread::spawn(move || {
        let mut reader = BufReader::new(input);
        loop {
            match read_frame(&mut reader) {
                Ok(Some(body)) => {
                    let _ = app.emit(
                        &format!("{channel}-message"),
                        BridgeMessage {
                            session: session.clone(),
                            body,
                        },
                    );
                }
                // EOF = 对端退出了，正常结束
                Ok(None) => break,
                Err(error) => {
                    let _ = app.emit(&format!("{channel}-error"), format!("{session}: {error}"));
                    break;
                }
            }
        }
        // 不管怎么结束都通知一声 —— 前端要拿它把「已连接」状态收回去
        let _ = app.emit(&format!("{channel}-exit"), session);
    });
}

/// stderr 也得有人读。
///
/// ⚠ 不读的话管道缓冲区会满，对端一写日志就**卡死在那儿** ——
///   表现是「起来了一会儿就没反应了」，而 stdout 那边什么都看不到
///   （debugpy 尤其能写，它所有内部报错都往 stderr 倒）
fn spawn_bridge_stderr(app: AppHandle, session: String, stderr: ChildStderr, channel: &'static str) {
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            let _ = app.emit(&format!("{channel}-stderr"), format!("[{session}] {line}"));
        }
    });
}

/// 往指定的会话发一帧
fn bridge_send(
    sessions: &Mutex<HashMap<String, BridgeSession>>,
    session: &str,
    message: &str,
    what: &str,
) -> Result<(), String> {
    // ★ 先把写端的 Arc 取出来、把 sessions 那把锁放掉，再去写 ——
    //   写管道 / socket 是会阻塞的，握着大锁写会把别的会话一起堵住
    let writer = {
        let found = sessions.lock().unwrap();
        let entry = found
            .get(session)
            .ok_or_else(|| format!("没有这个会话：{session}"))?;
        Arc::clone(&entry.writer)
    };

    let mut writer = writer.lock().unwrap();
    // ⚠ `&mut *writer` 而不是 `&mut writer`：MutexGuard 本身不是 Write，
    //   要显式解引用成里面的写端
    write_frame(&mut *writer, message).map_err(|error| format!("写给{what}失败：{error}"))?;
    Ok(())
}

/// 关掉一个会话（有子进程就把它杀掉）
fn bridge_stop(sessions: &Mutex<HashMap<String, BridgeSession>>, session: &str) {
    let mut found = sessions.lock().unwrap();
    if let Some(mut entry) = found.remove(session) {
        // ⚠ TCP 模式（接别人的适配器）没有子进程 —— 不判一下会直接 panic。
        //   那种情况下把连接丢掉（writer 被 drop）就是正确的「断开」
        if let Some(child) = entry.child.as_mut() {
            // kill 失败不是错误 —— 进程可能已经自己退了
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// 关掉**所有**会话。
///
/// ★★ 为什么需要它：前端**刷新页面**时（开发时改一行就 HMR、或者用户按 F5），
///   旧页面的 `onUnmounted` 里那个 dispose 根本不会执行 ——
///   页面是被直接拆掉的，不是「卸载」。于是 Rust 侧那些进程全成了孤儿：
///   没人 stop、也没人会再给它们发消息。
///   新页面起来时先清一次，就不会一代一代攒下来。
///
/// ⚠ 还有个更隐蔽的后果：进程虽然「活着」，但它的 stdin 只有 Rust 手里那一份。
///   谁也发不了消息，等于白白占着内存。
fn bridge_stop_all(sessions: &Mutex<HashMap<String, BridgeSession>>) {
    let all: Vec<BridgeSession> = {
        let mut found = sessions.lock().unwrap();
        // ★ 先 drain 出来、把锁放掉，再去 kill + wait ——
        //   wait 是阻塞的，握着锁等子进程退出会把别的命令一起堵住
        found.drain().map(|(_, session)| session).collect()
    };

    for mut entry in all {
        if let Some(child) = entry.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// 起一个语言服务器
#[tauri::command]
async fn lsp_start(
    app: AppHandle,
    state: State<'_, LspState>,
    session: String,
    server_id: String,
    cwd: Option<String>,
) -> Result<(), String> {
    let info = find_lsp_servers()
        .into_iter()
        .find(|s| s.id == server_id)
        .ok_or_else(|| format!("找不到语言服务器：{server_id}"))?;

    start_bridge(app, &state.0.sessions, session, &info.program, &info.args, cwd.as_deref(), "lsp")
}

/// 往前端指定的会话发一帧
#[tauri::command]
async fn lsp_send(state: State<'_, LspState>, session: String, message: String) -> Result<(), String> {
    bridge_send(&state.0.sessions, &session, &message, "语言服务器")
}

/// 关掉一个会话（同时把进程杀掉）
#[tauri::command]
async fn lsp_stop(state: State<'_, LspState>, session: String) -> Result<(), String> {
    bridge_stop(&state.0.sessions, &session);
    Ok(())
}

/// 关掉**所有**会话（刷新页面时先清一次，见 bridge_stop_all 的说明）
#[tauri::command]
async fn lsp_stop_all(state: State<'_, LspState>) -> Result<(), String> {
    bridge_stop_all(&state.0.sessions);
    Ok(())
}

// ============================ 调试（DAP） ============================
//
// ★★ DAP 和 LSP 的关系：**兄弟协议**。
//   · 分帧一模一样（`Content-Length` + JSON），所以传输层直接复用上面那套
//   · 区别在语义：LSP 管「代码长什么样」，DAP 管「程序跑到哪儿了」
//   · 前端也是一样：`dap.ts` 和 `lsp.ts` 同构，但请求/事件的形状完全不同
//
// ★ 为什么不自己写调试器：调试器的核心是跟**操作系统**打交道
//   （断点 = 改机器码 / ptrace / 系统调试 API），那是另一个量级的工程。
//   业界统一的做法就是「说 DAP 这门语言，让现成的适配器去干活」——
//   VS Code 那几百个 debugger 扩展全是这么实现的

/// 一个可用的调试适配器
#[derive(Serialize, Clone)]
struct DapAdapterInfo {
    /// 稳定标识（"debugpy" …）。前端拿它当会话 id 的一部分
    id: String,
    /// 给人看的名字
    label: String,
    /// 管哪些扩展名。前端拿它决定「当前这个文件能不能调试」
    extensions: Vec<String>,
    program: String,
    args: Vec<String>,
}

/// 适配器清单只问一次 —— 每个候选都要真起一个进程问一遍「装没装」，不便宜
static DAP_ADAPTERS: OnceLock<Vec<DapAdapterInfo>> = OnceLock::new();

/// 问一下这个 python 里有没有 debugpy。
///
/// ⚠ ★ 必须带超时：这不是「一定会结束」的命令。
///   Windows 上 `python3` 常常是应用商店的**别名占位**，一跑就等用户去装 ——
///   没有超时的话这个命令会把调它的那条线程永久占住
fn has_debugpy(program: &str) -> bool {
    let mut command = Command::new(program);
    command.args(["-c", "import debugpy"]);
    command.stdin(Stdio::null());
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let Ok(mut child) = command.spawn() else {
        return false;
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => return false,
        }
    }
}

/// 枚举本机可用的调试适配器。
///
/// ★ 和语言服务器一个思路：**不自己打包适配器，用机器上已有的**。
///   目前只认 debugpy（Python）—— 它是标准的 **stdio** DAP，
///   和我们这套传输层天然合拍。
///
/// ⚠ js-debug（Node 那个，VS Code 自带）也在这台机器上，但它**不是 stdio**：
///   它要监听一个端口、用 socket 说话 ⇒ 得先给传输层加 TCP 支持。
///   那是下一步的事（这个列表加一项就行）
fn find_dap_adapters() -> Vec<DapAdapterInfo> {
    let mut adapters = Vec::new();

    // ⚠ 只试 `python` 和 `py`，**故意不试 `python3`**：
    //   Windows 上它常常是应用商店的别名占位，跑起来只会弹商店
    for (bin, id) in [("python", "debugpy"), ("py", "debugpy-py")] {
        let Some(program) = which(bin) else { continue };
        // ★ 光有 python 不够，还得确认 debugpy 真的装了 ——
        //   这是**环境事实**，只有真问一下才知道。
        //   不问的话，用户要点「启动调试」才会看到一句难懂的 ModuleNotFoundError
        if !has_debugpy(&program) {
            continue;
        }
        adapters.push(DapAdapterInfo {
            id: id.to_string(),
            label: format!("Python（{bin} + debugpy）"),
            extensions: vec!["py".to_string()],
            program,
            // `python -m debugpy.adapter` 就是 VS Code 的 Python 扩展用的那个入口
            args: vec!["-m".to_string(), "debugpy.adapter".to_string()],
        });
    }

    adapters
}

fn dap_adapters_cached() -> Vec<DapAdapterInfo> {
    DAP_ADAPTERS.get_or_init(find_dap_adapters).clone()
}

/// 列出可用的调试适配器
#[tauri::command]
async fn dap_adapters() -> Vec<DapAdapterInfo> {
    // 要真起进程问「装没装」，算阻塞 IO —— 别占住 async 的线程
    tauri::async_runtime::spawn_blocking(dap_adapters_cached)
        .await
        .unwrap_or_default()
}

/// 起一个调试适配器
#[tauri::command]
async fn dap_start(
    app: AppHandle,
    state: State<'_, DapState>,
    session: String,
    adapter_id: String,
    cwd: Option<String>,
) -> Result<(), String> {
    let info = dap_adapters_cached()
        .into_iter()
        .find(|a| a.id == adapter_id)
        .ok_or_else(|| format!("找不到调试适配器：{adapter_id}"))?;

    start_bridge(app, &state.0.sessions, session, &info.program, &info.args, cwd.as_deref(), "dap")
}

/// 连到一个**已经在监听端口**的调试适配器。
///
/// ★★ 为什么值得单独做一个命令：有些适配器**只**提供端口模式 ——
///   js-debug 那一系的 `DebugAdapterServer`、跑在别的机器上的适配器等。
///   stdio 那条路对它们无能为力（实测：VS Code 自带的 js-debug 在
///   这台机器上**没有**可独立启动的 DAP 服务器入口，`node bootloader.js`
///   直接就退了；它只接受扩展宿主驱动）。
///   但带 `--port` 的适配器（比如 debugpy 的 debugServer 模式）就能这么接
///
/// ★ 握手流程和 stdio 模式**一模一样**（实测）：
///   initialize → launch（不等响应）→ initialized → setBreakpoints → configurationDone ——
///   换的只是传输层
///
/// ⚠ 连接是**阻塞且要重试**的（最多 8 秒），所以丢进 spawn_blocking ——
///   别占住 async 的线程（和 `lsp_servers` 一个理由）
#[tauri::command]
async fn dap_attach(
    app: AppHandle,
    state: State<'_, DapState>,
    session: String,
    host: String,
    port: u16,
) -> Result<(), String> {
    let sessions = Arc::clone(&state.0.sessions);
    let target = host.clone();

    tauri::async_runtime::spawn_blocking(move || {
        connect_bridge(app, &sessions, session, &target, port, "dap")
    })
    .await
    .map_err(|error| format!("连接调试端口失败：{error}"))?
}

#[tauri::command]
async fn dap_send(state: State<'_, DapState>, session: String, message: String) -> Result<(), String> {
    bridge_send(&state.0.sessions, &session, &message, "调试适配器")
}

#[tauri::command]
async fn dap_stop(state: State<'_, DapState>, session: String) -> Result<(), String> {
    bridge_stop(&state.0.sessions, &session);
    Ok(())
}

/// 关掉所有调试会话（刷新页面时先清一次）
#[tauri::command]
async fn dap_stop_all(state: State<'_, DapState>) -> Result<(), String> {
    bridge_stop_all(&state.0.sessions);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- 语言服务器的分帧 ----------

    #[test]
    fn reads_a_simple_frame() {
        let raw = "Content-Length: 7\r\n\r\n{\"a\":1}";
        let mut reader = BufReader::new(raw.as_bytes());
        let frame = read_frame(&mut reader).unwrap();
        assert_eq!(frame.as_deref(), Some("{\"a\":1}"));
    }

    #[test]
    fn reads_several_frames_in_a_row() {
        // ★ 帧与帧之间**没有**分隔符 —— 一个帧的正文后面直接跟着下一个帧的头。
        //   （我第一版测试在这里多写了一个 \r\n，结果把正确的实现测挂了）
        let raw = "Content-Length: 2\r\n\r\n[]Content-Length: 3\r\n\r\n\"x\"";
        let mut reader = BufReader::new(raw.as_bytes());
        assert_eq!(read_frame(&mut reader).unwrap().as_deref(), Some("[]"));
        assert_eq!(read_frame(&mut reader).unwrap().as_deref(), Some("\"x\""));
    }

    #[test]
    fn tolerates_a_stray_blank_line_between_frames() {
        // 不合规范但真有实现会多吐一个空行。跳过它，不要把整个会话弄挂
        let raw = "Content-Length: 2\r\n\r\n[]\r\nContent-Length: 3\r\n\r\n\"x\"";
        let mut reader = BufReader::new(raw.as_bytes());
        assert_eq!(read_frame(&mut reader).unwrap().as_deref(), Some("[]"));
        assert_eq!(read_frame(&mut reader).unwrap().as_deref(), Some("\"x\""));
    }

    #[test]
    fn content_length_counts_bytes_not_chars() {
        // ★★ 这是最容易写错的一处：
        //   "一" 只有 1 个字符，但 UTF-8 下是 3 个字节。
        //   用 chars().count() 算头部就会写 1，于是下一次读从半个字中间开始 ——
        //   后面所有消息全部错位，而且报错信息指不到这里。
        //   中文报错消息在真实服务器里很常见（比如 tsserver），必然踩到
        let body = "无";
        let mut framed: Vec<u8> = Vec::new();
        write_frame(&mut framed, body).unwrap();

        let text = String::from_utf8(framed.clone()).unwrap();
        assert_eq!(text, "Content-Length: 3\r\n\r\n无");

        // 而且真的能读回来
        let mut reader = BufReader::new(framed.as_slice());
        assert_eq!(read_frame(&mut reader).unwrap().as_deref(), Some(body));
    }

    #[test]
    fn ignores_extra_headers() {
        // 规范允许多余的头（Content-Type 之类），只认 Content-Length 就行
        let raw = "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: 2\r\nX-Other: 1\r\n\r\n{}";
        let mut reader = BufReader::new(raw.as_bytes());
        assert_eq!(read_frame(&mut reader).unwrap().as_deref(), Some("{}"));
    }

    #[test]
    fn header_name_is_case_insensitive() {
        let raw = "content-length: 2\r\n\r\n[]";
        let mut reader = BufReader::new(raw.as_bytes());
        assert_eq!(read_frame(&mut reader).unwrap().as_deref(), Some("[]"));
    }

    #[test]
    fn eof_is_not_an_error() {
        // 对端关掉管道 = 进程退出了，是正常结束
        let mut reader = BufReader::new(&b""[..]);
        assert!(read_frame(&mut reader).unwrap().is_none());
    }

    /// 真的把 VS Code 自带的 JSON 语言服务器起起来，走一遍完整流程。
    ///
    /// ★ 为什么值得写这个：分帧那几条用例只证明「我自己写的读和写能互相配合」，
    ///   证明不了「读的东西真是别人写的」。这个测试拿**真实的服务器**验证，
    ///   而且顺带把 initialize 握手、didOpen、诊断推送整条链路都跑了一遍。
    ///
    /// ⚠ 机器上没有 VS Code / node 时**直接返回** —— 这不是失败，
    ///   只是这台机器没这个条件（和 finds_vscode_builtin_extensions 一个态度）
    #[test]
    fn talks_to_the_real_json_language_server() {
        let Some(script) = find_bundled_server("json-language-features", "json") else {
            return;
        };
        let Some(node) = which("node") else { return };

        let mut child = Command::new(node)
            .arg(&script)
            .arg("--stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("起不来 JSON 语言服务器");
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        // ★★ 读消息要放在**独立线程**里，主线程用 recv_timeout 收。
        //   直接在主线程 read_frame 的话，服务器一旦不说话测试就**永远挂着** ——
        //   而「测试挂住」比「测试失败」难查得多（没有报错，只有一直转圈）
        let (sender, receiver) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(Some(frame)) = read_frame(&mut reader) {
                if sender.send(frame).is_err() {
                    break;
                }
            }
        });

        let wait_for = |receiver: &std::sync::mpsc::Receiver<String>, needle: &str| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    return false;
                }
                let Ok(frame) = receiver.recv_timeout(left) else {
                    return false;
                };
                if frame.contains(needle) {
                    return true;
                }
            }
        };

        // 1) 握手。回应里必须带 capabilities —— 那是服务器能力的清单
        write_frame(
            &mut stdin,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"processId":null,"rootUri":null,"capabilities":{}}}"#,
        )
        .unwrap();
        assert!(
            wait_for(&receiver, "\"capabilities\""),
            "服务器没有正确回应 initialize"
        );

        // 2) initialized 通知（告诉它可以开始干活了）
        write_frame(
            &mut stdin,
            r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
        )
        .unwrap();

        // 3) 打开一份**语法错的** JSON，等它推诊断回来。
        //    ★ 这里用 `{ "a": }` —— 少了一个值，是 json 解析器一定会报的错
        write_frame(
            &mut stdin,
            r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///d%3A/no/such/broken.json","languageId":"json","version":1,"text":"{ \"a\": }"}}}"#,
        )
        .unwrap();

        assert!(
            wait_for(&receiver, "publishDiagnostics"),
            "没等到服务器推诊断"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn finds_the_bundled_language_servers() {
        // 条件不具备时跳过（见上一个测试的说明）
        if which("node").is_none() {
            return;
        }
        let servers = find_lsp_servers();
        // 有 VS Code 就应该至少认出 json / html / css 三个之一
        if find_bundled_server("json-language-features", "json").is_some() {
            let json = servers.iter().find(|s| s.id == "json");
            assert!(json.is_some(), "没认出 JSON 语言服务器");
            // ★ 语言 id 必须和语法扫描给出的对得上，否则永远匹配不上
            assert!(json.unwrap().languages.contains(&"json".to_string()));
        }
    }

    // ---------- 源代码管理（git status 的解析）----------

    /// 常规条目：工作区改动 / 暂存区改动 / 未跟踪
    #[test]
    fn parses_git_status_output() {
        let raw = " M src/App.vue\0A  new.ts\0?? 没跟踪的文件.txt\0";
        let changes = parse_git_status(raw, "D:/demo");

        assert_eq!(changes.len(), 3);

        assert_eq!(changes[0].path, "D:/demo/src/App.vue");
        assert_eq!(changes[0].relative, "src/App.vue");
        assert_eq!(changes[0].label, "已修改");

        assert_eq!(changes[1].label, "已暂存：新增");

        // ★ 空格和中文都原样保留 —— 这正是用 `-z` 的理由：
        //   默认格式会把这种文件名加引号加转义，解析要写一堆特例
        assert_eq!(changes[2].relative, "没跟踪的文件.txt");
        assert_eq!(changes[2].path, "D:/demo/没跟踪的文件.txt");
        assert_eq!(changes[2].label, "未跟踪");
    }

    /// ★ 重命名会多一个字段，不跳过的话后面的条目会**全部错位**
    #[test]
    fn handles_rename_entries() {
        let raw = "R  new-name.ts\0old-name.ts\0 M other.ts\0";
        let changes = parse_git_status(raw, "D:/demo");

        assert_eq!(changes.len(), 2, "重命名的旧路径不该被当成一条改动");
        assert_eq!(changes[0].relative, "new-name.ts");
        assert_eq!(changes[0].label, "已暂存：重命名");
        assert_eq!(changes[1].relative, "other.ts");
        assert_eq!(changes[1].label, "已修改");
    }

    /// 两列都有值时要把两边都说清楚
    #[test]
    fn labels_both_columns() {
        assert_eq!(git_status_label("MM"), "已暂存：修改，已修改");
        assert_eq!(git_status_label(" M"), "已修改");
        assert_eq!(git_status_label("M "), "已暂存：修改");
        assert_eq!(git_status_label("??"), "未跟踪");
    }

    #[test]
    fn handles_empty_git_status() {
        assert!(parse_git_status("", "D:/demo").is_empty());
    }

    // ---------- 全文搜索 ----------

    /// ★ 最容易错的一条：下标不能按**字节**算。
    ///   中文一个字在 UTF-8 里占 3 字节 —— 按字节当列号会得到 7 而不是 3
    #[test]
    fn search_units_are_not_bytes() {
        let hits = search_in_text("你好世界", "世界", true);
        assert_eq!(hits.len(), 1);
        let (line, column, text, start, end) = &hits[0];
        assert_eq!(*line, 1);
        assert_eq!(*column, 3, "不能按字节算：按字节会得到 7");
        assert_eq!(text, "你好世界");
        assert_eq!((*start, *end), (2, 4));
    }

    /// ★ emoji 是「1 个 char、2 个 UTF-16 单位」。
    ///   对中文（BMP）字符数和 UTF-16 长度恰好相等，所以中文用例盖不住这个差别 ——
    ///   得专门拿补充平面的字符来试
    #[test]
    fn search_counts_utf16_units_like_javascript() {
        // 😀 = U+1F600：chars().count() 是 1，encode_utf16().count() 是 2
        let hits = search_in_text("😀ok", "ok", true);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, 3, "列号：emoji 占 2 个 UTF-16 单位，所以 ok 从第 3 列开始");
        assert_eq!((hits[0].3, hits[0].4), (2, 4), "高亮下标也要和 JS 的 slice 对齐");
    }

    /// ★ 两条下标的口径不一样，很容易搞混：
    ///   列号是「相对原始行」的（跳转用），高亮下标是「相对展示文本」的（trim 过）
    #[test]
    fn search_offsets_highlight_by_leading_whitespace() {
        let hits = search_in_text("    let value = 1;", "value", true);
        let (_, column, text, start, end) = &hits[0];
        assert_eq!(text, "let value = 1;");
        assert_eq!(*column, 9, "列号相对原始行，不减空白");
        assert_eq!((*start, *end), (4, 9), "高亮下标相对展示文本，要减掉前导空白");
    }

    #[test]
    fn search_finds_every_hit_on_a_line() {
        let hits = search_in_text("a b a b a", "a", true);
        assert_eq!(hits.len(), 3);
        let starts: Vec<usize> = hits.iter().map(|hit| hit.3).collect();
        assert_eq!(starts, vec![0, 4, 8]);
    }

    #[test]
    fn search_respects_case_sensitivity() {
        assert_eq!(search_in_text("Hello", "hello", true).len(), 0);
        assert_eq!(search_in_text("Hello", "hello", false).len(), 1);
        // 大小写不敏感时，返回的仍然是**原文**，不是小写版 ——
        // 不然结果列表里会显示一段被改过大小写的代码
        assert_eq!(search_in_text("Hello", "hell", false)[0].2, "Hello");
    }

    #[test]
    fn search_handles_empty_and_missing() {
        assert!(search_in_text("abc", "", true).is_empty());
        assert!(search_in_text("abc", "xyz", true).is_empty());
    }

    /// 自检：VS Code 程序目录里的内置扩展能不能被找到。
    ///
    /// 那是 html / css / typescript 这些**父语法**的家 —— 找不到它们，
    /// Vue 这类「寄生语法」就会缺一大块高亮。
    ///
    /// 跑：cargo test -- --nocapture
    /// ★ 验证清单里的 `injectTo` 真的被读出来了。
    ///
    /// 为什么值得单独测：这个字段是**注入语法**能不能生效的唯一依据 ——
    /// 读不到的话，Vue 里的 `v-if` / `@click` 会一直保持纯白，
    /// 而且**不会有任何报错**（和「缺 include」是同一种失效模式：
    /// 不是坏掉，而是变差，所以必须主动验证）
    // ---------- 代码片段 ----------

    /// ★ `contributes.snippets[].language` 既可以是字符串，也可以是数组。
    ///
    /// 为什么值得单独测：字段写成 `String` 的话，声明成数组的那些扩展会被 serde
    /// **整个跳过** —— 而 serde 是「全有或全无」的，那一个 package.json 整个不要了，
    /// 它的语法 / 主题 / 片段跟着一起消失。
    /// 症状是「某个扩展怎么完全没反应」，而且**不报错**，极难查
    #[test]
    fn snippet_language_accepts_both_string_and_array() {
        #[derive(Deserialize)]
        struct Holder {
            language: Option<LanguageList>,
        }

        let one: Holder = serde_json::from_str(r#"{"language": "typescript"}"#).unwrap();
        assert_eq!(one.language.unwrap().into_vec(), vec!["typescript"]);

        let many: Holder =
            serde_json::from_str(r#"{"language": ["javascript", "typescript"]}"#).unwrap();
        assert_eq!(
            many.language.unwrap().into_vec(),
            vec!["javascript", "typescript"]
        );

        // 没写 language 的是「全局片段」，我们暂时不支持 —— 但不能因此报错
        let none: Holder = serde_json::from_str("{}").unwrap();
        assert!(none.language.is_none());
    }

    /// 真机上扫一遍，把结果打出来看看（`--nocapture`）
    #[test]
    fn scans_snippet_extensions() {
        let entries = scan_snippet_extensions().expect("扫描应该成功");

        let languages: HashSet<&str> = entries
            .iter()
            .flat_map(|entry| entry.languages.iter().map(String::as_str))
            .collect();

        println!("[测试] 片段文件 {} 个，覆盖 {} 个语言", entries.len(), languages.len());
        for language in ["typescript", "javascript", "vue", "rust"] {
            let hits = entries
                .iter()
                .filter(|entry| entry.languages.iter().any(|id| id == language))
                .count();
            println!("[测试]   {language}: {hits} 份");
        }

        // ⚠ 不断言数量：换台机器可能一个扩展片段都没有，
        //   那种情况下这个测试也不该失败
        for entry in &entries {
            assert!(!entry.path.is_empty(), "片段条目必须带路径");
            assert!(!entry.languages.is_empty(), "进来之前就该滤掉没有语言的");
        }
    }

    #[test]
    fn reads_inject_to_from_manifests() {
        let entries = scan_grammar_extensions().expect("扫描应该成功");

        let injections: Vec<_> = entries
            .iter()
            .filter(|entry| !entry.inject_to.is_empty())
            .collect();

        // 跑 `cargo test -- --nocapture` 能看到这些
        println!("[测试] 注入语法 {} 个：", injections.len());
        for entry in &injections {
            println!("[测试]   {} → {:?}", entry.scope_name, entry.inject_to);
        }

        // ★ 顺便盯一下「降级探针」是不是都在。
        //   这四个 scopeName 只存在于 VS Code 程序目录的内置扩展里，
        //   是前端判定「基础语法库到手没有」的依据。少任何一个都可能让
        //   .vue 这类语法整批退回手写表 ——
        //   而且编译、分词全都不报错，症状仅仅是「状态栏的语言名从 vue 变成 html」
        println!("[测试] 降级探针：");
        for probe in ["text.html.basic", "source.css", "source.ts", "source.json"] {
            let found = entries.iter().any(|entry| entry.scope_name == probe);
            println!("[测试]   {probe}：{}", if found { "在" } else { "**不在**" });
        }

        // ⚠ 不断言数量：本机装了 Volar 才有，换台机器可能是 0 个，
        //   那种情况下这个测试也不该失败
        for entry in &injections {
            // 目前见过的注入语法都是「没写 language」的。
            // 万一出现既有 language 又有 injectTo 的，提出来看一眼 ——
            // 那说明它既当主分词器又要注入，得确认前端两条路都没漏
            assert!(
                entry.id.is_none(),
                "{} 既有 language 又有 injectTo，需要重新确认处理方式",
                entry.scope_name
            );
        }
    }

    // ---------- 密钥存储 ----------

    /// ★ 验证密钥文件确实是**加密**的，而且能原样读回来。
    ///
    /// 为什么值得单独测：这个功能有两种坏法，而**第二种完全看不出来** ——
    ///   ① 读不出密钥（会立刻暴露）
    ///   ② 以为加密了、其实还是明文（看起来一切正常）
    /// ★ 验证聊天记录的往返，以及**连着写两次**。
    ///
    /// 为什么值得测：第 ③ 条盯的是 Windows 上「rename 不能覆盖已存在文件」
    /// 那个坑 —— 它只在**第二次**保存时才会露出来，而「第一次能用」很有迷惑性。
    /// 第 ① 条盯的是「第一次用不该报错」
    #[test]
    fn chats_roundtrip_and_overwrite() {
        let dir = std::env::temp_dir().join("toocode-chats-test");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("toocode.chats.json");

        // ① 文件还不存在 ⇒ 空列表，而不是报错
        assert_eq!(read_chats(&path).unwrap(), "[]");

        // ② 写一次再读回来
        write_chats(&path, r#"[{"id":"a"}]"#).unwrap();
        assert_eq!(read_chats(&path).unwrap(), r#"[{"id":"a"}]"#);

        // ③ ★ 连着写第二次（覆盖）
        write_chats(&path, r#"[{"id":"b"}]"#).unwrap();
        assert_eq!(read_chats(&path).unwrap(), r#"[{"id":"b"}]"#);

        // ④ 临时文件不该留在磁盘上
        assert!(!path.with_extension("json.tmp").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(windows)]
    #[test]
    fn secret_file_is_encrypted_and_roundtrips() {
        let dir = std::env::temp_dir().join("toocode-secret-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.secret");

        let key = "sk-test-1234567890abcdef";
        write_secret(&path, key).unwrap();

        // ① 文件里**不能**出现明文密钥
        let raw = std::fs::read(&path).unwrap();
        assert!(
            raw.starts_with(SECRET_MAGIC),
            "加密文件应该带 TOOCODE1 标记"
        );
        assert!(
            !raw.windows(key.len()).any(|window| window == key.as_bytes()),
            "文件里不该出现明文密钥"
        );

        // ② 但要能原样读回来
        assert_eq!(read_secret(&path).unwrap(), key);

        // ③ 旧格式（整个文件就是明文）也要能读 ——
        //    这是「老用户不用重填密钥」的前提
        std::fs::write(&path, "sk-legacy-plain").unwrap();
        assert_eq!(read_secret(&path).unwrap(), "sk-legacy-plain");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn finds_vscode_builtin_extensions() {
        let roots = extension_roots();
        println!("\n找到的扩展根（{} 个）：", roots.len());
        for root in &roots {
            println!("  {}", root.display());
        }

        // 探针：这几个目录只随 VS Code 程序目录一起发布
        let mut hit = 0;
        for probe in ["html", "css", "typescript-basics", "json"] {
            let found = roots
                .iter()
                .any(|root| root.join(probe).join("package.json").is_file());
            if found {
                hit += 1;
            }
            println!("内置扩展 {probe}: {}", if found { "有" } else { "★没有" });
        }

        // 一个都没找到也不算失败 —— 用户可能没装 VS Code，或装在很奇怪的地方。
        // 但那就意味着「父语法拿不到」，前端会降级 ── 所以这里至少要说一声
        if hit == 0 {
            println!("★ 一个内置扩展都没找到：父语法拿不到，好多种语言会降级");
        }
    }

    /// ★ 同一份片段被两个「VS Code 版本目录」各扫一遍时，只该登记一次。
    ///
    /// 为什么值得测：这个 bug 的**表现是「重复」而不是「报错」** ——
    ///   补全列表里每条片段出现两次，看起来就像「本来就长这样」。
    ///   而它只在「新旧两个版本目录同时存在」时出现（VS Code 刚更新完那一阵），
    ///   等旧目录被清掉就自己消失了 —— 所以**不主动测就永远碰不到**
    #[test]
    fn dedups_snippets_across_duplicate_install_roots() {
        let base = std::env::temp_dir().join("toocode-snippet-dedup");
        std::fs::remove_dir_all(&base).ok();

        // 两个「安装目录」，结构照抄 VS Code：
        //   <安装根>\<版本哈希>\resources\app\extensions\<扩展名>\
        let mut roots = Vec::new();
        for install in ["install-a", "install-b"] {
            let root = base
                .join(install)
                .join("resources")
                .join("app")
                .join("extensions");
            for ext in ["markdown-basics", "other-ext"] {
                let dir = root.join(ext);
                std::fs::create_dir_all(dir.join("snippets")).unwrap();
                std::fs::write(
                    dir.join("package.json"),
                    r#"{ "contributes": { "snippets": [
                        { "language": "markdown", "path": "./snippets/markdown.json" } ] } }"#,
                )
                .unwrap();
                std::fs::write(dir.join("snippets").join("markdown.json"), "{}").unwrap();
            }
            roots.push(root);
        }

        let entries = scan_snippets_in(&roots);
        println!("\n扫到 {} 条片段：", entries.len());
        for entry in &entries {
            println!("  {} -> {:?}", entry.source, entry.languages);
        }

        // markdown-basics 在两个版本目录里都有，只能剩一条；
        // 而 other-ext 是**另一个扩展**（虽然相对路径一模一样）—— 那是两回事，不该被合并
        assert_eq!(entries.len(), 2, "同一扩展只该留一条，不同扩展不该被合并");
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.source == "markdown-basics")
                .count(),
            1
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.source == "other-ext")
                .count(),
            1
        );

        std::fs::remove_dir_all(&base).ok();
    }
}

// ============================ 代码片段（snippets）============================

/// `contributes.snippets[].language` —— 它**既可以是字符串，也可以是字符串数组**。
///
/// ⚠ 这是个真坑：字段写成 `String` 的话，那些声明成数组的扩展会被 serde 直接跳过。
///   而 serde 是「全有或全无」的 —— 一个字段类型不对，
///   那个扩展的 package.json **整个**都不要了，它的语法 / 主题也跟着一起没。
///   症状会是「某个扩展怎么完全没反应」，而且不报错
#[derive(Deserialize)]
#[serde(untagged)]
enum LanguageList {
    One(String),
    Many(Vec<String>),
}

impl LanguageList {
    fn into_vec(self) -> Vec<String> {
        match self {
            LanguageList::One(id) => vec![id],
            LanguageList::Many(ids) => ids,
        }
    }
}

#[derive(Deserialize)]
struct SnippetContribution {
    /// 不写 language 的片段文件是给「所有语言」用的，我们暂时跳过它们
    #[serde(default)]
    language: Option<LanguageList>,
    #[serde(default)]
    path: String,
}

/// 一份可用的片段文件
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SnippetEntry {
    /// 这份片段适用于哪些 Monaco 语言 id（一个文件可以同时给好几种语言用）
    languages: Vec<String>,
    /// 片段文件的绝对路径（前端再自己去读内容）
    path: String,
    /// 这份片段来自哪个扩展。
    /// ★ 不是装饰：片段重名时用户会问「这条是哪来的」，而且排查也只靠它
    source: String,
}

/// 扫一遍所有扩展，把它们贡献的代码片段（`contributes.snippets`）列出来。
///
/// ★ 只负责「哪个语言的片段在哪个文件」，**不解析片段内容** ——
///   内容是前端读的（和语法 / 主题一致）。
///   一个扩展往往在 snippets/ 下摆十几个语言的文件，
///   启动时全读进来解析一遍纯属浪费，用到哪个读哪个
#[tauri::command]
fn scan_snippet_extensions() -> Result<Vec<SnippetEntry>, String> {
    Ok(scan_snippets_in(&extension_roots()))
}

/// 真正干活的那一层。
///
/// ★ 为什么把 `roots` 做成参数、而不是在这里自己调 `extension_roots()`：
///   「两个版本目录里的同一份片段只登记一次」这条规则**没法靠眼睛验** ——
///   它只在「新旧两个版本目录同时存在」时才会起作用，而那种状态
///   VS Code 更新完过一阵子就自己没了。参数化之后测试能自己搭两个假安装
///   目录把它钉住（见 `dedups_snippets_across_duplicate_install_roots`）
fn scan_snippets_in(roots: &[PathBuf]) -> Vec<SnippetEntry> {
    let mut entries: Vec<SnippetEntry> = Vec::new();

    // ★ 先到先得（和语法那套一致）：扫描顺序 = 目录优先级（用户扩展 > 内置），
    //   所以用户装的扩展能盖住内置的
    let mut seen = HashSet::new();

    for_each_manifest(roots, |dir, manifest| {
        let Some(contributes) = manifest.contributes else {
            return;
        };

        for snippet in contributes.snippets {
            // 老规矩：**先确认文件真的存在，再占位**。
            // 反过来的话，一条「声明了路径但文件不在」的片段会把 key 白白吃掉
            let path = dir.join(&snippet.path);
            if !path.is_file() {
                continue;
            }

            let Some(languages) = snippet.language else {
                // 没写 language 的片段文件通常是「全局片段」，我们还不支持
                continue;
            };

            let extension_name = dir
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();

            // ⚠ 判重的 key 用**元组**，不要拿一个字符拼字符串 ——
            //   拼的话就得挑一个「路径里绝不会出现」的分隔符，
            //   而那种魔术字符正是以后会被忘掉、然后出 bug 的东西
            //
            // ★★ key 是「语言 + **扩展目录名** + **扩展内的相对路径**」，
            //   而不是绝对路径。这一条是被现实逼出来的：
            //   · VS Code 把自己的扩展放在 `<安装根>\<版本哈希>\resources\app\extensions`
            //   · 而**更新之后新旧两个版本目录会同时存在**（旧的要留一阵子好回滚）
            //   ⇒ 按绝对路径判重的话，同一份 `snippets/xxx.json` 会被两个版本
            //     各登记一次。实测：本机 VS Code 刚更新过，片段文件数从 17 变成 **34**，
            //     界面上就是「补全列表里每条片段出现两次」
            //   ⇒ 而「这份片段是谁贡献的」本来就和它在哪个盘上无关，
            //     真正的身份是「哪个扩展 + 扩展里的哪个文件」
            //   ⚠ 用扩展目录名而不是绝对路径，同时也意味着：**不同**的扩展
            //     哪怕相对路径一模一样，也还是两条（那是两回事）
            let normalized = path.to_string_lossy().replace('\\', "/");
            let relative = snippet.path.replace('\\', "/");
            let relative = relative.trim_start_matches("./").to_string();

            let wanted: Vec<String> = languages
                .into_vec()
                .into_iter()
                .filter(|id| {
                    seen.insert((id.clone(), extension_name.clone(), relative.clone()))
                })
                .collect();

            if wanted.is_empty() {
                continue;
            }

            entries.push(SnippetEntry {
                languages: wanted,
                path: normalized,
                source: extension_name,
            });
        }
    });

    println!(
        "[代码片段] 得到 {} 个片段文件，覆盖 {} 个语言",
        entries.len(),
        entries
            .iter()
            .flat_map(|entry| entry.languages.iter())
            .collect::<HashSet<_>>()
            .len()
    );

    entries
}

// ============================ AI 对话 ============================
//
// ★★ 分工：Rust 只干两件事 ——「发一次请求」和「把原始 SSE 文本转发回去」。
//   解析（delta 拼接 / 工具调用累加）留在前端，理由：
//     1. 那段逻辑已经用纯函数测过了（`src/agentStream.ts`）
//     2. 各家 API 的字段差异（`reasoning` vs `reasoning_content`）
//        在 JS 里加个 `??` 就行，改 Rust 结构体要重新编译
//
// ★★ 为什么请求不能由前端直接发：云端 API 基本都不允许浏览器跨域（CORS），
//   WebView 里 fetch 会被直接拦掉
//
// ★★ 为什么密钥存后端、而且**不回传**给前端：
//   前端的一切都能被 F12 看到，localStorage 也是明文。
//   前端只需要知道「配没配」，用的时候由 Rust 自己去读

/// 一次对话请求的参数。
/// 用 OpenAI 兼容的格式 —— 这样 OpenAI / DeepSeek / 通义 / 本地的 Ollama
/// 和 LM Studio 都是同一套代码
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChatRequest {
    /// 接口根地址，如 `https://api.deepseek.com/v1`
    base_url: String,
    model: String,
    /// 直接透传的 messages 数组（含 role / content / tool_calls / tool_call_id）
    messages: Vec<serde_json::Value>,
    /// 工具声明。为 None 时就是纯聊天
    #[serde(default)]
    tools: Option<Vec<serde_json::Value>>,
    /// 采样温度
    #[serde(default)]
    temperature: Option<f32>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ChatDeltaEvent {
    /// 和发起时传的一致 —— 前端可能同时开着多个会话
    request_id: String,
    /// 原始 SSE 文本，前端自己按行解析
    text: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ChatEndEvent {
    request_id: String,
    /// 出错时带上原因；正常结束是 None
    error: Option<String>,
}

/// 密钥文件放哪儿：`appDataDir/toocode.secret`
fn secret_file(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    use tauri::Manager;
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|err| format!("拿不到应用数据目录：{err}"))?;

    // 首次运行时目录还不存在 —— create_dir_all 幂等，不用先判断
    std::fs::create_dir_all(&dir).map_err(|err| format!("建目录失败：{err}"))?;

    Ok(dir.join("toocode.secret"))
}

// ---------- 密钥的加密存储 ----------
//
// ★★ 先说清楚这个加密**防的是什么**（威胁模型）：
//   · 「前端 / 篡改的页面把密钥偷走」—— 这个**已经**防住了，和加不加密无关：
//     密钥不进前端、没有「读密钥」的命令、请求也从 Rust 发
//   · 「磁盘上的文件被别的程序读走」—— 这个是**明文文件**的问题，加密要解决的就是它
//
// ★ Windows 上用 DPAPI（CryptProtectData）：
//   加密出来的数据**只有同一个用户在同一台机器上**能解开，密钥材料由系统保管。
//   ★ 为什么不自己写一套加密：那必然要面对「主密码存哪」——
//     用一个写死的密钥去加密等于没加密，而让用户每次输密码体验又太差。
//     DPAPI 正好绕开了这个死结
//
// ★ 非 Windows 平台退回明文，并且**明说**这件事，不要让人以为已经加密了

/// 加密文件的头部。
/// 没有这个头就说明是**旧的明文格式** —— 于是老文件照样能读，
/// 下次保存时自动写成加密格式（等于无缝迁移）
const SECRET_MAGIC: &[u8] = b"TOOCODE1";

#[cfg(windows)]
mod dpapi {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
    };

    fn as_blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        }
    }

    /// 把系统分配的内存交还回去。
    /// ⚠ DPAPI 的输出是用 `LocalAlloc` 分配的，必须用 `LocalFree` 释放 ——
    ///   拿 `Vec` 接走之后忘了这一步就是内存泄漏
    unsafe fn take_output(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as *mut core::ffi::c_void);
        bytes
    }

    pub fn protect(plain: &[u8]) -> Result<Vec<u8>, String> {
        let mut input = as_blob(plain);
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };

        let ok = unsafe {
            CryptProtectData(
                &mut input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                &mut output,
            )
        };

        if ok == 0 {
            return Err("密钥加密失败".into());
        }
        Ok(unsafe { take_output(output) })
    }

    pub fn unprotect(blob: &[u8]) -> Result<Vec<u8>, String> {
        let mut input = as_blob(blob);
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };

        let ok = unsafe {
            CryptUnprotectData(
                &mut input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                &mut output,
            )
        };

        if ok == 0 {
            // 常见原因：换了台机器、或者换了 Windows 用户 —— 那种情况下
            // 密钥确实解不开了（这是 DPAPI 的设计意图，不是故障）
            return Err("密钥解密失败（可能是换了电脑或系统用户）".into());
        }
        Ok(unsafe { take_output(output) })
    }
}

/// 读取密钥。自动兼容明文（旧格式）和加密（新格式）
fn read_secret(path: &Path) -> Result<String, String> {
    let raw = std::fs::read(path).map_err(|_| "还没配置 API 密钥".to_string())?;

    let plain = if raw.starts_with(SECRET_MAGIC) {
        let body = &raw[SECRET_MAGIC.len()..];

        #[cfg(windows)]
        {
            dpapi::unprotect(body)?
        }
        #[cfg(not(windows))]
        {
            // 非 Windows 上 magic 后面直接就是明文（见 write_secret）
            body.to_vec()
        }
    } else {
        // 旧格式：整个文件就是明文密钥
        raw
    };

    String::from_utf8(plain).map_err(|_| "密钥文件不是合法的 UTF-8".to_string())
}

/// 写入密钥。Windows 上会先加密
fn write_secret(path: &Path, value: &str) -> Result<(), String> {
    #[cfg(windows)]
    let payload = {
        let encrypted = dpapi::protect(value.as_bytes())?;
        let mut buf = SECRET_MAGIC.to_vec();
        buf.extend_from_slice(&encrypted);
        buf
    };

    #[cfg(not(windows))]
    let payload = {
        // ⚠ 非 Windows 上没有等价的「系统帮你管密钥」的机制，
        //   所以这里就是明文。不要假装它被加密了
        let mut buf = SECRET_MAGIC.to_vec();
        buf.extend_from_slice(value.as_bytes());
        buf
    };

    std::fs::write(path, payload).map_err(|err| format!("写入失败：{err}"))
}

/// 存 API 密钥。值**只进不出** —— 没有对应的「读出来」命令
#[tauri::command]
fn save_secret(app: tauri::AppHandle, value: String) -> Result<(), String> {
    let path = secret_file(&app)?;
    write_secret(&path, value.trim())
}

/// 前端只问这个 —— 用来决定「显示配置界面」还是「可以开聊了」
#[tauri::command]
fn has_secret(app: tauri::AppHandle) -> Result<bool, String> {
    let path = secret_file(&app)?;
    // ⚠ 这里走完整的解密流程，而不是「文件存在就算配过」——
    //   在别的机器上拷过来的文件解不开，那种情况下应该让用户重新配，
    //   而不是等他发消息时才报错
    Ok(read_secret(&path).map(|key| !key.trim().is_empty()).unwrap_or(false))
}

#[tauri::command]
fn clear_secret(app: tauri::AppHandle) -> Result<(), String> {
    let path = secret_file(&app)?;
    if path.exists() {
        std::fs::remove_file(&path).map_err(|err| format!("删除失败：{err}"))?;
    }
    Ok(())
}

/// 复用同一个 HTTP 客户端。
/// ★ 每次新建 Client 会连连接池一起丢掉，逐轮对话时开销很明显
static HTTP_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();

fn http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(|| {
        let mut builder = reqwest::Client::builder()
            // ★★ 默认**不读系统代理**，只认环境变量。
            //
            // 这一段是拿真接口一步步试出来的，值得记下来：
            //   · `reqwest` 0.12 的默认 feature 里带了 `system-proxy` ——
            //     它会去读 **Windows「Internet 选项」里那个代理**
            //     （`HKCU\...\Internet Settings` 的 `ProxyEnable` / `ProxyServer`）。
            //     本机就是开的：`ProxyEnable=1`、`ProxyServer=127.0.0.1:7897`（Clash 那种）
            //   · 走那条路时请求**必然**失败：
            //     `error sending request … ← client error (Connect) ← unexpected EOF during handshake`
            //     （CONNECT 建上了，然后对端把连接关掉，TLS 握手当场结束）
            //   · 而**同一个 URL、同样的 body**：
            //     · `curl`（会读 IE 代理）→ 401，0.12s
            //     · `curl --noproxy "*"`（直连）→ 401，1.1s
            //     · PowerShell → 401
            //     · 只有我们过不去 —— 换成 `.no_proxy()` 之后**立刻就好了**
            //       （回答正常流式返回、`read_file` 工具也调起来了）
            //   ⇒ 结论：**那个系统代理不可靠**（它的规则完全可能把某个域名路由到死节点上），
            //     而 Windows 的「IE 代理」本来就是**给浏览器**用的约定 ——
            //     Node / Python requests 这些默认都不读它，跟着它走反而会踩别人的坑
            //
            // ★ 但**不能**因此把代理整个砍掉：有人直连就是不通（比如用 OpenAI 的接口），
            //   对他们是必须的。所以保留**环境变量**这条路 —— 这也是绝大多数 CLI 工具的规矩
            //   （`HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY`，大小写都认）
            //   ⚠ `no_proxy()` 只是**关掉自动发现**，下面显式加的代理照样生效
            //     （所以顺序要紧：先 no_proxy 再 proxy）
            .no_proxy()
            // 模型「思考 + 写代码」可能要很久，超时给宽一点；
            // 但**不能不给** —— 没有超时的话网络一断这个命令就永远挂着
            .timeout(std::time::Duration::from_secs(300));

        let from_env = [
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "ALL_PROXY",
            "all_proxy",
        ]
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .filter(|value| !value.trim().is_empty());

        if let Some(value) = from_env {
            match reqwest::Proxy::all(value.trim()) {
                Ok(proxy) => builder = builder.proxy(proxy),
                // 填错了不该让整个应用起不来 —— 说一声，然后按「不用代理」继续
                Err(error) => eprintln!("[网络] 环境变量里的代理填得不对，已忽略：{error}"),
            }
        }

        builder.build().expect("构造 HTTP 客户端不该失败")
    })
}

/// 把 reqwest 的错误**连同底层原因**一起说清楚。
///
/// ★★ 为什么必须自己走一遍 source 链：`reqwest::Error` 的 Display 只有最外面那一句
///   —— `error sending request for url (…)`，**完全指不到原因**。
///   真正有用的（连接被拒 / DNS 解不开 / TLS 握手失败 / 超时）都在 `source()` 里。
///   实测：先看到的就是那句废话，只能靠猜；把链走完才知道是「连接被重置」还是别的
fn describe_http_error(context: &str, error: &reqwest::Error) -> String {
    let mut parts = vec![error.to_string()];
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(error);
    while let Some(inner) = source {
        let text = inner.to_string();
        // 有几层只是把上面那句话重复一遍（hyper 那层常常如此），去掉重复的
        if !parts.iter().any(|part| part == &text) {
            parts.push(text);
        }
        source = inner.source();
    }
    format!("{context}：{}", parts.join(" ← "))
}

/// 发起一次对话，把响应**原始文本**逐块 emit 回前端。
///
/// 事件：
///   `chat-delta` —— 每收到一块就发一次（可能是半行，前端要自己缓冲）
///   `chat-end`   —— 这一轮结束（正常或出错都发，前端靠它收尾）
#[tauri::command]
async fn chat_stream(
    app: tauri::AppHandle,
    request_id: String,
    request: ChatRequest,
) -> Result<(), String> {
    use futures_util::StreamExt;
    use tauri::Emitter;

    let key = {
        let path = secret_file(&app)?;
        // 走 read_secret 而不是直接读文件 —— 密钥在磁盘上是**加密**的
        read_secret(&path)?.trim().to_string()
    };
    if key.is_empty() {
        return Err("还没配置 API 密钥".into());
    }

    let url = format!("{}/chat/completions", request.base_url.trim_end_matches('/'));

    let mut body = serde_json::json!({
        "model": request.model,
        "messages": request.messages,
        // ★ 不写 stream 的话服务端会把整个回复一次返回，
        //   那「逐字输出」的效果就没了
        "stream": true,
    });
    if let Some(tools) = request.tools {
        if !tools.is_empty() {
            body["tools"] = serde_json::Value::Array(tools);
        }
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = serde_json::json!(temperature);
    }

    let send = http_client()
        .post(&url)
        .bearer_auth(&key)
        .json(&body)
        .send()
        .await;

    let response = match send {
        Ok(response) => response,
        Err(err) => {
            let _ = app.emit(
                "chat-end",
                ChatEndEvent {
                    request_id,
                    error: Some(describe_http_error("请求失败", &err)),
                },
            );
            return Ok(());
        }
    };

    // ★ 非 2xx 也要把**响应体**读出来 —— 服务端会在这里说明原因
    //   （密钥错了、模型名不对、余额不足…），只报状态码的话用户无从下手
    if !response.status().is_success() {
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        let _ = app.emit(
            "chat-end",
            ChatEndEvent {
                request_id,
                error: Some(format!("服务端返回 {status}：{}", detail.trim())),
            },
        );
        return Ok(());
    }

    // ★★ UTF-8 边界：`bytes_stream()` 给的一块**不保证**落在字符边界上，
    //   直接 `from_utf8_lossy` 会在中文被切开时偶发一个 `�`。
    //   做法是攒着，只在「能完整解码」的部分才发出去，剩下的字节留到下一块
    let mut pending: Vec<u8> = Vec::new();
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let bytes = match chunk {
            Ok(bytes) => bytes,
            Err(err) => {
                let _ = app.emit(
                    "chat-end",
                    ChatEndEvent {
                        request_id,
                        error: Some(describe_http_error("读取响应中断", &err)),
                    },
                );
                return Ok(());
            }
        };

        pending.extend_from_slice(&bytes);

        // 从尾部往回找「合法的截断点」：一个不完整的多字节序列最多 3 个字节
        let valid_upto = match std::str::from_utf8(&pending) {
            Ok(_) => pending.len(),
            Err(err) => err.valid_up_to(),
        };

        if valid_upto > 0 {
            let text = String::from_utf8_lossy(&pending[..valid_upto]).to_string();
            pending.drain(..valid_upto);
            let _ = app.emit(
                "chat-delta",
                ChatDeltaEvent {
                    request_id: request_id.clone(),
                    text,
                },
            );
        }
    }

    // 收尾：还有残余字节就说明流被切断了，但也要尽量交出去
    if !pending.is_empty() {
        let _ = app.emit(
            "chat-delta",
            ChatDeltaEvent {
                request_id: request_id.clone(),
                text: String::from_utf8_lossy(&pending).to_string(),
            },
        );
    }

    let _ = app.emit(
        "chat-end",
        ChatEndEvent {
            request_id,
            error: None,
        },
    );

    Ok(())
}

// ---------- 聊天记录（Topilot 的「会话」）----------

/// 聊天记录放哪儿：`appDataDir/toocode.chats.json`
fn chats_file(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    use tauri::Manager;
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|err| format!("拿不到应用数据目录：{err}"))?;
    std::fs::create_dir_all(&dir).map_err(|err| format!("建目录失败：{err}"))?;
    Ok(dir.join("toocode.chats.json"))
}

/// 读聊天记录。**返回原始 JSON 文本**，解析留给前端。
///
/// ★★ 为什么不放 localStorage：它有容量上限，而且和 hot exit **共用**同一个配额 ——
///   聊天记录一多，未保存内容的备份就会**静默**写失败。
///   丢聊天记录只是烦，丢未保存的代码是真事故
///
/// ★ 和 `chat_stream` 一样的分工理由：会话的结构是前端的事，
///   以后加个字段不用重编 Rust（把结构体写在这儿就得跟着改）
fn read_chats(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        // ★ 文件还不存在是**正常**的（第一次用），不是错误 ——
        //   算报错的话调用方还得先判断「是不是第一次」，很啰嗦
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok("[]".to_string()),
        Err(error) => Err(format!("读聊天记录失败：{error}")),
    }
}

/// 写聊天记录。
///
/// ★ 先写临时文件再改名，而不是直接覆盖：
///   直接覆盖的话，写到一半被打断（崩溃 / 强杀）会留下一个**半截的 JSON**，
///   下次启动就整个读不出来了。改名是「要么全有要么全无」的 ——
///   最坏情况也只是回到上一个版本
fn write_chats(path: &Path, data: &str) -> Result<(), String> {
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, data).map_err(|err| format!("写聊天记录失败：{err}"))?;

    // ⚠ Windows 上 `rename` **不能覆盖已存在的文件**（会报 AlreadyExists），
    //   所以得先把目标删掉。中间那一瞬间文件是不存在的 —— 可接受：
    //   真在那时崩了，最多丢掉这一次改动，不会留下一个坏文件
    if path.exists() {
        std::fs::remove_file(path).map_err(|err| format!("替换聊天记录失败：{err}"))?;
    }
    std::fs::rename(&temp, path).map_err(|err| format!("保存聊天记录失败：{err}"))?;
    Ok(())
}

#[tauri::command]
fn load_chats(app: tauri::AppHandle) -> Result<String, String> {
    read_chats(&chats_file(&app)?)
}

#[tauri::command]
fn save_chats(app: tauri::AppHandle, data: String) -> Result<(), String> {
    write_chats(&chats_file(&app)?, &data)
}

// ============================ 全文搜索 ============================
//
// ★ 为什么不复用 `walk()`：那个函数是给文件树用的 —— 它返回的是「结构」，
//   受 MAX_DEPTH 限制，而且不读文件内容。搜索要的是「把所有能读的文件过一遍」，
//   是另一件事，硬套只会让两边都别扭。
//   ⚠ 但**共用了同一份 IGNORED_DIRS** —— 这条规则必须一致，
//     不然会出现「文件树里看不到 node_modules、但搜索能搜到」这种怪事。

/// 一条匹配
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchMatch {
    path: String,
    /// 1 起的行号（Monaco 的 setPosition 也是 1 起）
    line: usize,
    /// 1 起的列号，**按字符数**算（Monaco 的 column 也是字符口径）
    column: usize,
    /// 整行的内容（去掉首尾空白，省得结果列表里一堆缩进）
    text: String,
    /// 匹配在 `text` 里的起止字符下标，给前端做高亮
    start: usize,
    end: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchResponse {
    matches: Vec<SearchMatch>,
    /// 是不是撞到上限提前停了 —— 前端要告诉用户「结果被截断了」
    truncated: bool,
    /// 实际扫了多少个文件 —— 给用户一个「它确实干活了」的反馈
    files_scanned: usize,
}

/// 一次搜索最多返回多少条。不封顶的话大项目能把前端灌死
const MAX_SEARCH_RESULTS: usize = 1000;
/// 单个文件超过这个大小就跳过。
/// ⚠ 比 `MAX_FILE_BYTES`（5MB，给「打开一个文件」用的）更保守 ——
///   搜索是**批量**读文件，同一个上限会让一次搜索读进来几百 MB
const MAX_SEARCH_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// 单行最多取多少字符。压缩过的 js 可能一行几十万字符
const MAX_LINE_CHARS: usize = 400;
/// 防软链接成环的兜底深度
const MAX_SEARCH_DEPTH: usize = 32;

/// 搜索要扫的一个文件。
///
/// ★ 为什么要把 mtime/size 一起带出来：`searchdb` 要靠它判断「这个文件变过没有」，
///   而「每次搜索都做一遍 stat 扫描」正是索引的**固定成本**。
///   在外面再 `metadata()` 一次，等于每个文件多一次系统调用 ——
///   一次搜索多几千次，就不是零头了
struct SearchFile {
    path: PathBuf,
    /// 毫秒时间戳（拿不到就是 0，那就每次重建）
    mtime_ms: i64,
    size: u64,
}

/// 把所有值得搜的文件收集起来。和 `walk()` 共用 IGNORED_DIRS，但不设业务深度上限。
///
/// ★ 两条路都从这里拿文件：索引的 stat 差集、以及索引还没覆盖到的那部分文件的
///   现场扫描 —— 共用同一个遍历，忽略规则 / 深度上限 / 大小上限就只需要维护一份
fn collect_search_files(dir: &Path, out: &mut Vec<SearchFile>, depth: usize) {
    if depth > MAX_SEARCH_DEPTH {
        return;
    }

    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let path = entry.path();

        if meta.is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if IGNORED_DIRS.contains(&name.as_str()) {
                continue;
            }
            collect_search_files(&path, out, depth + 1);
        } else if meta.is_file() && meta.len() <= MAX_SEARCH_FILE_BYTES {
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            out.push(SearchFile {
                path,
                mtime_ms,
                size: meta.len(),
            });
        }
    }
}


/// 在一个文件的内容里找出所有匹配。
///
/// 返回 `(行号, 列号, 行内容, 匹配起点, 匹配终点)` —— 后两个是**相对行内容**的下标。
///
/// ★ 下标全部按 **UTF-16 单位**算（`encode_utf16().count()`），**不是**字符数。
///   为什么不直接用 `chars().count()`（直觉上更「对」）：
///   - Monaco 的 `column` 是 UTF-16 口径
///   - 前端高亮要用 `String.prototype.slice()`，JS 的字符串下标也是 UTF-16 口径
///   两边都按这个来，前端拿到的下标才能**直接**用。
///   ⚠ 对 BMP 字符（中文、ASCII）字符数 = UTF-16 长度，看不出差别；
///     但 emoji（补充平面，如 😀）是「1 个 char、2 个 UTF-16 单位」，
///     用字符数当列号会让光标插到 emoji 中间，高亮也会短一截
fn search_in_text(
    content: &str,
    query: &str,
    case_sensitive: bool,
) -> Vec<(usize, usize, String, usize, usize)> {
    let mut hits = Vec::new();
    if query.is_empty() {
        return hits;
    }

    let needle = if case_sensitive {
        query.to_string()
    } else {
        query.to_lowercase()
    };
    let needle_units = needle.encode_utf16().count();

    for (line_index, raw_line) in content.lines().enumerate() {
        // 超长行先截断再比对。
        // ⚠ 这一步按 char 切（不是 UTF-16）—— 按 UTF-16 切可能把代理对劈成两半，
        //   得到一个非法字符串
        let line: String = raw_line.chars().take(MAX_LINE_CHARS).collect();
        let haystack = if case_sensitive {
            line.clone()
        } else {
            line.to_lowercase()
        };

        // 去掉首尾空白后的行，才是展示给用户看的
        let shown = line.trim();
        // 展示文本相对原始行「少了几个 UTF-16 单位」——
        // 高亮下标要减掉这个偏移，否则会整体往前偏
        let leading = line.encode_utf16().count() - line.trim_start().encode_utf16().count();
        let shown_units = shown.encode_utf16().count();

        let mut from = 0;
        while let Some(offset) = haystack[from..].find(&needle) {
            let byte_start = from + offset;
            let byte_end = byte_start + needle.len();

            // ★ 字节下标 → UTF-16 单位下标
            let unit_start = line[..byte_start].encode_utf16().count();
            let unit_end = unit_start + needle_units;

            hits.push((
                line_index + 1,
                unit_start + 1,
                shown.to_string(),
                unit_start.saturating_sub(leading).min(shown_units),
                unit_end.saturating_sub(leading).min(shown_units),
            ));

            from = byte_end;
            // 单文件上限：防某些病态文件（一行里出现几千次）把内存吃光
            if hits.len() > 500 {
                return hits;
            }
        }
    }

    hits
}

/// 在整个文件夹里搜关键词
///
/// ★ 有索引时走索引（`searchdb`），没有 / 建不出来 / 查询词太短时**完全退回**
///   原来的现场扫描 —— 两条路的结果一样，只是快慢不同。
///   这是整套设计里最重要的一条：索引是**加速手段**，不是正确性的前提
#[tauri::command]
async fn search_in_folder(
    app: tauri::AppHandle,
    store: State<'_, crate::searchdb::IndexStore>,
    path: String,
    query: String,
    case_sensitive: bool,
) -> Result<SearchResponse, String> {
    if query.trim().is_empty() {
        return Ok(SearchResponse {
            matches: Vec::new(),
            truncated: false,
            files_scanned: 0,
        });
    }

    // ★ 搜索是重活（要挨个读文件），必须挪到**阻塞线程池**里。
    //   直接在 async fn 主体里跑会把运行时线程占住，同时发起的其它调用只能排队
    let store = store.0.clone();
    tauri::async_runtime::spawn_blocking(move || {
        // ① 问索引：「哪些文件可能命中」+「哪些文件还没建好」
        //
        //    ⚠ 这里任何一步出问题都当成「没有索引」处理，绝不能让**搜索本身**失败 ——
        //      索引是锦上添花，它坏了不该连搜索都用不了
        let plan = if crate::searchdb::is_indexable(&query) {
            crate::searchdb::with_index(&app, &store, &path, |index| {
                index.sync(Path::new(&path), false)?;
                if !index.is_usable() {
                    return Ok(None);
                }
                Ok(Some((
                    index.candidate_paths(&query)?,
                    index.unindexed_paths()?,
                )))
            })
            .unwrap_or_else(|err| {
                println!("[搜索索引] 这次没用上索引，走旧路径：{err}");
                None
            })
        } else {
            // 查询词短于 3 个字符：trigram 提取不出三元组，索引帮不上忙
            None
        };

        // ② 要读哪些文件
        let targets: Vec<PathBuf> = match plan {
            Some((candidates, unindexed)) => {
                // ★ 两类文件**必须都读**：
                //   · candidates —— 索引说「里面可能有这个词」
                //   · unindexed —— 还没建进索引的（索引是渐进建的）
                //   少了后者就等于「索引没建完 → 搜不到东西」，而那种错是无声的
                let mut targets: Vec<PathBuf> =
                    candidates.into_iter().map(PathBuf::from).collect();
                targets.extend(unindexed.into_iter().map(PathBuf::from));
                targets
            }
            None => {
                let mut entries = Vec::new();
                collect_search_files(Path::new(&path), &mut entries, 0);
                entries.into_iter().map(|entry| entry.path).collect()
            }
        };

        let mut matches: Vec<SearchMatch> = Vec::new();
        let mut truncated = false;
        let mut files_scanned = 0usize;

        for file in &targets {
            if matches.len() >= MAX_SEARCH_RESULTS {
                truncated = true;
                break;
            }

            // 读不出来（二进制 / 没权限 / 不是 UTF-8）就当它不存在。
            // 搜索应该容错，不该因为一个文件就整个失败
            let Ok(bytes) = fs::read(file) else { continue };
            let Ok(content) = String::from_utf8(bytes) else {
                continue;
            };

            files_scanned += 1;
            let file_path = file.to_string_lossy().replace('\\', "/");

            for (line, column, text, start, end) in
                search_in_text(&content, &query, case_sensitive)
            {
                if matches.len() >= MAX_SEARCH_RESULTS {
                    truncated = true;
                    break;
                }
                matches.push(SearchMatch {
                    path: file_path.clone(),
                    line,
                    column,
                    text,
                    start,
                    end,
                });
            }
        }

        Ok(SearchResponse {
            matches,
            truncated,
            files_scanned,
        })
    })
    .await
    .map_err(|err| err.to_string())?
}


// ============================ 终端（PTY） ============================
//
// 数据流：
//   前端 xterm --onData--> pty_write(id, data)      用户敲的键
//                          pty_resize(id, cols, rows) 面板尺寸变了
//   Rust 读线程 --emit("pty-output")--> 前端 xterm.write(字节)
//
// ★ 三个关键决策：
//
// 1. **输出必须传字节，不能传 String**。PTY 吐的是裸字节流，一个多字节字符
//    （中文、emoji）可能正好横跨两次 read()。两边各自解 UTF-8 都会失败，
//    屏幕上就变成 `�`。所以 Rust 侧不解码，原样传过去，由 xterm 自己处理
//
// 2. **走 base64 而不是 Vec<u8>**。Tauri 的事件 payload 是 JSON，
//    `Vec<u8>` 会被序列化成 `[27,91,51,50,109,...]` 数字数组，体积膨胀约 3 倍，
//    高频输出（`dir /s`）会直接把 JSON 解析干掉。base64 只膨胀 4/3
//
// 3. **读输出必须开独立线程**。`read()` 是阻塞的，挂在命令里会把
//    Tauri 的命令线程整个卡死 —— 终端会“敲了没反应”

/// 一个终端会话的全部家当。三样缺一不可：
/// writer 用来写输入，master 用来 resize，child 用来 kill
struct PtySession {
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

/// 所有活着的会话。前端拿 id 来寻址
#[derive(Default)]
struct PtyState(Mutex<HashMap<u32, PtySession>>);

impl Drop for PtyState {
    fn drop(&mut self) {
        // 应用退出时把子进程全收掉 —— 不然 powershell 会留在后台变成孤儿进程
        if let Ok(mut map) = self.0.lock() {
            for (_, session) in map.iter_mut() {
                let _ = session.child.kill();
            }
        }
    }
}

/// 推给前端的一条输出。data 是 base64，不是明文
#[derive(Clone, Serialize)]
struct PtyOutput {
    id: u32,
    data: String,
}

/// 起一个 shell 会话。id 由前端生成（前端维护计数器，不依赖后端状态）
#[tauri::command]
fn pty_spawn(
    app: AppHandle,
    state: State<'_, PtyState>,
    id: u32,
    cols: u16,
    rows: u16,
    cwd: Option<String>,
) -> Result<(), String> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|err| err.to_string())?;

    let mut cmd = CommandBuilder::new("powershell.exe");
    // -NoLogo：不打印版权头，省得开盘就刷掉一屏
    cmd.arg("-NoLogo");
    // ⚠ 只在目录真的存在时才设 cwd —— 直接 cwd() 一个不存在的路径
    //   会让 spawn 整个失败，而用户只是换了个工作区而已
    if let Some(dir) = cwd {
        if !dir.is_empty() && Path::new(&dir).is_dir() {
            cmd.cwd(dir);
        }
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|err| err.to_string())?;

    // ★ 必须把 slave 端丢掉：它是“另一端”的句柄，留着的话 master 永远读不到 EOF，
    //   子进程退出后读线程会一直挂在那里
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().map_err(|err| err.to_string())?;
    let writer = pair.master.take_writer().map_err(|err| err.to_string())?;

    state.0.lock().map_err(|err| err.to_string())?.insert(
        id,
        PtySession {
            writer,
            master: pair.master,
            child,
        },
    );

    // 读线程：PTY 有输出就 base64 后 emit 给前端
    let app_handle = app.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                // Ok(0) = 子进程关了那一端，正常收工
                Ok(0) => break,
                Ok(count) => {
                    let data = base64::engine::general_purpose::STANDARD.encode(&buf[..count]);
                    if app_handle
                        .emit("pty-output", PtyOutput { id, data })
                        .is_err()
                    {
                        // emit 失败 = 窗口没了，没必要再读
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    Ok(())
}

/// 把用户敲的键写进 PTY
#[tauri::command]
fn pty_write(state: State<'_, PtyState>, id: u32, data: String) -> Result<(), String> {
    let mut map = state.0.lock().map_err(|err| err.to_string())?;
    let session = map.get_mut(&id).ok_or("终端会话不存在")?;
    session
        .writer
        .write_all(data.as_bytes())
        .map_err(|err| err.to_string())?;
    // ★ 必须 flush：不刷的话输入会缓在 writer 里，表现为“敲了不出字”
    session.writer.flush().map_err(|err| err.to_string())?;
    Ok(())
}

/// 面板尺寸变了，告诉 PTY。不告诉的话 shell 会按 80 列排版，
/// 换行位置全是错的（vim / top 这类全屏程序会花屏）
#[tauri::command]
fn pty_resize(state: State<'_, PtyState>, id: u32, cols: u16, rows: u16) -> Result<(), String> {
    let map = state.0.lock().map_err(|err| err.to_string())?;
    let session = map.get(&id).ok_or("终端会话不存在")?;
    session
        .master
        .resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|err| err.to_string())?;
    Ok(())
}

/// 关掉会话（面板里的 × 或标签切换时）
#[tauri::command]
fn pty_kill(state: State<'_, PtyState>, id: u32) -> Result<(), String> {
    let mut map = state.0.lock().map_err(|err| err.to_string())?;
    if let Some(mut session) = map.remove(&id) {
        let _ = session.child.kill();
    }
    Ok(())
}

// ============================ 启动 ============================

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        // dialog 插件：提供系统原生的文件/文件夹选择对话框
        .plugin(tauri_plugin_dialog::init())
        // 这里列出的命令才会被注册到前端可调用的名单里
        .invoke_handler(tauri::generate_handler![
            read_dir,
            read_file,
            read_preview_bytes,
            write_file,
            scan_grammar_extensions,
            scan_theme_extensions,
            scan_snippet_extensions,
            search_in_folder,
            searchdb::index_info,
            searchdb::index_files,
            searchdb::index_symbols,
            searchdb::index_warm,
            searchdb::index_rebuild,
            searchdb::index_invalidate,
            pty_spawn,
            pty_write,
            pty_resize,
            pty_kill,
            save_secret,
            has_secret,
            clear_secret,
            chat_stream,
            load_chats,
            save_chats,
            git_status,
            git_show_head,
            lsp_servers,
            lsp_start,
            lsp_send,
            lsp_stop,
            lsp_stop_all,
            dap_adapters,
            dap_start,
            dap_attach,
            dap_send,
            dap_stop,
            dap_stop_all
        ])
        // 终端会话表。放在 State 里而不是全局变量 —— Tauri 会管它的生命周期
        .manage(PtyState::default())
        // 语言服务器 / 调试适配器的会话表。同样的理由
        .manage(LspState::default())
        .manage(DapState::default())
        // 搜索索引（每个工作区一个 SQLite 库）
        .manage(searchdb::IndexStore::default())
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
