//定义文件夹和文件类型，用|隔开表示二选一，
export interface FileNode {
    name: string;
    type: "file" | "folder";//联合类型
    children?: FileNode[];//文件夹用的字段，?表示可选，文件无子节点，不用写字段
    content?: string;//文件用的字段，文件内容
}

//定义编辑器标签页的内容
export interface Tab {
    path: string;//文件路径，作为唯一标识
    name: string;//显示名
    content: string;//当前内容
}
