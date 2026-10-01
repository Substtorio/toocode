use base64::Engine as _;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
// ⚠ `Child` 这个名字已经被 portable_pty 占了（终端那块的），
//   所以 std 的这个显式改名成 StdChild —— 两边混用会编不过，但报错信息不好懂
use std::process::{
    Child as StdChild, ChildStderr, ChildStdin, ChildStdout, Command, Stdio,
};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, State};

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

/// 跑起来的会话
struct LspSession {
    /// 留着是为了 stop 时能 kill —— 不存的话进程就没人管了
    child: StdChild,
    /// ★ 包成 Arc<Mutex<…>> 是为了「拿得到就行」：
    ///   `lsp_send` 不必在整个写操作期间占着 sessions 那把大锁
    stdin: Arc<Mutex<ChildStdin>>,
}

#[derive(Default)]
struct LspState {
    sessions: Mutex<HashMap<String, LspSession>>,
}

/// 发给前端的每条消息
#[derive(Serialize, Clone)]
struct LspMessage {
    session: String,
    /// 原始 JSON 文本。★ Rust 不解析它 —— 解析留前端，
    /// 这样改 LSP 版本 / 加方法都不用重编 Rust
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

/// 起一个语言服务器，返回会话 id
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

    let mut command = Command::new(&info.program);
    command.args(&info.args);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    // cwd 只在目录真的存在时才设 —— 设一个不存在的路径会让 spawn 整个失败
    //（终端那块踩过同一个坑）
    if let Some(dir) = cwd.as_ref().filter(|d| Path::new(d).is_dir()) {
        command.current_dir(dir);
    }

    // ⚠ Windows 上不给这个的话会闪一个黑框（和 git 命令同一个坑）
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = command
        .spawn()
        .map_err(|error| format!("启动语言服务器失败：{error}"))?;

    let stdin = child.stdin.take().ok_or("拿不到语言服务器的 stdin")?;
    let stdout = child.stdout.take().ok_or("拿不到语言服务器的 stdout")?;
    let stderr = child.stderr.take().ok_or("拿不到语言服务器的 stderr")?;

    spawn_lsp_reader(app.clone(), session.clone(), stdout);
    spawn_lsp_stderr(app, session.clone(), stderr);

    state.sessions.lock().unwrap().insert(
        session,
        LspSession {
            child,
            stdin: Arc::new(Mutex::new(stdin)),
        },
    );

    Ok(())
}

/// 读服务器的输出，一帧一条事件发给前端
fn spawn_lsp_reader(app: AppHandle, session: String, stdout: ChildStdout) {
    // ★ 必须开独立线程：`read` 是阻塞的，挂在命令里会把命令线程整个卡死
    //   （终端的 pty 那块踩过同一个坑）
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_frame(&mut reader) {
                Ok(Some(body)) => {
                    let _ = app.emit(
                        "lsp-message",
                        LspMessage {
                            session: session.clone(),
                            body,
                        },
                    );
                }
                // EOF = 服务器退出了，正常结束
                Ok(None) => break,
                Err(error) => {
                    let _ = app.emit("lsp-error", format!("{session}: {error}"));
                    break;
                }
            }
        }
        // 不管怎么结束都通知一声 —— 前端要拿它把「已连接」状态收回去
        let _ = app.emit("lsp-exit", session);
    });
}

/// 服务器的 stderr 也得有人读。
///
/// ⚠ 不读的话管道缓冲区会满，服务器一写日志就**卡死在那儿** ——
///   表现是「服务器起来了一会儿就没反应了」，而 stdout 那边什么都看不到
fn spawn_lsp_stderr(app: AppHandle, session: String, stderr: ChildStderr) {
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            let _ = app.emit("lsp-stderr", format!("[{session}] {line}"));
        }
    });
}

