use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    /// 这个语法服务于哪个语言 id。没写就跳过 —— 我们不知道把它挂在谁头上
    language: Option<String>,
    #[serde(rename = "scopeName")]
    scope_name: String,
    path: String,
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
            });
        }
    });

    // 终端里能看见，出问题好排查（Tauri dev 会把 stdout 打出来）
    println!("[语法扩展] 候选扩展根 {} 个：", roots.len());
    for root in &roots {
        println!("[语法扩展]   {}", root.display());
    }
    println!(
        "[语法扩展] 得到 {} 个语法，其中 {} 个挂了具体语言",
        entries.len(),
        entries.iter().filter(|entry| entry.id.is_some()).count()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 自检：VS Code 程序目录里的内置扩展能不能被找到。
    ///
    /// 那是 html / css / typescript 这些**父语法**的家 —— 找不到它们，
    /// Vue 这类「寄生语法」就会缺一大块高亮。
    ///
    /// 跑：cargo test -- --nocapture
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
            scan_theme_extensions
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
