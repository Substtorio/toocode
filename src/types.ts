//定义文件夹和文件类型，用|隔开表示二选一，
export interface FileNode {
    name: string;
    path: string;//从工作区根算起的完整路径，作为唯一标识（给 key 和标签页用）
    type: "file" | "folder";//联合类型
    children?: FileNode[];//文件夹用的字段，?表示可选，文件无子节点，不用写字段
}