/// 往前端指定的会话发一帧
#[tauri::command]
async fn lsp_send(state: State<'_, LspState>, session: String, message: String) -> Result<(), String> {
    // ★ 先把 stdin 的 Arc 取出来、把 sessions 那把锁放掉，再去写 ——
    //   写管道是会阻塞的，握着大锁写会把别的会话一起堵住
    let stdin = {
        let sessions = state.sessions.lock().unwrap();
        let found = sessions
            .get(&session)
            .ok_or_else(|| format!("没有这个会话：{session}"))?;
        Arc::clone(&found.stdin)
    };

    let mut stdin = stdin.lock().unwrap();
    // ⚠ `&mut *stdin` 而不是 `&mut stdin`：MutexGuard 本身不是 Write，
    //   要显式解引用成里面的 ChildStdin
    write_frame(&mut *stdin, &message).map_err(|error| format!("写给语言服务器失败：{error}"))?;
    Ok(())
}

/// 关掉一个会话（同时把进程杀掉）
#[tauri::command]
async fn lsp_stop(state: State<'_, LspState>, session: String) -> Result<(), String> {
    let mut sessions = state.sessions.lock().unwrap();
    if let Some(mut found) = sessions.remove(&session) {
        // kill 失败不是错误 —— 进程可能已经自己退了
        let _ = found.child.kill();
        let _ = found.child.wait();
    }
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
    let roots = extension_roots();
    let mut entries: Vec<SnippetEntry> = Vec::new();

    // ★ 先到先得（和语法那套一致）：扫描顺序 = 目录优先级（用户扩展 > 内置），
    //   所以用户装的扩展能盖住内置的。
    //   判重的 key 是「语言 + 文件路径」—— 同一份文件可以服务多个语言，
    //   但同一个语言的同一份文件只登记一次
    let mut seen = HashSet::new();

    for_each_manifest(&roots, |dir, manifest| {
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

            // ⚠ 判重的 key 用**元组**，不要拿一个字符拼字符串 ——
            //   拼的话就得挑一个「路径里绝不会出现」的分隔符，
            //   而那种魔术字符正是以后会被忘掉、然后出 bug 的东西
            let normalized = path.to_string_lossy().replace('\\', "/");
            let wanted: Vec<String> = languages
                .into_vec()
                .into_iter()
                .filter(|id| seen.insert((id.clone(), normalized.clone())))
                .collect();

            if wanted.is_empty() {
                continue;
            }

            entries.push(SnippetEntry {
                languages: wanted,
                path: normalized,
                source: dir
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default(),
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

    Ok(entries)
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
        reqwest::Client::builder()
            // 模型「思考 + 写代码」可能要很久，超时给宽一点；
            // 但**不能不给** —— 没有超时的话网络一断这个命令就永远挂着
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("构造 HTTP 客户端不该失败")
    })
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
                    error: Some(format!("请求失败：{err}")),
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
                        error: Some(format!("读取响应中断：{err}")),
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

/// 把所有值得搜的文件收集起来。和 `walk()` 共用 IGNORED_DIRS，但不设业务深度上限
fn collect_search_files(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
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
            out.push(path);
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
#[tauri::command]
async fn search_in_folder(
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
    tauri::async_runtime::spawn_blocking(move || {
        let mut files = Vec::new();
        collect_search_files(Path::new(&path), &mut files, 0);

        let mut matches: Vec<SearchMatch> = Vec::new();
        let mut truncated = false;
        let mut files_scanned = 0usize;

        for file in &files {
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
            pty_spawn,
            pty_write,
            pty_resize,
            pty_kill,
            save_secret,
            has_secret,
            clear_secret,
            chat_stream,
            git_status,
            git_show_head,
            lsp_servers,
            lsp_start,
            lsp_send,
            lsp_stop
        ])
        // 终端会话表。放在 State 里而不是全局变量 —— Tauri 会管它的生命周期
        .manage(PtyState::default())
        // 语言服务器会话表。同样的理由
        .manage(LspState::default())
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
